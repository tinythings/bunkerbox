use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const PORT: u16 = 20000;
const BIND_ADDR: &str = "127.0.0.1";

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
                            // TODO: route to log socket
                            let _ = handle_client(stream, &allow).await;
                        });
                    }
                    Err(e) => {
                        eprintln!("bunkerbox-proxy: accept error: {e}");
                    }
                }
            }
        });

        Ok((handle, bound_port))
    }
}

async fn handle_client(mut client: TcpStream, allow: &[String]) -> Result<(), String> {
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
        let (h, p) = url.split_once('/').map_or_else(|| (url, "80"), |(hp, _)| {
            hp.split_once(':').unwrap_or((hp, "80"))
        });
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
    let mut upstream = TcpStream::connect(&upstream_addr)
        .await
        .map_err(|e| format!("connect to {upstream_addr}: {e}"))?;

    if is_connect {
        let established = b"HTTP/1.1 200 Connection Established\r\n\r\n";
        client.write_all(established).await.map_err(|e| format!("write 200: {e}"))?;
    } else {
        upstream.write_all(&buf[..n]).await.map_err(|e| format!("write upstream: {e}"))?;
    }

    let (mut cr, mut cw) = client.into_split();
    let (mut ur, mut uw) = upstream.into_split();

    let c_to_u = tokio::spawn(async move { tokio::io::copy(&mut cr, &mut uw).await });
    let u_to_c = tokio::spawn(async move { tokio::io::copy(&mut ur, &mut cw).await });

    let _ = tokio::try_join!(c_to_u, u_to_c);

    Ok(())
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
