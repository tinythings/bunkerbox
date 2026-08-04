use crate::logging;
use std::net::SocketAddr;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixListener};

pub const PORT: u16 = 20000;
const BIND_ADDR: &str = "127.0.0.1";

pub struct UnixProxyHandle {
    task: tokio::task::JoinHandle<()>,
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl UnixProxyHandle {
    pub fn stop(self) {
        self.task.abort();
        self.remove_if_unchanged();
    }

    fn remove_if_unchanged(&self) {
        if let Ok(meta) = std::fs::symlink_metadata(&self.path) {
            if meta.dev() == self.dev && meta.ino() == self.ino && meta.file_type().is_socket() {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

impl Drop for UnixProxyHandle {
    fn drop(&mut self) {
        self.task.abort();
        self.remove_if_unchanged();
    }
}

pub struct FilterProxy {
    allow: Vec<String>,
}

impl FilterProxy {
    pub fn new(allow: Vec<String>) -> Self {
        Self { allow }
    }

    pub async fn bind(self) -> Result<tokio::task::JoinHandle<()>, String> {
        let (handle, _) = self.bind_on(PORT).await?;
        Ok(handle)
    }

    pub async fn bind_on(self, port: u16) -> Result<(tokio::task::JoinHandle<()>, u16), String> {
        let addr: SocketAddr = format!("{BIND_ADDR}:{port}").parse().map_err(|e| format!("invalid proxy bind address: {e}"))?;

        let listener = TcpListener::bind(addr).await.map_err(|e| format!("failed to bind proxy on {addr}: {e}"))?;
        let bound_port = listener.local_addr().map_err(|e| format!("get local addr: {e}"))?.port();

        let allow = self.allow;

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => {
                        let allow = allow.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(stream, &allow).await {
                                logging::diagnostic(&format!("bunkerbox-proxy: client failed: {err}"));
                            }
                        });
                    }
                    Err(e) => {
                        logging::diagnostic(&format!("bunkerbox-proxy: accept error: {e}"));
                    }
                }
            }
        });

        Ok((handle, bound_port))
    }

    pub async fn bind_unix(self, path: impl AsRef<Path>) -> Result<UnixProxyHandle, String> {
        let path = path.as_ref().to_path_buf();

        let listener = UnixListener::bind(&path).map_err(|e| format!("failed to bind proxy on {}: {e}", path.display()))?;

        let meta = std::fs::symlink_metadata(&path).map_err(|e| format!("failed to stat socket {}: {e}", path.display()))?;

        let allow = self.allow;

        let task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => {
                        let allow = allow.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(stream, &allow).await {
                                logging::diagnostic(&format!("bunkerbox-proxy: client failed: {err}"));
                            }
                        });
                    }
                    Err(e) => {
                        logging::diagnostic(&format!("bunkerbox-proxy: accept error: {e}"));
                    }
                }
            }
        });

        Ok(UnixProxyHandle { task, path, dev: meta.dev(), ino: meta.ino() })
    }
}

async fn handle_client<C>(mut client: C, allow: &[String]) -> Result<(), String>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = [0u8; 8192];
    let n = client.read(&mut buf).await.map_err(|e| format!("read request: {e}"))?;

    if n == 0 {
        return Ok(());
    }

    let request = String::from_utf8_lossy(&buf[..n]);
    let first_line = request.lines().next().unwrap_or("");

    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 {
        return Err(format!("malformed request: {first_line}"));
    }

    let method = parts[0];
    let target = parts[1];

    let (host, port, is_connect) = if method.eq_ignore_ascii_case("CONNECT") {
        let (h, p) = parse_host_port(target)?;
        (h, p, true)
    } else if target.starts_with("http://") {
        let url = target.strip_prefix("http://").ok_or_else(|| format!("malformed URL: {target}"))?;
        let (h, p) = url.split_once('/').map_or_else(|| (url, "80"), |(hp, _)| hp.split_once(':').unwrap_or((hp, "80")));
        (h.to_string(), p.to_string(), false)
    } else {
        return Err(format!("unsupported request: {first_line}"));
    };

    if !is_allowed(&host, allow) {
        let forbidden = b"HTTP/1.1 403 Forbidden\r\n\r\n";
        let _ = client.write_all(forbidden).await;
        return Err(format!("blocked: {host}"));
    }

    let upstream_addr = format!("{host}:{port}");
    let mut upstream = TcpStream::connect(&upstream_addr).await.map_err(|e| format!("connect to {upstream_addr}: {e}"))?;

    if is_connect {
        let established = b"HTTP/1.1 200 Connection Established\r\n\r\n";
        client.write_all(established).await.map_err(|e| format!("write 200: {e}"))?;
    } else {
        upstream.write_all(&buf[..n]).await.map_err(|e| format!("write upstream: {e}"))?;
    }

    tokio::io::copy_bidirectional(&mut client, &mut upstream).await.map(|_| ()).map_err(|e| format!("relay error: {e}"))
}

fn parse_host_port(target: &str) -> Result<(String, String), String> {
    if let Some((host, port)) = target.rsplit_once(':') {
        if port.chars().all(|c| c.is_ascii_digit()) {
            return Ok((host.to_string(), port.to_string()));
        }
    }
    Ok((target.to_string(), "443".to_string()))
}

fn is_allowed(host: &str, allow: &[String]) -> bool {
    let host_lower = host.to_lowercase();
    allow.iter().any(|entry| {
        let entry_lower = entry.to_lowercase();
        host_lower == entry_lower || host_lower.ends_with(&format!(".{entry_lower}"))
    })
}

#[cfg(test)]
#[path = "proxy_ut.rs"]
mod proxy_tests;
