mod common;
use common::{require_bwrap, run_bwrap};
use std::fs;
use std::path::PathBuf;

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

#[test]
fn raw_connect_blocked_inside_unshare_net() {
    if !require_bwrap() {
        return;
    }
    let src = write_temp_source("raw_connect.c", RAW_CONNECT_C);

    let script = format!("cc -o /tmp/raw_connect {} 2>/dev/null && /tmp/raw_connect 127.0.0.1 18081; exit $?", src.display());

    let mut bwrap_args = bwrap_minimal_with_cc(&script);
    bwrap_args.push("--ro-bind".into());
    bwrap_args.push(src.to_string_lossy().to_string());
    bwrap_args.push(src.to_string_lossy().to_string());

    let bwrap_refs: Vec<&str> = bwrap_args.iter().map(|s| s.as_str()).collect();
    let output = run_bwrap(&bwrap_refs);
    assert!(!output.status.success(), "raw connect inside --unshare-net should fail");
}

#[tokio::test]
async fn missing_proxy_socket_fails_closed() {
    if !require_bwrap() {
        return;
    }
    let src = write_temp_source("raw_connect.c", RAW_CONNECT_C);

    let script = format!("cc -o /tmp/raw {} 2>/dev/null && /tmp/raw 127.0.0.1 18085; exit $?", src.display());

    let mut bwrap_args = bwrap_minimal_with_cc(&script);
    bwrap_args.push("--ro-bind".into());
    bwrap_args.push(src.to_string_lossy().to_string());
    bwrap_args.push(src.to_string_lossy().to_string());
    bwrap_args.push("--setenv".into());
    bwrap_args.push("HTTP_PROXY".into());
    bwrap_args.push("http://127.0.0.1:20000".into());

    let bwrap_refs: Vec<&str> = bwrap_args.iter().map(|s| s.as_str()).collect();
    let output = run_bwrap(&bwrap_refs);
    assert!(!output.status.success(), "raw connect should fail even with proxy env set");
}

#[test]
fn exploit_artifact_preserved() {
    assert!(RAW_CONNECT_C.contains("socket(AF_INET, SOCK_STREAM, 0)"));
    assert!(RAW_CONNECT_C.contains("connect(fd"));
    assert!(RAW_CONNECT_C.contains("AF_INET"));
    assert!(!RAW_CONNECT_C.contains("HTTP_PROXY"));
    assert!(!RAW_CONNECT_C.contains("http_proxy"));
}
