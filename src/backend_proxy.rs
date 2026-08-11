use std::fs::File;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use actix_web::{web, HttpRequest, HttpResponse};
use awc::Client;
use indexmap::IndexMap;

use crate::backend_auth::{self, AuthFlow};

pub struct ProxyState {
    pub host: String,
    pub jwt: Arc<RwLock<Option<CachedToken>>>,
    pub extra_headers: IndexMap<String, String>,
    pub prompt_id: String,
    pub log: Option<Arc<Mutex<File>>>,
}

#[derive(Clone)]
pub struct CachedToken {
    pub token: String,
    pub expires_at: Instant,
}

pub fn spawn_refresh_loop(
    jwt: Arc<RwLock<Option<CachedToken>>>,
    auth: Arc<AuthFlow>,
    host: String,
    refresh_credential: Option<String>,
) {
    let Some(refresh_credential) = refresh_credential else { return };

    actix_rt::spawn(async move {
        loop {
            let sleep_dur = {
                let guard = jwt.read().unwrap();
                guard.as_ref().map(|t| {
                    let remaining = t.expires_at.saturating_duration_since(Instant::now());
                    remaining / 2
                })
            };

            match sleep_dur {
                Some(dur) if dur > std::time::Duration::from_secs(30) => {
                    let secs = dur.as_secs().min(300);
                    actix_rt::time::sleep(std::time::Duration::from_secs(secs)).await;
                }
                _ => {
                    match backend_auth::refresh(&auth, &host, &refresh_credential) {
                        Ok(a) => {
                            *jwt.write().unwrap() = Some(CachedToken {
                                token: a.token,
                                expires_at: a.expires_at.unwrap_or_else(|| Instant::now() + std::time::Duration::from_secs(3600)),
                            });
                        }
                        Err(e) => eprintln!("backend: token refresh failed: {e}"),
                    }
                    actix_rt::time::sleep(std::time::Duration::from_secs(30)).await;
                }
            }
        }
    });
}

pub async fn get_token(state: web::Data<ProxyState>) -> HttpResponse {
    let guard = state.jwt.read().unwrap();
    match guard.as_ref() {
        Some(t) => {
            let remaining = t.expires_at.saturating_duration_since(Instant::now());
            HttpResponse::Ok().json(serde_json::json!({
                "token": t.token,
                "expires_in_secs": remaining.as_secs()
            }))
        }
        None => HttpResponse::ServiceUnavailable().json(serde_json::json!({"error": "token not yet available"})),
    }
}

pub async fn proxy_models(state: web::Data<ProxyState>, _req: HttpRequest) -> HttpResponse {
    let token = current_token(&state);
    let client = Client::default();
    let target = format!("{}/chat/v2/models", state.host);

    proxy_log(&state, &format!("GET {target}"));

    let mut req = client
        .get(&target)
        .insert_header(("Authorization", format!("Bearer {token}")))
        .insert_header(("cloud-sla", "onprem"))
        .insert_header(("Accept", "application/json"));

    for (name, value) in &state.extra_headers {
        req = req.insert_header((name.as_str(), value.as_str()));
    }

    match req.send().await {
        Ok(mut upstream) => {
            let status = upstream.status();
            match upstream.body().limit(8_000_000).await {
                Ok(bytes) => {
                    proxy_log(&state, &format!("→ {status} ({} bytes)", bytes.len()));
                    let translated = translate_tabnine_models(&bytes);
                    HttpResponse::build(status).body(translated)
                }
                Err(e) => {
                    proxy_log(&state, &format!("→ body read error: {e}"));
                    HttpResponse::BadGateway().body(e.to_string())
                }
            }
        }
        Err(e) => {
            proxy_log(&state, &format!("→ ERROR: {e}"));
            HttpResponse::BadGateway().body(e.to_string())
        }
    }
}

fn translate_tabnine_models(body: &[u8]) -> Vec<u8> {
    let Ok(tabnine) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.to_vec();
    };

    let Some(models) = tabnine.get("models").and_then(|m| m.as_array()) else {
        return body.to_vec();
    };

    let data: Vec<serde_json::Value> = models
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?;
            let name = m.get("name").and_then(|n| n.as_str()).unwrap_or(id);
            Some(serde_json::json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": "tabnine",
                "name": name
            }))
        })
        .collect();

    let openai = serde_json::json!({
        "object": "list",
        "data": data
    });

    openai.to_string().into_bytes()
}

pub async fn proxy_chat_completions(state: web::Data<ProxyState>, body: web::Bytes) -> HttpResponse {
    proxy_upstream(&state, "/chat/openai/v1/chat/completions", body).await
}

async fn proxy_upstream(state: &web::Data<ProxyState>, path: &str, body: web::Bytes) -> HttpResponse {
    let token = current_token(state);
    let client = Client::default();
    let target = format!("{}{path}", state.host);

    proxy_log(state, &format!("POST {target}"));

    let mut req = client
        .post(&target)
        .insert_header(("Authorization", format!("Bearer {token}")))
        .insert_header(("Content-Type", "application/json"))
        .insert_header(("prompt-id", state.prompt_id.as_str()))
        .insert_header(("x-agent-type", "cli"));

    for (name, value) in &state.extra_headers {
        req = req.insert_header((name.as_str(), value.as_str()));
    }

    match req.send_body(body.to_vec()).await {
        Ok(mut upstream) => {
            let status = upstream.status();
            let mut resp = HttpResponse::build(status);
            for (key, val) in upstream.headers() {
                if !key.as_str().eq_ignore_ascii_case("transfer-encoding") {
                    resp.insert_header((key.clone(), val.clone()));
                }
            }
            match upstream.body().limit(128_000_000).await {
                Ok(bytes) => {
                    proxy_log(state, &format!("→ {status} ({} bytes)", bytes.len()));
                    resp.body(bytes)
                }
                Err(e) => {
                    proxy_log(state, &format!("→ body read error: {e}"));
                    HttpResponse::BadGateway().body(e.to_string())
                }
            }
        }
        Err(e) => {
            proxy_log(state, &format!("→ ERROR: {e}"));
            HttpResponse::BadGateway().body(e.to_string())
        }
    }
}

fn proxy_log(state: &web::Data<ProxyState>, msg: &str) {
    if let Some(ref log) = state.log {
        use std::io::Write;
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        let line = format!("[{ts}] {msg}\n");
        let _ = log.lock().unwrap().write_all(line.as_bytes());
    }
}

fn current_token(state: &web::Data<ProxyState>) -> String {
    state.jwt.read().unwrap().as_ref().map(|t| t.token.clone()).unwrap_or_default()
}
