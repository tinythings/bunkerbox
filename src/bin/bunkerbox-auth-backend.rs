#[path = "../backend_auth.rs"]
mod backend_auth;
#[path = "../backend_proxy.rs"]
mod backend_proxy;

use std::fs;
use std::fs::File;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::sync::{Arc, Mutex, RwLock};

use actix_web::{web, App, HttpServer};

use backend_auth::AuthFlow;
use backend_proxy::{spawn_refresh_loop, CachedToken, ProxyState};

#[derive(Debug, serde::Deserialize)]
struct SidecarConfig {
    #[serde(flatten)]
    backend: InnerConfig,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum InnerConfig {
    #[serde(rename = "declarative")]
    Declarative(Box<DeclarativeCfg>),
    #[serde(rename = "plugin")]
    #[allow(dead_code)]
    Plugin { name: String, config: serde_json::Value },
}

#[derive(Debug, serde::Deserialize)]
struct DeclarativeCfg {
    host: String,
    #[serde(default)]
    request_headers: indexmap::IndexMap<String, String>,
    auth: AuthFlow,
}

#[actix_web::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("bunkerbox-auth-backend: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) != Some("start") {
        eprintln!("usage: bunkerbox-auth-backend start --config FILE --status-fd FD");
        return Ok(());
    }

    let mut config_path: Option<String> = None;
    let mut status_fd_raw: Option<i32> = None;
    let mut log_path: Option<String> = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                config_path = args.get(i + 1).cloned();
                i += 2;
            }
            "--status-fd" => {
                status_fd_raw = args.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            "--log" => {
                log_path = args.get(i + 1).cloned();
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }

    let config_path = config_path.ok_or("--config required")?;
    let status_fd_raw = status_fd_raw.ok_or("--status-fd required")?;
    let mut status_fd = unsafe { File::from_raw_fd(status_fd_raw) };

    let config_json = fs::read_to_string(&config_path).map_err(|e| format!("read config: {e}"))?;
    let config: SidecarConfig = serde_json::from_str(&config_json).map_err(|e| format!("parse config: {e}"))?;

    match config.backend {
        InnerConfig::Declarative(decl) => run_declarative(*decl, &mut status_fd, log_path).await,
        InnerConfig::Plugin { name, .. } => Err(format!("plugin '{name}' run directly: bunkerbox-auth-backend-{name}")),
    }
}

async fn run_declarative(decl: DeclarativeCfg, status_fd: &mut File, log_path: Option<String>) -> Result<(), String> {
    let host = decl.host.trim_end_matches('/').to_string();

    let log_file = log_path
        .map(|p| std::fs::OpenOptions::new().create(true).append(true).open(&p).map_err(|e| format!("open log {p}: {e}")))
        .transpose()?
        .map(|f| Arc::new(Mutex::new(f)));

    let initial = backend_auth::authenticate(&decl.auth, &host, status_fd)?;

    let jwt = Arc::new(RwLock::new(Some(CachedToken {
        token: initial.token,
        expires_at: initial.expires_at.unwrap_or_else(|| std::time::Instant::now() + std::time::Duration::from_secs(3600)),
    })));

    spawn_refresh_loop(jwt.clone(), Arc::new(decl.auth), host.clone(), initial.refresh_credential);

    let prompt_id = uuid::Uuid::new_v4().to_string();
    let state = web::Data::new(ProxyState { host, jwt, extra_headers: decl.request_headers, prompt_id, log: log_file });

    let server = HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .route("/token", web::get().to(backend_proxy::get_token))
            .route("/v1/models", web::get().to(backend_proxy::proxy_models))
            .route("/v1/chat/completions", web::post().to(backend_proxy::proxy_chat_completions))
    })
    .bind("0.0.0.0:0")
    .map_err(|e| format!("bind: {e}"))?;

    let port = server.addrs().first().map(|a| a.port()).ok_or("no bound port")?;

    writeln!(std::io::stdout(), r#"{{"port":{port}}}"#).map_err(|e| format!("write port: {e}"))?;
    std::io::stdout().flush().map_err(|e| format!("flush: {e}"))?;

    let _ = status_fd.write_all(b"Ready\n");
    let _ = status_fd.flush();

    server.run().await.map_err(|e| format!("server: {e}"))
}
