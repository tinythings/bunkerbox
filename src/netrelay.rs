use crate::proxy::PORT as PROXY_PORT;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use tokio::net::{TcpListener, UnixStream};
use tokio::process::Command;
use tokio::task::JoinSet;

const BIND_ADDR: &str = "127.0.0.1";

pub async fn bind_relay_listener() -> Result<TcpListener, String> {
    let addr: SocketAddr = format!("{BIND_ADDR}:{PROXY_PORT}").parse().map_err(|e| format!("invalid relay bind address: {e}"))?;
    TcpListener::bind(addr).await.map_err(|e| format!("failed to bind relay on {addr}: {e}"))
}

pub async fn relay(listener: TcpListener, socket_path: PathBuf, target_args: &[String]) -> Result<ExitStatus, String> {
    if target_args.is_empty() {
        return Err("no target command".to_string());
    }

    let relay_task = tokio::spawn(accept_loop(listener, socket_path));

    let mut child = spawn_target(target_args).map_err(|e| format!("spawn target '{}': {e}", target_args[0]))?;

    let status = child.wait().await.map_err(|e| format!("wait target: {e}"))?;

    relay_task.abort();
    let _ = relay_task.await;

    Ok(status)
}

async fn accept_loop(listener: TcpListener, socket_path: PathBuf) {
    let mut connections = JoinSet::new();

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (mut tcp, _) = match result {
                    Ok(v) => v,
                    Err(_) => return,
                };

                let path = socket_path.clone();
                connections.spawn(async move {
                    let mut unix = match UnixStream::connect(&path).await {
                        Ok(u) => u,
                        Err(_) => return,
                    };
                    tokio::io::copy_bidirectional(&mut tcp, &mut unix).await.ok();
                });
            }
            result = connections.join_next(), if !connections.is_empty() => {
                let _ = result;
            }
        }
    }
}

fn target_command(target_args: &[String]) -> Command {
    let mut cmd = Command::new(&target_args[0]);
    cmd.args(&target_args[1..]);
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    cmd.kill_on_drop(true);
    cmd
}

fn spawn_target(target_args: &[String]) -> std::io::Result<tokio::process::Child> {
    target_command(target_args).spawn()
}

#[cfg(test)]
#[path = "netrelay_ut.rs"]
mod netrelay_tests;
