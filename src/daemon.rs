use crate::vscomm::{ExecRequest, Frame, FrameType, VSOCK_PORT};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

struct VsockSession {
    passthrough: Arc<Vec<String>>,
    workspace: PathBuf,
}

pub struct VsockDaemon {
    join_handle: tokio::task::JoinHandle<()>,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

impl VsockDaemon {
    pub fn start(passthrough: Vec<String>, workspace: PathBuf) -> Result<Self, String> {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let session = Arc::new(VsockSession { passthrough: Arc::new(passthrough), workspace });

        let join_handle = tokio::spawn(async move {
            let result = daemon_loop(session, shutdown_rx).await;
            if let Err(err) = result {
                eprintln!("bunkerbox: vsock daemon: {err}");
            }
        });

        Ok(Self { join_handle, shutdown: shutdown_tx })
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.join_handle.await;
    }
}

async fn daemon_loop(session: Arc<VsockSession>, mut shutdown_rx: tokio::sync::oneshot::Receiver<()>) -> Result<(), String> {
    use tokio_vsock::VsockListener;

    let listener = match VsockListener::bind(tokio_vsock::VsockAddr::new(libc::VMADDR_CID_ANY, VSOCK_PORT)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bunkerbox: vsock unavailable (passthrough disabled): {e}");
            let _ = shutdown_rx.await;
            return Ok(());
        }
    };

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _peer)) => {
                        let session = session.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_connection(stream, &session).await {
                                eprintln!("bunkerbox: vsock session error: {err}");
                            }
                        });
                    }
                    Err(e) => {
                        eprintln!("bunkerbox: vsock accept error: {e}");
                    }
                }
            }
            _ = &mut shutdown_rx => {
                break;
            }
        }
    }

    Ok(())
}

async fn handle_connection(stream: tokio_vsock::VsockStream, session: &VsockSession) -> Result<(), String> {
    let (mut reader, mut writer) = tokio::io::split(stream);

    let req = read_exec_request(&mut reader).await?;

    if !is_allowed(&session.passthrough, &req.command, &req.args) {
        let msg = format!("bunkerbox-vscomm: command '{}' not whitelisted\n", req.command);
        write_frame(&mut writer, &Frame::new(FrameType::Stderr, msg.into_bytes())).await?;
        write_frame(&mut writer, &Frame::new(FrameType::Exit, 1i32.to_le_bytes().to_vec())).await?;
        return Ok(());
    }

    let cwd = if req.cwd.starts_with("/workspace") {
        session.workspace.join(req.cwd.strip_prefix("/workspace").unwrap_or(&req.cwd).trim_start_matches('/'))
    } else {
        PathBuf::from(&req.cwd)
    };

    let mut cmd = Command::new(&req.command);
    cmd.args(&req.args);
    cmd.current_dir(&cwd);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    for (key, val) in &req.env {
        if key == "PATH" || key == "HOME" || key == "VSOCK_CID" || key.starts_with("BUNKERBOX_") || key.starts_with("XDG_") {
            continue;
        }
        cmd.env(key, val);
    }

    let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {e}", req.command))?;

    let child_stdout = child.stdout.take().ok_or_else(|| "no stdout".to_string())?;
    let child_stderr = child.stderr.take().ok_or_else(|| "no stderr".to_string())?;

    let (stdout_tx, mut stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (stderr_tx, mut stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    let stdout_task = tokio::spawn(async move {
        pump_to_channel(child_stdout, stdout_tx).await;
    });
    let stderr_task = tokio::spawn(async move {
        pump_to_channel(child_stderr, stderr_tx).await;
    });

    loop {
        tokio::select! {
            chunk = stdout_rx.recv() => {
                if let Some(data) = chunk {
                    write_frame(&mut writer, &Frame::new(FrameType::Stdout, data)).await?;
                }
            }
            chunk = stderr_rx.recv() => {
                if let Some(data) = chunk {
                    write_frame(&mut writer, &Frame::new(FrameType::Stderr, data)).await?;
                }
            }
        }

        if stdout_rx.is_closed() && stderr_rx.is_closed() {
            break;
        }
    }

    let status = child.wait().await.map_err(|e| format!("wait {}: {e}", req.command))?;
    let exit_code = status.code().unwrap_or(-1);
    write_frame(&mut writer, &Frame::new(FrameType::Exit, exit_code.to_le_bytes().to_vec())).await?;

    stdout_task.await.map_err(|e| format!("stdout task: {e}"))?;
    stderr_task.await.map_err(|e| format!("stderr task: {e}"))?;

    Ok(())
}

fn is_allowed(passthrough: &[String], command: &str, args: &[String]) -> bool {
    for entry in passthrough {
        let entry = entry.trim();
        if let Some(cmd) = entry.strip_suffix(" *") {
            if cmd.trim() == command {
                return true;
            }
        } else if entry == command && args.is_empty() {
            return true;
        }
    }
    false
}

async fn read_exec_request<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<ExecRequest, String> {
    let mut header = [0u8; 6];
    reader.read_exact(&mut header).await.map_err(|e| format!("read header: {e}"))?;

    let frame_type_raw = u16::from_le_bytes([header[0], header[1]]);
    let ft = FrameType::from_u16(frame_type_raw).ok_or_else(|| format!("unknown frame type: {frame_type_raw}"))?;

    if !matches!(ft, FrameType::ExecReq) {
        return Err(format!("expected ExecReq, got {:?}", ft as u16));
    }

    let payload_len = u32::from_le_bytes([header[2], header[3], header[4], header[5]]) as usize;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        reader.read_exact(&mut payload).await.map_err(|e| format!("read payload: {e}"))?;
    }

    ExecRequest::deserialize(&payload)
}

async fn pump_to_channel<R: AsyncReadExt + Unpin>(mut reader: R, tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

async fn write_frame<W: AsyncWriteExt + Unpin>(writer: &mut W, frame: &Frame) -> Result<(), String> {
    let frame_type_raw = frame.frame_type as u16;
    let payload_len = frame.payload.len() as u32;

    let mut header = [0u8; 6];
    header[0..2].copy_from_slice(&frame_type_raw.to_le_bytes());
    header[2..6].copy_from_slice(&payload_len.to_le_bytes());

    writer.write_all(&header).await.map_err(|e| format!("write header: {e}"))?;
    if !frame.payload.is_empty() {
        writer.write_all(&frame.payload).await.map_err(|e| format!("write payload: {e}"))?;
    }
    writer.flush().await.map_err(|e| format!("flush: {e}"))?;

    Ok(())
}
