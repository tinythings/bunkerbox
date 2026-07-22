use bunkerbox::proxy::FilterProxy;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn proxy_allows_connect_to_allowed_host() {
    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = echo.accept().await.unwrap();
        let mut buf = [0u8; 64];
        let n = sock.read(&mut buf).await.unwrap();
        sock.write_all(&buf[..n]).await.unwrap();
    });

    let (handle, proxy_port) = FilterProxy::new(vec!["localhost".into()])
        .bind_on(0)
        .await
        .unwrap();

    let mut client = tokio::net::TcpStream::connect(format!("127.0.0.1:{proxy_port}"))
        .await
        .unwrap();

    client
        .write_all(format!("CONNECT localhost:{echo_port} HTTP/1.1\r\n\r\n").as_bytes())
        .await
        .unwrap();

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
async fn proxy_blocks_connect_to_denied_host() {
    let (handle, proxy_port) = FilterProxy::new(vec!["only.this.host".into()])
        .bind_on(0)
        .await
        .unwrap();

    let mut client = tokio::net::TcpStream::connect(format!("127.0.0.1:{proxy_port}"))
        .await
        .unwrap();

    client
        .write_all(b"CONNECT evil.com:443 HTTP/1.1\r\n\r\n")
        .await
        .unwrap();

    let mut response = [0u8; 256];
    let n = client.read(&mut response).await.unwrap();
    let resp = String::from_utf8_lossy(&response[..n]);
    assert!(resp.contains("403 Forbidden"), "got: {resp}");

    handle.abort();
}

#[tokio::test]
async fn proxy_forwards_plain_http_to_allowed_host() {
    let srv = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let srv_port = srv.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = srv.accept().await.unwrap();
        sock.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\n\r\nworld")
            .await
            .unwrap();
    });

    let (handle, proxy_port) = FilterProxy::new(vec!["localhost".into()])
        .bind_on(0)
        .await
        .unwrap();

    let mut client = tokio::net::TcpStream::connect(format!("127.0.0.1:{proxy_port}"))
        .await
        .unwrap();

    client
        .write_all(
            format!("GET http://localhost:{srv_port}/items HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();

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
