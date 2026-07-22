use crate::cfg::EnvMode;
use crate::sandbox::{resolve_profile, MergedProfile, NetworkMode};
use crate::vscomm::{ExecRequest, Frame, FrameType, VSOCK_PORT};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

struct VsockSession {
    passthrough: Arc<Vec<String>>,
    env_mode: EnvMode,
    workspace: PathBuf,
    merged_profile: Option<Arc<MergedProfile>>,
}

pub struct VsockDaemon {
    join_handle: tokio::task::JoinHandle<()>,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

impl VsockDaemon {
    pub fn start(
        passthrough: Vec<String>,
        env_mode: EnvMode,
        workspace: PathBuf,
        profiles: Vec<String>,
        share_dir: PathBuf,
    ) -> Result<Self, String> {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let merged_profile = if profiles.is_empty() {
            None
        } else {
            let loaded: Vec<_> = profiles
                .iter()
                .map(|p| resolve_profile(p, &share_dir))
                .collect::<Result<Vec<_>, _>>()?;
            let merged = MergedProfile::from_profiles(&loaded);

            let check = std::process::Command::new("bwrap")
                .arg("--version")
                .output()
                .map_err(|e| format!("bwrap not found: {e}"))?;
            if !check.status.success() {
                return Err("bwrap is not functional. Install bubblewrap to use sandbox profiles.".into());
            }

            Some(Arc::new(merged))
        };

        let session = Arc::new(VsockSession {
            passthrough: Arc::new(passthrough),
            env_mode,
            workspace,
            merged_profile,
        });

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

async fn daemon_loop(
    session: Arc<VsockSession>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), String> {
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

async fn handle_connection(
    stream: tokio_vsock::VsockStream,
    session: &VsockSession,
) -> Result<(), String> {
    let (mut reader, mut writer) = tokio::io::split(stream);

    let req = read_exec_request(&mut reader).await?;

    if !is_allowed(&session.passthrough, &req.command, &req.args) {
        let msg = format!("bunkerbox-vscomm: command '{}' not whitelisted\n", req.command);
        write_frame(&mut writer, &Frame::new(FrameType::Stderr, msg.into_bytes())).await?;
        write_frame(&mut writer, &Frame::new(FrameType::Exit, 1i32.to_le_bytes().to_vec())).await?;
        return Ok(());
    }

    let sandbox_cwd = req.cwd.clone();
    let host_cwd = if req.cwd.starts_with("/workspace") {
        session
            .workspace
            .join(req.cwd.strip_prefix("/workspace").unwrap_or(&req.cwd).trim_start_matches('/'))
    } else {
        PathBuf::from(&req.cwd)
    };

    let mut cmd = build_command(session, &req, &host_cwd, &sandbox_cwd)?;
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {e}", req.command))?;

    let child_stdout = child.stdout.take().ok_or_else(|| "no stdout".to_string())?;
    let child_stderr = child.stderr.take().ok_or_else(|| "no stderr".to_string())?;

    let (stdout_tx, mut stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (stderr_tx, mut stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    let stdout_task = tokio::spawn(async move { pump_to_channel(child_stdout, stdout_tx).await });
    let stderr_task = tokio::spawn(async move { pump_to_channel(child_stderr, stderr_tx).await });

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

fn build_command(session: &VsockSession, req: &ExecRequest, host_cwd: &Path, sandbox_cwd: &str) -> Result<Command, String> {
    if let Some(ref merged) = session.merged_profile {
        let mut cmd = Command::new("bwrap");

        cmd.arg("--bind").arg(&session.workspace).arg("/workspace");

        for (name, host_path) in &merged.bin {
            let resolved = if host_path.exists() {
                host_path.clone()
            } else if let Some(found) = find_in_path(name) {
                found
            } else {
                eprintln!("bunkerbox: warning: binary '{name}' not found, skipping");
                continue;
            };
            let dest = PathBuf::from("/usr/bin").join(name);
            cmd.arg("--ro-bind").arg(&resolved).arg(&dest);
        }

        for dir in &merged.ro {
            let p = Path::new(dir);
            if p.exists() {
                cmd.arg("--ro-bind").arg(dir).arg(dir);
            }
        }

        for dir in &merged.rw {
            let p = Path::new(dir);
            if p.exists() {
                cmd.arg("--bind").arg(dir).arg(dir);
            }
        }

        if let Ok(resolved) = which_sh(&merged.shell) {
            cmd.arg("--ro-bind").arg(&resolved).arg("/bin/sh");
        }

        if matches!(merged.network, NetworkMode::None) {
            cmd.arg("--unshare-net");
        }

        cmd.arg("--proc").arg("/proc");
        cmd.arg("--dev").arg("/dev");
        cmd.arg("--tmpfs").arg("/tmp");
        cmd.arg("--tmpfs").arg("/home");

        if !sandbox_cwd.is_empty() && sandbox_cwd != "/" {
            cmd.arg("--dir").arg(sandbox_cwd);
        }
        cmd.arg("--chdir").arg(sandbox_cwd);

        cmd.arg("--clearenv");
        cmd.arg("--setenv").arg("PATH").arg("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        cmd.arg("--setenv").arg("HOME").arg("/home");

        for (key, val) in &merged.env {
            cmd.arg("--setenv").arg(key).arg(val);
        }

        if session.env_mode == EnvMode::Relaxed {
            for (key, val) in &req.env {
                if key == "PATH" || key == "HOME" || key == "VSOCK_CID" || key.starts_with("BUNKERBOX_") || key.starts_with("XDG_") {
                    continue;
                }
                cmd.arg("--setenv").arg(key).arg(val);
            }
        }

        cmd.arg("--");
        cmd.arg(&req.command);
        for arg in &req.args {
            cmd.arg(arg);
        }

        Ok(cmd)
    } else {
        let mut cmd = Command::new(&req.command);
        cmd.args(&req.args);
        cmd.current_dir(host_cwd);

        if session.env_mode == EnvMode::Relaxed {
            for (key, val) in &req.env {
                if key == "PATH" || key == "HOME" || key == "VSOCK_CID" || key.starts_with("BUNKERBOX_") || key.starts_with("XDG_") {
                    continue;
                }
                cmd.env(key, val);
            }
        }

        Ok(cmd)
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).find_map(|dir| {
            let candidate = dir.join(name);
            candidate.is_file().then_some(candidate)
        })
    })
}

fn which_sh(shell: &Path) -> Result<PathBuf, String> {
    if shell.exists() {
        return Ok(shell.to_path_buf());
    }
    if let Some(name) = shell.file_name().and_then(|n| n.to_str()) {
        if let Some(found) = find_in_path(name) {
            return Ok(found);
        }
    }
    Err(format!("shell not found: {}", shell.display()))
}

fn is_allowed(passthrough: &[String], command: &str, args: &[String]) -> bool {
    for entry in passthrough {
        let entry = entry.trim();
        if let Some(cmd) = entry.strip_suffix(" *") {
            if cmd.trim() == command {
                return true;
            }
        } else {
            let full = if args.is_empty() {
                command.to_string()
            } else {
                format!("{} {}", command, args.join(" "))
            };
            if entry == full {
                return true;
            }
        }
    }
    false
}

async fn read_exec_request<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<ExecRequest, String> {
    let mut header = [0u8; 6];
    reader.read_exact(&mut header).await.map_err(|e| format!("read header: {e}"))?;

    let frame_type_raw = u16::from_le_bytes([header[0], header[1]]);
    let ft = FrameType::from_u16(frame_type_raw)
        .ok_or_else(|| format!("unknown frame type: {frame_type_raw}"))?;

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

async fn pump_to_channel<R: AsyncReadExt + Unpin>(
    mut reader: R,
    tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
) {
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
