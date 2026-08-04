use crate::proxy::FilterProxy;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const RAW_CONNECT_C: &str = r#"
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <stdlib.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc != 3) return 2;
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) return 1;
    struct sockaddr_in addr = {0};
    addr.sin_family = AF_INET;
    addr.sin_port = htons((unsigned short)atoi(argv[2]));
    addr.sin_addr.s_addr = inet_addr(argv[1]);
    if (connect(fd, (struct sockaddr*)&addr, sizeof(addr)) < 0) return 1;
    close(fd); return 0;
}
"#;

const PROXY_CLIENT_C: &str = r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
int main(int argc, char **argv) {
    if (argc != 2) { fprintf(stderr,"usage: proxy_client <upstream-port>\n"); return 1; }
    char *proxy = getenv("HTTP_PROXY");
    if (!proxy) { fprintf(stderr, "no HTTP_PROXY\n"); return 2; }
    char ph[256]; int pp = 80;
    if (sscanf(proxy, "http://%255[^:]:%d", ph, &pp) < 1) { fprintf(stderr,"bad proxy: %s\n",proxy); return 3; }
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) return 4;
    struct sockaddr_in a = {0}; a.sin_family = AF_INET;
    a.sin_port = htons((unsigned short)pp);
    a.sin_addr.s_addr = inet_addr(ph);
    if (connect(fd, (struct sockaddr*)&a, sizeof(a)) < 0) { fprintf(stderr,"proxy connect fail\n"); return 5; }
    char req[512];
    snprintf(req, sizeof(req), "GET http://127.0.0.1:%s/ok HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n", argv[1]);
    write(fd, req, strlen(req));
    char buf[8192]; int n = read(fd, buf, sizeof(buf)-1);
    if (n > 0) { buf[n] = 0; fwrite(buf, 1, n, stdout); fflush(stdout); }
    close(fd);
    return (n > 0 && strstr(buf, "ok-body")) ? 0 : 6;
}
"#;

fn has_bwrap() -> bool {
    Command::new("bwrap").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn require_bwrap() -> bool {
    if !has_bwrap() {
        eprintln!("SKIP: bwrap not available");
        false
    } else {
        true
    }
}

fn run_bwrap(args: &[&str]) -> Output {
    Command::new("bwrap").args(args).output().expect("spawn bwrap")
}

fn write_temp_source(name: &str, content: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bunkerbox-test-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, content).unwrap();
    path
}

fn bwrap_minimal_with_cc(script: &str) -> Vec<String> {
    vec![
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--tmpfs".into(),
        "/tmp".into(),
        "--ro-bind".into(),
        "/usr/bin/cc".into(),
        "/usr/bin/cc".into(),
        "--ro-bind".into(),
        "/lib".into(),
        "/lib".into(),
        "--ro-bind".into(),
        "/lib64".into(),
        "/lib64".into(),
        "--ro-bind".into(),
        "/usr/lib".into(),
        "/usr/lib".into(),
        "--ro-bind".into(),
        "/usr/include".into(),
        "/usr/include".into(),
        "--unshare-net".into(),
        "--clearenv".into(),
        "--setenv".into(),
        "PATH".into(),
        "/usr/bin:/bin".into(),
        "--".into(),
        "sh".into(),
        "-c".into(),
        script.to_string(),
    ]
}

#[tokio::test]
async fn raw_connect_blocked_with_proxy_enabled() {
    if !require_bwrap() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("proxy.sock");

    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = match echo.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut b = [0u8; 64];
                let n = s.read(&mut b).await.unwrap_or(0);
                if n > 0 {
                    s.write_all(&b[..n]).await.ok();
                }
            });
        }
    });

    let _proxy = FilterProxy::new_test_no_destination_check(vec!["localhost".into()]).bind_unix(&sock_path).await.unwrap();

    let src = write_temp_source("raw_connect.c", RAW_CONNECT_C);

    let script = format!("cc -o /tmp/raw_connect {} 2>/dev/null && /tmp/raw_connect 127.0.0.1 {}; exit $?", src.display(), echo_port);

    let mut bwrap_args = bwrap_minimal_with_cc(&script);
    bwrap_args.push("--ro-bind".into());
    bwrap_args.push(src.to_string_lossy().to_string());
    bwrap_args.push(src.to_string_lossy().to_string());
    bwrap_args.push("--setenv".into());
    bwrap_args.push("HTTP_PROXY".into());
    bwrap_args.push("http://127.0.0.1:20000".into());

    let bwrap_refs: Vec<&str> = bwrap_args.iter().map(|s| s.as_str()).collect();
    let output = run_bwrap(&bwrap_refs);
    assert!(
        !output.status.success(),
        "raw connect inside --unshare-net with proxy should still fail: stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test]
async fn allowed_proxied_http_succeeds() {
    if !require_bwrap() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("proxy.sock");

    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = match upstream.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                s.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 7\r\n\r\nok-body").await.ok();
            });
        }
    });

    let _proxy = FilterProxy::new_test_no_destination_check(vec!["127.0.0.1".into()]).bind_unix(&sock_path).await.unwrap();

    let netrelay_path = std::env::current_exe().unwrap().parent().unwrap().join("bunkerbox-netrelay");
    if !netrelay_path.is_file() {
        eprintln!("SKIP: bunkerbox-netrelay not found");
        return;
    }

    let src = write_temp_source("proxy_client.c", PROXY_CLIENT_C);

    let script = format!("cc -o /tmp/client {} 2>/dev/null && /tmp/client {}; exit $?", src.display(), upstream_port);

    let mut bwrap_args = bwrap_minimal_with_cc(&script);
    bwrap_args.push("--ro-bind".into());
    bwrap_args.push(src.to_string_lossy().to_string());
    bwrap_args.push(src.to_string_lossy().to_string());
    bwrap_args.push("--dir".into());
    bwrap_args.push("/run/bunkerbox".into());
    bwrap_args.push("--ro-bind".into());
    bwrap_args.push(netrelay_path.to_string_lossy().to_string());
    bwrap_args.push("/run/bunkerbox/netrelay".into());
    bwrap_args.push("--ro-bind".into());
    bwrap_args.push(sock_path.to_string_lossy().to_string());
    bwrap_args.push("/run/bunkerbox/proxy.sock".into());
    bwrap_args.push("--setenv".into());
    bwrap_args.push("HTTP_PROXY".into());
    bwrap_args.push("http://127.0.0.1:20000".into());

    let bwrap_refs: Vec<&str> = bwrap_args.iter().map(|s| s.as_str()).collect();
    let output = run_bwrap(&bwrap_refs);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "proxied HTTP should succeed: {}", stdout);
    assert!(stdout.contains("ok-body"), "response should contain ok-body: {}", stdout);
}

#[tokio::test]
async fn denied_proxied_http_fails() {
    if !require_bwrap() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("proxy.sock");

    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = match upstream.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                s.write_all(b"HTTP/1.0 200 OK\r\n\r\nbody").await.ok();
            });
        }
    });

    let _proxy = FilterProxy::new_test_no_destination_check(vec!["only-this-host.example".into()]).bind_unix(&sock_path).await.unwrap();

    let netrelay_path = std::env::current_exe().unwrap().parent().unwrap().join("bunkerbox-netrelay");
    if !netrelay_path.is_file() {
        eprintln!("SKIP: bunkerbox-netrelay not found");
        return;
    }

    let src = write_temp_source("proxy_client.c", PROXY_CLIENT_C);

    let script = format!("cc -o /tmp/client {} 2>/dev/null && /tmp/client {}; exit $?", src.display(), upstream_port);

    let mut bwrap_args = bwrap_minimal_with_cc(&script);
    bwrap_args.push("--ro-bind".into());
    bwrap_args.push(src.to_string_lossy().to_string());
    bwrap_args.push(src.to_string_lossy().to_string());
    bwrap_args.push("--dir".into());
    bwrap_args.push("/run/bunkerbox".into());
    bwrap_args.push("--ro-bind".into());
    bwrap_args.push(netrelay_path.to_string_lossy().to_string());
    bwrap_args.push("/run/bunkerbox/netrelay".into());
    bwrap_args.push("--ro-bind".into());
    bwrap_args.push(sock_path.to_string_lossy().to_string());
    bwrap_args.push("/run/bunkerbox/proxy.sock".into());
    bwrap_args.push("--setenv".into());
    bwrap_args.push("HTTP_PROXY".into());
    bwrap_args.push("http://127.0.0.1:20000".into());

    let bwrap_refs: Vec<&str> = bwrap_args.iter().map(|s| s.as_str()).collect();
    let output = run_bwrap(&bwrap_refs);
    assert!(!output.status.success(), "denied proxied HTTP should fail: {}", String::from_utf8_lossy(&output.stdout));
}

#[test]
fn startup_cleanup_removes_owned_resources() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("proxy.sock");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let handle = rt.block_on(FilterProxy::new_test_no_destination_check(vec!["localhost".into()]).bind_unix(&sock_path)).unwrap();
    assert!(sock_path.exists());

    handle.stop();
    assert!(!sock_path.exists());
}
