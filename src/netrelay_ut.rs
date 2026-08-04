use super::*;
use std::ffi::OsStr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};

#[tokio::test]
async fn tcp_to_unix_forwarding() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("echo.sock");

    let echo = UnixListener::bind(&sock_path).unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match echo.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n > 0 {
                    stream.write_all(&buf[..n]).await.ok();
                }
            });
        }
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_port = listener.local_addr().unwrap().port();
    let relay_task = tokio::spawn(accept_loop(listener, sock_path.clone()));

    let mut client = tokio::net::TcpStream::connect(format!("127.0.0.1:{relay_port}")).await.unwrap();
    client.write_all(b"hello").await.unwrap();

    let mut response = [0u8; 64];
    let n = client.read(&mut response).await.unwrap();
    assert_eq!(&response[..n], b"hello");

    relay_task.abort();
    let _ = relay_task.await;
}

#[tokio::test]
async fn bidirectional_forwarding() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("echo.sock");

    let echo = UnixListener::bind(&sock_path).unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match echo.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n > 0 {
                    stream.write_all(&buf[..n]).await.ok();
                }
            });
        }
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_port = listener.local_addr().unwrap().port();
    let relay_task = tokio::spawn(accept_loop(listener, sock_path.clone()));

    let mut client = tokio::net::TcpStream::connect(format!("127.0.0.1:{relay_port}")).await.unwrap();
    client.write_all(b"ping").await.unwrap();

    let mut response = [0u8; 64];
    let n = client.read(&mut response).await.unwrap();
    assert_eq!(&response[..n], b"ping");

    relay_task.abort();
    let _ = relay_task.await;
}

#[tokio::test]
async fn multiple_simultaneous_connections() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("echo.sock");

    let echo = UnixListener::bind(&sock_path).unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match echo.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n > 0 {
                    stream.write_all(&buf[..n]).await.ok();
                }
            });
        }
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_port = listener.local_addr().unwrap().port();
    let relay_task = tokio::spawn(accept_loop(listener, sock_path.clone()));

    let mut handles = Vec::new();
    for i in 0u8..3 {
        handles.push(tokio::spawn(async move {
            let mut client = tokio::net::TcpStream::connect(format!("127.0.0.1:{relay_port}")).await.unwrap();
            let msg = [i; 4];
            client.write_all(&msg).await.unwrap();
            let mut response = [0u8; 64];
            let n = client.read(&mut response).await.unwrap();
            assert_eq!(&response[..n], &msg);
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    relay_task.abort();
    let _ = relay_task.await;
}

#[tokio::test]
async fn target_exit_status_zero() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sock_path = std::env::temp_dir().join(format!("test-relay-{}.sock", std::process::id()));

    let status = relay(listener, sock_path, &["true".to_string()]).await.unwrap();
    assert_eq!(status.code(), Some(0));
}

#[tokio::test]
async fn target_exit_status_nonzero() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sock_path = std::env::temp_dir().join(format!("test-relay-{}.sock", std::process::id()));

    let status = relay(listener, sock_path, &["sh".to_string(), "-c".to_string(), "exit 42".to_string()]).await.unwrap();
    assert_eq!(status.code(), Some(42));
}

#[tokio::test]
async fn missing_unix_socket_accept_loop_stays_alive() {
    let dir = tempfile::tempdir().unwrap();
    let nonexistent = dir.path().join("nonexistent.sock");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_port = listener.local_addr().unwrap().port();
    let relay_task = tokio::spawn(accept_loop(listener, nonexistent));

    let mut client1 = tokio::net::TcpStream::connect(format!("127.0.0.1:{relay_port}")).await.unwrap();
    client1.write_all(b"data").await.unwrap();
    let mut buf = [0u8; 64];
    let n = client1.read(&mut buf).await.unwrap_or(0);
    assert_eq!(n, 0);

    let mut client2 = tokio::net::TcpStream::connect(format!("127.0.0.1:{relay_port}")).await.unwrap();
    client2.write_all(b"data2").await.unwrap();
    let n = client2.read(&mut buf).await.unwrap_or(0);
    assert_eq!(n, 0);

    relay_task.abort();
    let _ = relay_task.await;
}

#[tokio::test]
async fn bind_relay_listener_fails_on_occupied_port() {
    let occupant = TcpListener::bind("127.0.0.1:20000").await.unwrap();
    let result = bind_relay_listener().await;
    assert!(result.is_err());
    drop(occupant);

    let result = bind_relay_listener().await;
    assert!(result.is_ok());
}

#[test]
fn argv_spaces_preserved() {
    let cmd = target_command(&["myprog".into(), "arg with spaces".into()]);
    let args: Vec<_> = cmd.as_std().get_args().collect();
    assert!(args.iter().any(|a| *a == OsStr::new("arg with spaces")));
}

#[test]
fn argv_shell_metacharacters_not_interpreted() {
    let cmd = target_command(&["myprog".into(), "$HOME".into(), "$(id)".into(), "a;b".into()]);
    let args: Vec<_> = cmd.as_std().get_args().collect();
    assert!(args.iter().any(|a| *a == OsStr::new("$HOME")));
    assert!(args.iter().any(|a| *a == OsStr::new("$(id)")));
    assert!(args.iter().any(|a| *a == OsStr::new("a;b")));
}

#[test]
fn target_command_argv_zero_is_executable() {
    let cmd = target_command(&["/usr/bin/env".into(), "VAR=val".into()]);
    assert_eq!(cmd.as_std().get_program(), "/usr/bin/env");
}

#[tokio::test]
async fn accept_loop_shutdown_terminates_connections() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("hang.sock");

    let hang = UnixListener::bind(&sock_path).unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match hang.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let _ = stream.read(&mut [0u8; 1]).await;
            });
        }
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_port = listener.local_addr().unwrap().port();
    let relay_task = tokio::spawn(accept_loop(listener, sock_path));

    let mut client = tokio::net::TcpStream::connect(format!("127.0.0.1:{relay_port}")).await.unwrap();
    client.write_all(b"hanging").await.unwrap();

    relay_task.abort();
    let _ = relay_task.await;

    let mut buf = [0u8; 64];
    let n = client.read(&mut buf).await.unwrap_or(0);
    assert_eq!(n, 0);
}

#[tokio::test]
async fn accept_loop_reaps_completed_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("echo.sock");

    let echo = UnixListener::bind(&sock_path).unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match echo.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n > 0 {
                    stream.write_all(&buf[..n]).await.ok();
                }
            });
        }
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_port = listener.local_addr().unwrap().port();
    let relay_task = tokio::spawn(accept_loop(listener, sock_path));

    for i in 0u8..10 {
        let mut client = tokio::net::TcpStream::connect(format!("127.0.0.1:{relay_port}")).await.unwrap();
        let msg = [i; 4];
        client.write_all(&msg).await.unwrap();
        let mut response = [0u8; 64];
        let n = client.read(&mut response).await.unwrap();
        assert_eq!(&response[..n], &msg);
    }

    relay_task.abort();
    let _ = relay_task.await;
}
