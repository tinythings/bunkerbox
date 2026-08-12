use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::cfg::{AuthBackendConfig, AuthRef};

pub struct AuthBackendHandle {
    child: Child,
    url: String,
}

impl AuthBackendHandle {
    pub fn start(auth_ref: &AuthRef, share_dir: &Path, status_fd: RawFd) -> Result<Self, String> {
        let config = match auth_ref {
            AuthRef::Inline(cfg) => (**cfg).clone(),
            AuthRef::Named(name) => load_named_config(name, share_dir)?,
        };

        let binary = match &config {
            AuthBackendConfig::Declarative(_) => find_builtin_auth(share_dir)?,
            AuthBackendConfig::Plugin(p) => discover_auth_plugin(&p.name, share_dir)?,
        };

        let config_json = config_to_json(&config)?;
        let config_path = write_temp_config(&config_json)?;

        let result = (|| -> Result<Self, String> {
            let mut child = Command::new(&binary)
                .arg("start")
                .arg("--config")
                .arg(&config_path)
                .arg("--status-fd")
                .arg(status_fd.to_string())
                .arg("--log")
                .arg(crate::logging::log_path())
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| format!("spawn {}: {e}", binary.display()))?;

            let port = read_auth_port(&mut child, Duration::from_secs(120))?;
            let url = format!("http://10.247.0.1:{port}");

            Ok(Self { child, url })
        })();

        let _ = fs::remove_file(&config_path);
        result
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn shutdown(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let _ = self.child.wait();
    }
}

impl Drop for AuthBackendHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn load_named_config(name: &str, share_dir: &Path) -> Result<AuthBackendConfig, String> {
    let path = share_dir.join(format!("{name}.conf"));
    let contents = fs::read_to_string(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    serde_yaml::from_str(&contents).map_err(|e| format!("failed to parse {}: {e}", path.display()))
}

fn find_builtin_auth(share_dir: &Path) -> Result<PathBuf, String> {
    let name = "bunkerbox-auth-backend";

    {
        let candidate = share_dir.join("bin").join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    let exe = std::env::current_exe().map_err(|e| format!("locate self: {e}"))?;

    if let Some(dir) = exe.parent() {
        {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    if let Some(path) = find_on_path(name) {
        return Ok(path);
    }

    Err("bunkerbox-auth-backend not found. Run: make dev".into())
}

fn discover_auth_plugin(name: &str, share_dir: &Path) -> Result<PathBuf, String> {
    let bin_name = format!("auth-backend-{name}");

    {
        let candidate = share_dir.join("bin").join(&bin_name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(&bin_name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    if let Some(path) = find_on_path(&bin_name) {
        return Ok(path);
    }

    Err(format!("auth plugin '{name}' not found ({bin_name} not on PATH). Run: make dev"))
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).find_map(|dir| {
            let candidate = dir.join(name);
            candidate.is_file().then_some(candidate)
        })
    })
}

fn config_to_json(config: &AuthBackendConfig) -> Result<String, String> {
    let value = match config {
        AuthBackendConfig::Declarative(d) => {
            let mut v = serde_json::to_value(d).map_err(|e| format!("serialize: {e}"))?;
            if let serde_json::Value::Object(ref mut map) = v {
                map.insert("type".into(), serde_json::Value::String("declarative".into()));
            }
            v
        }
        AuthBackendConfig::Plugin(p) => {
            let mut v = serde_json::to_value(p).map_err(|e| format!("serialize: {e}"))?;
            if let serde_json::Value::Object(ref mut map) = v {
                map.insert("type".into(), serde_json::Value::String("plugin".into()));
            }
            v
        }
    };
    serde_json::to_string(&value).map_err(|e| format!("serialize: {e}"))
}

fn write_temp_config(json: &str) -> Result<PathBuf, String> {
    let path = std::env::temp_dir().join(format!("bunkerbox-auth-{}.json", std::process::id()));
    fs::write(&path, json.as_bytes()).map_err(|e| format!("write temp config: {e}"))?;
    Ok(path)
}

fn read_auth_port(child: &mut Child, timeout: Duration) -> Result<u16, String> {
    let stdout = child.stdout.take().ok_or_else(|| "no stdout".to_string())?;
    let mut reader = BufReader::new(stdout);
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > timeout {
            let _ = child.kill();
            return Err("auth backend did not start within timeout".to_string());
        }

        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                let status = child.wait();
                let stderr = child
                    .stderr
                    .take()
                    .and_then(|mut s| {
                        use std::io::Read;
                        let mut buf = String::new();
                        s.read_to_string(&mut buf).ok().map(|_| buf.trim().to_string())
                    })
                    .unwrap_or_default();
                let detail = if stderr.is_empty() {
                    format!("exit code: {}", status.map(|s| s.code().unwrap_or(-1).to_string()).unwrap_or_else(|_| "unknown".into()))
                } else {
                    format!("stderr: {stderr}")
                };
                return Err(format!("auth backend exited before reporting port ({detail})"));
            }
            Ok(_) => {
                let line = line.trim();
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(line) {
                    if let Some(port) = parsed.get("port").and_then(|p| p.as_u64()) {
                        return Ok(port as u16);
                    }
                    if let Some(err) = parsed.get("error").and_then(|e| e.as_str()) {
                        let _ = child.kill();
                        return Err(format!("auth backend error: {err}"));
                    }
                }
            }
            Err(e) => {
                let _ = child.kill();
                return Err(format!("read auth backend stdout: {e}"));
            }
        }
    }
}

#[cfg(test)]
#[path = "auth_backend_ut.rs"]
mod auth_backend_tests;
