use crate::logging;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
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
    check_destinations: bool,
}

impl FilterProxy {
    pub fn new(allow: Vec<String>) -> Self {
        Self { allow, check_destinations: true }
    }

    #[allow(dead_code)]
    pub(crate) fn new_test_no_destination_check(allow: Vec<String>) -> Self {
        Self { allow, check_destinations: false }
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
        let check_destinations = self.check_destinations;

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => {
                        let allow = allow.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(stream, &allow, check_destinations).await {
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
        let check_destinations = self.check_destinations;

        let task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => {
                        let allow = allow.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(stream, &allow, check_destinations).await {
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

async fn handle_client<C>(mut client: C, allow: &[String], check_destinations: bool) -> Result<(), String>
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

    let host = normalize_host(&host)?;

    if !is_allowed(&host, allow) {
        let forbidden = b"HTTP/1.1 403 Forbidden\r\n\r\n";
        let _ = client.write_all(forbidden).await;
        return Err(format!("blocked: {host}"));
    }

    let port_u16: u16 = port.parse().map_err(|_| format!("invalid port: {port}"))?;

    let mut upstream = if check_destinations { connect_upstream(&host, port_u16).await? } else { connect_direct(&host, port_u16).await? };

    if is_connect {
        let established = b"HTTP/1.1 200 Connection Established\r\n\r\n";
        client.write_all(established).await.map_err(|e| format!("write 200: {e}"))?;
    } else {
        upstream.write_all(&buf[..n]).await.map_err(|e| format!("write upstream: {e}"))?;
    }

    tokio::io::copy_bidirectional(&mut client, &mut upstream).await.map(|_| ()).map_err(|e| format!("relay error: {e}"))
}

fn normalize_host(host: &str) -> Result<String, String> {
    let host = host.trim_end_matches('.').trim();
    if host.is_empty() {
        return Err("empty host".to_string());
    }
    Ok(host.to_lowercase())
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
    let host_lower = host;
    allow.iter().any(|entry| {
        let entry_lower = entry.to_lowercase();
        host_lower == entry_lower || host_lower.ends_with(&format!(".{entry_lower}"))
    })
}

pub fn is_public_destination(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(v4) => is_public_ipv4(v4),
        IpAddr::V6(v6) => is_public_ipv6(v6),
    }
}

fn is_public_ipv4(v4: Ipv4Addr) -> bool {
    if v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast() {
        return false;
    }

    let bits = u32::from(v4);
    if (bits >> 24) == 0 {
        return false;
    }
    if bits & 0xFFC00000 == u32::from(Ipv4Addr::new(100, 64, 0, 0)) {
        return false;
    }
    if (bits >> 8) == (u32::from(Ipv4Addr::new(192, 0, 0, 0)) >> 8) {
        return false;
    }
    if (bits >> 8) == (u32::from(Ipv4Addr::new(192, 0, 2, 0)) >> 8) {
        return false;
    }
    if (bits >> 9) == (u32::from(Ipv4Addr::new(198, 18, 0, 0)) >> 9) {
        return false;
    }
    if (bits >> 8) == (u32::from(Ipv4Addr::new(198, 51, 100, 0)) >> 8) {
        return false;
    }
    if (bits >> 8) == (u32::from(Ipv4Addr::new(203, 0, 113, 0)) >> 8) {
        return false;
    }
    if (bits & 0xF0000000) == 0xF0000000 {
        return false;
    }

    true
}

fn is_public_ipv6(v6: Ipv6Addr) -> bool {
    if let Some(v4) = v6.to_ipv4_mapped() {
        return is_public_ipv4(v4);
    }
    if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || v6.is_unique_local() || v6.is_unicast_link_local() {
        return false;
    }

    let bits = u128::from(v6);
    if bits >> 96 == u128::from(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0)) >> 96 {
        return false;
    }

    true
}

async fn resolve_and_validate(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    let target = format!("{host}:{port}");
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(&target).await.map_err(|e| format!("DNS resolution failed for {host}: {e}"))?.collect();

    let valid: Vec<SocketAddr> = addrs.into_iter().filter(is_public_destination).collect();
    if valid.is_empty() {
        return Err(format!("all resolved addresses for {host} are forbidden destinations"));
    }
    Ok(valid)
}

async fn connect_upstream(host: &str, port: u16) -> Result<TcpStream, String> {
    let candidates = resolve_and_validate(host, port).await?;

    for addr in &candidates {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(_) => continue,
        }
    }

    Err(format!("failed to connect to {host}:{port} ({} addresses tried)", candidates.len()))
}

async fn connect_direct(host: &str, port: u16) -> Result<TcpStream, String> {
    let target = format!("{host}:{port}");
    TcpStream::connect(&target).await.map_err(|e| format!("connect to {target}: {e}"))
}

#[cfg(test)]
#[path = "proxy_ut.rs"]
mod proxy_tests;
