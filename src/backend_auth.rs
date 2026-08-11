use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::Instant;

use rand::distributions::Alphanumeric;
use rand::Rng;
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AuthFlow {
    #[serde(rename = "env")]
    Env { variable: String },
    #[serde(rename = "oauth2")]
    OAuth2 {
        authorize_url: String,
        token_url: String,
        #[serde(default)]
        refresh_url: Option<String>,
        client_id: String,
        #[serde(default)]
        scopes: Vec<String>,
        response_field: String,
        #[serde(default)]
        expires_in_field: Option<String>,
    },
    #[serde(rename = "custom-token-exchange")]
    CustomTokenExchange {
        login_url: String,
        #[serde(default)]
        manual_url: Option<String>,
        exchange_url: String,
        exchange_body_template: String,
        refresh_token_field: String,
        refresh_url: String,
        refresh_body_template: String,
        id_token_field: String,
        #[serde(default)]
        expires_in_field: Option<String>,
    },
}

pub struct InitialAuth {
    pub token: String,
    pub expires_at: Option<Instant>,
    pub refresh_credential: Option<String>,
}

pub fn authenticate(config: &AuthFlow, host: &str, status_fd: &mut File) -> Result<InitialAuth, String> {
    match config {
        AuthFlow::Env { variable } => authenticate_env(variable),
        AuthFlow::OAuth2 { authorize_url, token_url, refresh_url, client_id, scopes, response_field, expires_in_field } => authenticate_oauth2(
            host,
            authorize_url,
            token_url,
            refresh_url.as_deref(),
            client_id,
            scopes,
            response_field,
            expires_in_field.as_deref(),
            status_fd,
        ),
        AuthFlow::CustomTokenExchange {
            login_url,
            manual_url,
            exchange_url,
            exchange_body_template,
            refresh_token_field,
            refresh_url,
            refresh_body_template,
            id_token_field,
            expires_in_field,
        } => authenticate_custom_token(
            host,
            login_url,
            manual_url.as_deref(),
            exchange_url,
            exchange_body_template,
            refresh_token_field,
            refresh_url,
            refresh_body_template,
            id_token_field,
            expires_in_field.as_deref(),
            status_fd,
        ),
    }
}

pub fn refresh(config: &AuthFlow, host: &str, refresh_credential: &str) -> Result<InitialAuth, String> {
    match config {
        AuthFlow::Env { variable } => authenticate_env(variable),
        AuthFlow::OAuth2 { token_url, refresh_url: Some(refresh_url), client_id, response_field, expires_in_field, .. } => {
            refresh_oauth2(host, refresh_url, token_url, client_id, refresh_credential, response_field, expires_in_field.as_deref())
        }
        AuthFlow::CustomTokenExchange { refresh_url, refresh_body_template, id_token_field, expires_in_field, .. } => {
            refresh_custom_token(host, refresh_url, refresh_body_template, refresh_credential, id_token_field, expires_in_field.as_deref())
        }
        _ => Err("refresh not supported for this auth type".into()),
    }
}

fn authenticate_env(variable: &str) -> Result<InitialAuth, String> {
    let token = std::env::var(variable).map_err(|_| format!("env var {variable} not set"))?;
    if token.is_empty() {
        return Err(format!("env var {variable} is empty"));
    }
    Ok(InitialAuth { token, expires_at: None, refresh_credential: None })
}

fn authenticate_oauth2(
    host: &str, authorize_url_template: &str, token_url: &str, refresh_url: Option<&str>, client_id: &str, scopes: &[String], response_field: &str,
    expires_in_field: Option<&str>, status_fd: &mut File,
) -> Result<InitialAuth, String> {
    let code_verifier = random_string(64);
    let code_challenge = base64_url_no_pad(sha256(code_verifier.as_bytes()));

    let (callback_port, code) = start_callback(status_fd, "code")?;
    let redirect_uri = format!("http://localhost:{callback_port}/authcallback");

    let authorize_url = resolve_template(authorize_url_template, host, &redirect_uri, "", "");
    let mut url = format!(
        "{authorize_url}?response_type=code&client_id={client_id}&redirect_uri={redirect_uri}&code_challenge={code_challenge}&code_challenge_method=S256"
    );
    if !scopes.is_empty() {
        url.push_str(&format!("&scope={}", scopes.join(" ")));
    }

    status_line(status_fd, "Opening browser for login...");
    webbrowser::open(&url).map_err(|e| format!("browser: {e}"))?;
    let code: Result<String, String> =
        code.recv_timeout(std::time::Duration::from_secs(300)).map_err(|_: std::sync::mpsc::RecvTimeoutError| "login timed out".to_string())?;
    let code = code?;
    status_line(status_fd, "Exchanging authorization code...");

    let token_url = resolve_template(token_url, host, "", "", "");
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": redirect_uri,
        "client_id": client_id,
        "code_verifier": code_verifier,
    });
    let response = post_json(&token_url, &body.to_string(), 30)?;

    let token = extract_string(&response, response_field)?;
    let expires_in = expires_in_field.and_then(|f| extract_u64(&response, f).ok());
    let refresh_token = refresh_url.and_then(|_| extract_string(&response, "refresh_token").ok());

    Ok(InitialAuth { token, expires_at: expires_in.map(|s| Instant::now() + std::time::Duration::from_secs(s)), refresh_credential: refresh_token })
}

fn refresh_oauth2(
    host: &str, refresh_url: &str, _token_url: &str, client_id: &str, refresh_token_val: &str, response_field: &str, expires_in_field: Option<&str>,
) -> Result<InitialAuth, String> {
    let url = resolve_template(refresh_url, host, "", "", "");
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token_val,
        "client_id": client_id,
    });
    let response = post_json(&url, &body.to_string(), 30)?;

    let token = extract_string(&response, response_field)?;
    let expires_in = expires_in_field.and_then(|f| extract_u64(&response, f).ok());
    let new_refresh = extract_string(&response, "refresh_token").ok();

    Ok(InitialAuth {
        token,
        expires_at: expires_in.map(|s| Instant::now() + std::time::Duration::from_secs(s)),
        refresh_credential: new_refresh.or_else(|| Some(refresh_token_val.into())),
    })
}

fn authenticate_custom_token(
    host: &str, login_url_template: &str, manual_url: Option<&str>, exchange_url: &str, exchange_body_template: &str, refresh_token_field_val: &str,
    refresh_url_str: &str, refresh_body_template: &str, id_token_field: &str, expires_in_field: Option<&str>, status_fd: &mut File,
) -> Result<InitialAuth, String> {
    let (callback_port, custom_token) = start_callback(status_fd, "custom_token")?;
    let redirect_uri = format!("http://127.0.0.1:{callback_port}/callback");

    let login_url = resolve_template(login_url_template, host, &redirect_uri, "", "");
    status_line(status_fd, "Opening browser for login...");
    webbrowser::open(&login_url).map_err(|e| format!("browser: {e}"))?;

    let custom_token: Result<String, String> = match custom_token.recv_timeout(std::time::Duration::from_secs(300)) {
        Ok(inner) => inner,
        Err(_) => {
            if let Some(manual) = manual_url {
                status_line(status_fd, "Browser callback not arrived. Opening manual login...");
                let murl = resolve_template(manual, host, "", "", "");
                webbrowser::open(&murl).map_err(|e| format!("browser: {e}"))?;
                Ok(read_token_from_stdin("Paste the custom token: ")?)
            } else {
                Err("login timed out".into())
            }
        }
    };
    let custom_token = custom_token?;

    status_line(status_fd, "Exchanging custom token...");
    let exchange_body = resolve_template(exchange_body_template, host, "", &custom_token, "");
    let exchange_resp = post_json(&resolve_template(exchange_url, host, "", "", ""), &exchange_body, 30)?;

    let refresh_token = extract_string(&exchange_resp, refresh_token_field_val)?;
    status_line(status_fd, "Obtaining access token...");

    let refresh_body = resolve_template(refresh_body_template, host, "", "", &refresh_token);
    let refresh_resp = post_json(&resolve_template(refresh_url_str, host, "", "", ""), &refresh_body, 30)?;

    let id_token = extract_string(&refresh_resp, id_token_field)?;
    let expires_in = expires_in_field.and_then(|f| extract_u64(&refresh_resp, f).ok());

    Ok(InitialAuth {
        token: id_token,
        expires_at: expires_in.map(|s| Instant::now() + std::time::Duration::from_secs(s)),
        refresh_credential: Some(refresh_token),
    })
}

fn refresh_custom_token(
    host: &str, refresh_url: &str, refresh_body_template: &str, refresh_credential: &str, id_token_field: &str, expires_in_field: Option<&str>,
) -> Result<InitialAuth, String> {
    let url = resolve_template(refresh_url, host, "", "", "");
    let body = resolve_template(refresh_body_template, host, "", "", refresh_credential);
    let response = post_json(&url, &body, 30)?;

    let token = extract_string(&response, id_token_field)?;
    let expires_in = expires_in_field.and_then(|f| extract_u64(&response, f).ok());

    Ok(InitialAuth {
        token,
        expires_at: expires_in.map(|s| Instant::now() + std::time::Duration::from_secs(s)),
        refresh_credential: Some(refresh_credential.into()),
    })
}

fn start_callback(_status_fd: &mut File, param: &str) -> Result<(u16, std::sync::mpsc::Receiver<Result<String, String>>), String> {
    let param = param.to_string();
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("callback bind: {e}"))?;
    let port = listener.local_addr().map_err(|e| format!("addr: {e}"))?.port();
    let (tx, rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        let mut incoming = listener.incoming();
        let mut stream = match incoming.next() {
            Some(Ok(s)) => s,
            _ => {
                let _ = tx.send(Err("callback connection failed".into()));
                return;
            }
        };

        let mut reader = BufReader::new(&mut stream);
        let mut first_line = String::new();
        if reader.read_line(&mut first_line).is_err() {
            let _ = tx.send(Err("failed to read callback request".into()));
            return;
        }

        let path = first_line.split_whitespace().nth(1).unwrap_or("/");
        let query = path.split('?').nth(1).unwrap_or("");
        let val = parse_query_param(query, &param);
        let body = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 64\r\n\r\nAuthentication complete. Close this tab.";
        let _ = stream.write_all(body);
        let _ = tx.send(val.ok_or_else(|| format!("missing {param} in callback")).map(String::from));
    });

    Ok((port, rx))
}

fn read_token_from_stdin(prompt: &str) -> Result<String, String> {
    use std::io::{self, Write};
    let mut stdout = io::stdout();
    let _ = stdout.write_all(prompt.as_bytes());
    let _ = stdout.flush();
    let mut input = String::new();
    io::stdin().read_line(&mut input).map_err(|e| format!("read: {e}"))?;
    let token = input.trim().to_string();
    if token.is_empty() {
        Err("token required".into())
    } else {
        Ok(token)
    }
}

fn parse_query_param(query: &str, param: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if parts.next() == Some(param) {
            return parts.next().map(|v| url_decode(v));
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        match b {
            b'%' => {
                let hi = chars.next().and_then(hex_val);
                let lo = chars.next().and_then(hex_val);
                if let (Some(h), Some(l)) = (hi, lo) {
                    result.push((h << 4 | l) as char);
                }
            }
            b'+' => result.push(' '),
            _ => result.push(b as char),
        }
    }
    result
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn status_line(status_fd: &mut File, message: &str) {
    let line = message.lines().next().unwrap_or(message).to_string();
    let _ = status_fd.write_all(line.as_bytes());
    let _ = status_fd.write_all(b"\n");
}

fn resolve_template(template: &str, host: &str, callback_url: &str, custom_token: &str, refresh_token: &str) -> String {
    template
        .replace("{host}", host)
        .replace("{callback_url}", callback_url)
        .replace("{custom_token}", custom_token)
        .replace("{refresh_token}", refresh_token)
}

fn post_json(url: &str, body: &str, timeout_secs: u32) -> Result<serde_json::Value, String> {
    let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
    let agent: ureq::Agent =
        ureq::Agent::config_builder().tls_config(tls).timeout_global(Some(std::time::Duration::from_secs(timeout_secs as u64))).build().into();
    let resp = agent
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send(body)
        .map_err(|e| format!("POST {url}: {e}"))?;

    let status = resp.status();
    let text = resp.into_body().read_to_string().map_err(|e| format!("read: {e}"))?;

    if !(200..300).contains(&status.as_u16()) {
        return Err(format!("POST {url} returned HTTP {status}"));
    }

    serde_json::from_str(&text).map_err(|e| format!("parse JSON: {e}"))
}

fn extract_string(value: &serde_json::Value, field: &str) -> Result<String, String> {
    match dot_get(value, field) {
        serde_json::Value::String(s) if !s.is_empty() => Ok(s.clone()),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        _ => Err(format!("field '{field}' missing or empty")),
    }
}

fn extract_u64(value: &serde_json::Value, field: &str) -> Result<u64, String> {
    match dot_get(value, field) {
        serde_json::Value::Number(n) => n.as_u64().ok_or_else(|| format!("field '{field}' not integer")),
        serde_json::Value::String(s) => s.parse::<u64>().map_err(|_| format!("field '{field}' not integer string")),
        _ => Err(format!("field '{field}' not found")),
    }
}

fn dot_get<'a>(value: &'a serde_json::Value, path: &str) -> &'a serde_json::Value {
    let mut current = value;
    for segment in path.split('.') {
        current = match current {
            serde_json::Value::Object(map) => map.get(segment).unwrap_or(&serde_json::Value::Null),
            _ => return &serde_json::Value::Null,
        };
    }
    current
}

fn random_string(len: usize) -> String {
    rand::thread_rng().sample_iter(&Alphanumeric).take(len).map(char::from).collect()
}

fn base64_url_no_pad(bytes: impl AsRef<[u8]>) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    URL_SAFE_NO_PAD.encode(bytes.as_ref())
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().to_vec()
}

#[cfg(test)]
#[path = "backend_auth_ut.rs"]
mod backend_auth_tests;
