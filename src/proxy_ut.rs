use super::*;
use std::fs;
use std::io::Write;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixStream};

#[test]
fn test_parse_host_port_with_port() {
    let (host, port) = parse_host_port("example.com:443").unwrap();
    assert_eq!(host, "example.com");
    assert_eq!(port, "443");
}

#[test]
fn test_parse_host_port_without_port() {
    let (host, port) = parse_host_port("example.com").unwrap();
    assert_eq!(host, "example.com");
    assert_eq!(port, "443");
}

#[test]
fn test_is_allowed_exact_match() {
    let allow = vec!["crates.io".to_string()];
    assert!(is_allowed("crates.io", &allow));
}

#[test]
fn test_is_allowed_subdomain_match() {
    let allow = vec!["crates.io".to_string()];
    assert!(is_allowed("static.crates.io", &allow));
}

#[test]
fn test_is_allowed_not_matched() {
    let allow = vec!["crates.io".to_string()];
    assert!(!is_allowed("evil.com", &allow));
}

#[test]
fn test_is_allowed_case_insensitive() {
    let allow = vec!["Crates.IO".to_string()];
    assert!(is_allowed("static.crates.io", &allow));
}

#[test]
fn test_is_allowed_partial_no_match() {
    let allow = vec!["crates.io".to_string()];
    assert!(!is_allowed("notcrates.io", &allow));
}

#[test]
fn test_is_allowed_badexample_no_match() {
    let allow = vec!["example.com".to_string()];
    assert!(!is_allowed("badexample.com", &allow));
}

#[test]
fn test_normalize_host_trailing_dot() {
    assert_eq!(normalize_host("example.com.").unwrap(), "example.com");
    assert_eq!(normalize_host("Foo.Example.COM.").unwrap(), "foo.example.com");
}

#[test]
fn test_normalize_host_empty() {
    assert!(normalize_host("").is_err());
    assert!(normalize_host(".").is_err());
}

#[test]
fn test_is_public_destination_rejects_loopback() {
    assert!(!is_public_destination(&"127.0.0.1:80".parse().unwrap()));
    assert!(!is_public_destination(&"127.0.0.2:80".parse().unwrap()));
}

#[test]
fn test_is_public_destination_rejects_private() {
    assert!(!is_public_destination(&"10.0.0.1:80".parse().unwrap()));
    assert!(!is_public_destination(&"172.16.0.1:80".parse().unwrap()));
    assert!(!is_public_destination(&"192.168.1.1:80".parse().unwrap()));
}

#[test]
fn test_is_public_destination_rejects_link_local() {
    assert!(!is_public_destination(&"169.254.1.1:80".parse().unwrap()));
}

#[test]
fn test_is_public_destination_rejects_unspecified() {
    assert!(!is_public_destination(&"0.0.0.0:80".parse().unwrap()));
}

#[test]
fn test_is_public_destination_rejects_multicast() {
    assert!(!is_public_destination(&"224.0.0.1:80".parse().unwrap()));
}

#[test]
fn test_is_public_destination_rejects_broadcast() {
    assert!(!is_public_destination(&"255.255.255.255:80".parse().unwrap()));
}

#[test]
fn test_is_public_destination_rejects_current_network() {
    assert!(!is_public_destination(&"0.0.0.1:80".parse().unwrap()));
    assert!(!is_public_destination(&"0.255.255.255:80".parse().unwrap()));
}

#[test]
fn test_is_public_destination_rejects_cgnat() {
    assert!(!is_public_destination(&"100.64.0.1:80".parse().unwrap()));
    assert!(!is_public_destination(&"100.127.255.255:80".parse().unwrap()));
}

#[test]
fn test_is_public_destination_rejects_special_ranges() {
    assert!(!is_public_destination(&"192.0.0.1:80".parse().unwrap()));
    assert!(!is_public_destination(&"192.0.2.1:80".parse().unwrap()));
    assert!(!is_public_destination(&"198.18.0.1:80".parse().unwrap()));
    assert!(!is_public_destination(&"198.51.100.1:80".parse().unwrap()));
    assert!(!is_public_destination(&"203.0.113.1:80".parse().unwrap()));
    assert!(!is_public_destination(&"240.0.0.1:80".parse().unwrap()));
}

#[test]
fn test_is_public_destination_rejects_ipv6_loopback() {
    assert!(!is_public_destination(&"[::1]:80".parse().unwrap()));
}

#[test]
fn test_is_public_destination_rejects_ipv6_unspecified() {
    assert!(!is_public_destination(&"[::]:80".parse().unwrap()));
}

#[test]
fn test_is_public_destination_rejects_ipv6_multicast() {
    assert!(!is_public_destination(&"[ff02::1]:80".parse().unwrap()));
}

#[test]
fn test_is_public_destination_rejects_ipv6_unique_local() {
    assert!(!is_public_destination(&"[fc00::1]:80".parse().unwrap()));
    assert!(!is_public_destination(&"[fd00::1]:80".parse().unwrap()));
}

#[test]
fn test_is_public_destination_rejects_ipv6_link_local() {
    assert!(!is_public_destination(&"[fe80::1]:80".parse().unwrap()));
}

#[test]
fn test_is_public_destination_rejects_ipv6_documentation() {
    assert!(!is_public_destination(&"[2001:db8::1]:80".parse().unwrap()));
}

#[test]
fn test_is_public_destination_rejects_ipv4_mapped_ipv6() {
    assert!(!is_public_destination(&"[::ffff:127.0.0.1]:80".parse().unwrap()));
    assert!(!is_public_destination(&"[::ffff:10.0.0.1]:80".parse().unwrap()));
    assert!(!is_public_destination(&"[::ffff:172.16.0.1]:80".parse().unwrap()));
    assert!(!is_public_destination(&"[::ffff:192.168.1.1]:80".parse().unwrap()));
}

#[test]
fn test_is_public_destination_accepts_global() {
    assert!(is_public_destination(&"1.1.1.1:80".parse().unwrap()));
    assert!(is_public_destination(&"8.8.8.8:53".parse().unwrap()));
    assert!(is_public_destination(&"93.184.216.34:443".parse().unwrap()));
    assert!(is_public_destination(&"[2606:4700::6810:85e5]:443".parse().unwrap()));
}

#[tokio::test]
async fn proxy_unix_connect_allowed() {
    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = echo.accept().await.unwrap();
        let mut buf = [0u8; 64];
        let n = sock.read(&mut buf).await.unwrap();
        sock.write_all(&buf[..n]).await.unwrap();
    });

    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("proxy.sock");
    let handle = FilterProxy::new_test_no_destination_check(vec!["localhost".into()]).bind_unix(&sock_path).await.unwrap();

    let mut client = UnixStream::connect(&sock_path).await.unwrap();

    client.write_all(format!("CONNECT localhost:{echo_port} HTTP/1.1\r\n\r\n").as_bytes()).await.unwrap();

    let mut response = [0u8; 256];
    let n = client.read(&mut response).await.unwrap();
    let resp = String::from_utf8_lossy(&response[..n]);
    assert!(resp.contains("200 Connection Established"), "got: {resp}");

    client.write_all(b"hello").await.unwrap();
    let mut echo_back = [0u8; 64];
    let n = client.read(&mut echo_back).await.unwrap();
    assert_eq!(&echo_back[..n], b"hello");

    handle.stop();
}

#[tokio::test]
async fn proxy_unix_reject_denied() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("proxy.sock");
    let handle = FilterProxy::new_test_no_destination_check(vec!["only.this.host".into()]).bind_unix(&sock_path).await.unwrap();

    let mut client = UnixStream::connect(&sock_path).await.unwrap();

    client.write_all(b"CONNECT evil.com:443 HTTP/1.1\r\n\r\n").await.unwrap();

    let mut response = [0u8; 256];
    let n = client.read(&mut response).await.unwrap();
    let resp = String::from_utf8_lossy(&response[..n]);
    assert!(resp.contains("403 Forbidden"), "got: {resp}");

    handle.stop();
}

#[tokio::test]
async fn proxy_unix_plain_http() {
    let srv = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let srv_port = srv.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = srv.accept().await.unwrap();
        sock.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\n\r\nworld").await.unwrap();
    });

    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("proxy.sock");
    let handle = FilterProxy::new_test_no_destination_check(vec!["localhost".into()]).bind_unix(&sock_path).await.unwrap();

    let mut client = UnixStream::connect(&sock_path).await.unwrap();

    client.write_all(format!("GET http://localhost:{srv_port}/items HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes()).await.unwrap();

    let mut response = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match client.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let resp = String::from_utf8_lossy(&response);
    assert!(resp.contains("world"), "got: {resp}");

    handle.stop();
}

#[tokio::test]
async fn proxy_unix_bind_failure() {
    let result = FilterProxy::new(vec!["localhost".into()]).bind_unix("/nonexistent/dir/sock").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn proxy_unix_bind_over_existing_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not-a-socket");

    let mut f = fs::File::create(&path).unwrap();
    f.write_all(b"some data").unwrap();
    drop(f);

    let result = FilterProxy::new(vec!["localhost".into()]).bind_unix(&path).await;
    assert!(result.is_err());

    assert!(path.exists());
    let contents = fs::read_to_string(&path).unwrap();
    assert_eq!(contents, "some data");
}

#[tokio::test]
async fn proxy_unix_bind_over_active_socket_fails() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("proxy.sock");

    let first = FilterProxy::new(vec!["localhost".into()]).bind_unix(&sock_path).await.unwrap();
    assert!(sock_path.exists());

    let second = FilterProxy::new(vec!["localhost".into()]).bind_unix(&sock_path).await;
    assert!(second.is_err());

    assert!(sock_path.exists());

    first.stop();
}

#[tokio::test]
async fn proxy_unix_cleanup_preserves_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("proxy.sock");

    let handle = FilterProxy::new(vec!["localhost".into()]).bind_unix(&sock_path).await.unwrap();
    assert!(sock_path.exists());

    let saved_dev;
    let saved_ino;
    {
        let meta = fs::symlink_metadata(&sock_path).unwrap();
        saved_dev = meta.dev();
        saved_ino = meta.ino();
    }
    assert_eq!(handle.dev, saved_dev);
    assert_eq!(handle.ino, saved_ino);

    fs::remove_file(&sock_path).unwrap();
    assert!(!sock_path.exists());

    let mut f = fs::File::create(&sock_path).unwrap();
    f.write_all(b"replacement data").unwrap();
    drop(f);
    assert!(sock_path.exists());

    drop(handle);

    assert!(sock_path.exists());
    let contents = fs::read_to_string(&sock_path).unwrap();
    assert_eq!(contents, "replacement data");
}

#[tokio::test]
async fn proxy_unix_rejects_allowed_host_to_private() {
    let srv = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let srv_port = srv.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match srv.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                sock.write_all(b"HTTP/1.0 200 OK\r\n\r\nbody").await.ok();
            });
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("proxy.sock");
    let handle = FilterProxy::new(vec!["localhost".into()]).bind_unix(&sock_path).await.unwrap();

    let mut client = UnixStream::connect(&sock_path).await.unwrap();

    client.write_all(format!("GET http://localhost:{srv_port}/ HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes()).await.unwrap();

    let mut response = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match client.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let resp = String::from_utf8_lossy(&response);
    assert!(!resp.contains("200 OK"), "production proxy should reject localhost: {}", resp);

    handle.stop();
}

#[tokio::test]
async fn proxy_tcp_connect_allowed() {
    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = echo.accept().await.unwrap();
        let mut buf = [0u8; 64];
        let n = sock.read(&mut buf).await.unwrap();
        sock.write_all(&buf[..n]).await.unwrap();
    });

    let (handle, proxy_port) = FilterProxy::new_test_no_destination_check(vec!["localhost".into()]).bind_on(0).await.unwrap();

    let mut client = tokio::net::TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await.unwrap();

    client.write_all(format!("CONNECT localhost:{echo_port} HTTP/1.1\r\n\r\n").as_bytes()).await.unwrap();

    let mut response = [0u8; 256];
    let n = client.read(&mut response).await.unwrap();
    let resp = String::from_utf8_lossy(&response[..n]);
    assert!(resp.contains("200 Connection Established"), "got: {resp}");

    client.write_all(b"hello").await.unwrap();
    let mut echo_back = [0u8; 64];
    let n = client.read(&mut echo_back).await.unwrap();
    assert_eq!(&echo_back[..n], b"hello");

    handle.abort();
}

#[tokio::test]
async fn proxy_tcp_blocks_denied_host() {
    let (handle, proxy_port) = FilterProxy::new_test_no_destination_check(vec!["only.this.host".into()]).bind_on(0).await.unwrap();

    let mut client = tokio::net::TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await.unwrap();

    client.write_all(b"CONNECT evil.com:443 HTTP/1.1\r\n\r\n").await.unwrap();

    let mut response = [0u8; 256];
    let n = client.read(&mut response).await.unwrap();
    let resp = String::from_utf8_lossy(&response[..n]);
    assert!(resp.contains("403 Forbidden"), "got: {resp}");

    handle.abort();
}

#[tokio::test]
async fn proxy_tcp_forwards_plain_http() {
    let srv = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let srv_port = srv.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = srv.accept().await.unwrap();
        sock.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\n\r\nworld").await.unwrap();
    });

    let (handle, proxy_port) = FilterProxy::new_test_no_destination_check(vec!["localhost".into()]).bind_on(0).await.unwrap();

    let mut client = tokio::net::TcpStream::connect(format!("127.0.0.1:{proxy_port}")).await.unwrap();

    client.write_all(format!("GET http://localhost:{srv_port}/items HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes()).await.unwrap();

    let mut response = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match client.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let resp = String::from_utf8_lossy(&response);
    assert!(resp.contains("world"), "got: {resp}");

    handle.abort();
}
