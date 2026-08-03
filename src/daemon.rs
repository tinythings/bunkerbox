use crate::cfg::EnvMode;
use crate::logging;
use crate::loopback::{LoopbackBackend, RunRemoteSession};
use crate::proxy::{FilterProxy, UnixProxyHandle};
use crate::remote::{
    RemoteAuthorizationError, RemoteAuthorizationPolicy, RemoteBackend, RemoteBackendError, RemoteBackendEvent, RemoteEnvironmentPolicy,
    RemoteExecutionContext, RemoteRequest, RemoteResourcePolicy, RemoteToolPolicy,
};
use crate::sandbox::{resolve_profile, MergedProfile, NetworkMode};
use crate::vscomm::{validate_exec_request, validate_process_path, ExecRequest, Frame, FrameType, TOOLCHAIN_PORT};
use crate::workspace::WorkspaceCwd;
use rand::Rng;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const BWRAP_STATUS_FD: RawFd = 3;

#[derive(Clone, Copy)]
enum ProcessStream {
    Stdout,
    Stderr,
}

enum ChildEvent {
    Output(ProcessStream, Vec<u8>),
    StreamClosed,
    LauncherStarted,
    LauncherFailed(String),
}

#[derive(Debug)]
struct SandboxProxyConfig {
    socket_path: PathBuf,
    netrelay_path: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RemoteDispatchError {
    Unauthorized(RemoteAuthorizationError),
    Backend(RemoteBackendError),
    EventSinkClosed,
}

pub struct RemoteBroker {
    policy: RemoteAuthorizationPolicy,
    context: RemoteExecutionContext,
    backend: Arc<dyn RemoteBackend>,
}

impl RemoteBroker {
    pub fn new(policy: RemoteAuthorizationPolicy, context: RemoteExecutionContext, backend: Arc<dyn RemoteBackend>) -> Self {
        Self { policy, context, backend }
    }

    pub async fn dispatch(&self, request: RemoteRequest, events: tokio::sync::mpsc::Sender<RemoteBackendEvent>) -> Result<(), RemoteDispatchError> {
        let authorized = match self.policy.authorize(&self.context, request) {
            Ok(authorized) => authorized,
            Err(error) => {
                events.send(error.event()).await.map_err(|_| RemoteDispatchError::EventSinkClosed)?;
                return Err(RemoteDispatchError::Unauthorized(error));
            }
        };
        match self.backend.execute(authorized, events.clone()).await {
            Ok(()) => Ok(()),
            Err(error) => {
                events.send(error.event()).await.map_err(|_| RemoteDispatchError::EventSinkClosed)?;
                Err(RemoteDispatchError::Backend(error))
            }
        }
    }
}

struct VsockSession {
    passthrough: Arc<Vec<String>>,
    env_mode: EnvMode,
    workspace: PathBuf,
    merged_profile: Option<Arc<MergedProfile>>,
    proxy_config: Option<Arc<SandboxProxyConfig>>,
    remote_broker: Arc<RemoteBroker>,
}

pub struct VsockDaemon {
    join_handle: tokio::task::JoinHandle<()>,
    shutdown: tokio::sync::oneshot::Sender<()>,
    sandbox_proxy: Option<UnixProxyHandle>,
    sandbox_proxy_dir: Option<PathBuf>,
}

pub struct RemoteDaemonConfig {
    session: Arc<RunRemoteSession>,
    allowed_tools: Vec<String>,
    tool_policies: Option<Vec<(String, RemoteToolPolicy)>>,
    environment: Option<RemoteEnvironmentPolicy>,
    tools: std::collections::BTreeMap<String, PathBuf>,
    target_environment: std::collections::BTreeMap<String, String>,
    resources: RemoteResourcePolicy,
}

impl RemoteDaemonConfig {
    pub fn new(session: Arc<RunRemoteSession>, allowed_tools: Vec<String>, tools: std::collections::BTreeMap<String, PathBuf>) -> Self {
        Self {
            session,
            allowed_tools,
            tool_policies: None,
            environment: None,
            tools,
            target_environment: std::collections::BTreeMap::new(),
            resources: RemoteResourcePolicy::default(),
        }
    }

    pub fn with_policy(mut self, tools: Vec<(String, RemoteToolPolicy)>, environment: RemoteEnvironmentPolicy) -> Self {
        self.tool_policies = Some(tools);
        self.environment = Some(environment);
        self
    }

    pub fn with_target_environment(mut self, environment: std::collections::BTreeMap<String, String>) -> Self {
        self.target_environment = environment;
        self
    }

    pub fn with_resources(mut self, resources: RemoteResourcePolicy) -> Self {
        self.resources = resources;
        self
    }
}

struct RemoteComponents {
    context: RemoteExecutionContext,
    policy: RemoteAuthorizationPolicy,
    backend: Arc<dyn RemoteBackend>,
}

impl VsockDaemon {
    pub fn start_with_remote(
        passthrough: Vec<String>, env_mode: EnvMode, workspace: PathBuf, profiles: Vec<String>, share_dir: PathBuf, allow: Vec<String>,
        remote: RemoteDaemonConfig,
    ) -> Result<Self, String> {
        let RemoteDaemonConfig { session, allowed_tools, tool_policies, environment, tools, target_environment, resources } = remote;
        let remote_policy = match (tool_policies, environment) {
            (Some(tool_policies), Some(environment)) => {
                RemoteAuthorizationPolicy::from_policies(session.target(), session.session_id(), tool_policies, environment)?
            }
            (None, None) => RemoteAuthorizationPolicy::new(session.target(), session.session_id(), allowed_tools),
            _ => return Err("remote tool and environment policies must be configured together".to_string()),
        }
        .with_snapshot_authority(session.clone());
        let remote_context = RemoteExecutionContext { target: session.target(), workspace_session_id: session.session_id() };
        let backend = LoopbackBackend::new(session, tools)
            .with_target_environment(target_environment)
            .with_timeout(resources.build_timeout)
            .with_output_limit(resources.max_output_bytes);
        let remote_components = RemoteComponents { context: remote_context, policy: remote_policy, backend: Arc::new(backend) };
        Self::start_inner(passthrough, env_mode, workspace, profiles, share_dir, allow, remote_components)
    }

    fn start_inner(
        passthrough: Vec<String>, env_mode: EnvMode, workspace: PathBuf, profiles: Vec<String>, share_dir: PathBuf, allow: Vec<String>,
        remote: RemoteComponents,
    ) -> Result<Self, String> {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let merged_profile = if profiles.is_empty() {
            None
        } else {
            let loaded: Vec<_> = profiles.iter().map(|p| resolve_profile(p, &share_dir)).collect::<Result<Vec<_>, _>>()?;
            let host_home = std::env::var_os("HOME").map(PathBuf::from);
            let merged = MergedProfile::from_profiles(&loaded, host_home.as_deref())?;

            let check = std::process::Command::new("bwrap").arg("--version").output().map_err(|e| format!("bwrap not found: {e}"))?;
            if !check.status.success() {
                return Err("bwrap is not functional. Install bubblewrap to use sandbox profiles.".into());
            }

            Some(Arc::new(merged))
        };

        let mut sandbox_proxy: Option<UnixProxyHandle> = None;
        let mut sandbox_proxy_dir: Option<PathBuf> = None;
        let mut proxy_config: Option<SandboxProxyConfig> = None;

        if merged_profile.is_some() && !allow.is_empty() {
            let rt = tokio::runtime::Handle::current();

            let netrelay_path = find_netrelay_binary()?;

            let dir = make_proxy_runtime_dir()?;
            sandbox_proxy_dir = Some(dir.clone());

            let socket_path = dir.join("proxy.sock");
            sandbox_proxy = Some(rt.block_on(FilterProxy::new(allow).bind_unix(&socket_path)).inspect_err(|_| {
                let _ = std::fs::remove_dir(&dir);
            })?);

            proxy_config = Some(SandboxProxyConfig { socket_path, netrelay_path });
        }

        let connections = Arc::new(Mutex::new(Vec::new()));
        let remote_broker = Arc::new(RemoteBroker::new(remote.policy, remote.context, remote.backend));
        let session = Arc::new(VsockSession {
            passthrough: Arc::new(passthrough),
            env_mode,
            workspace,
            merged_profile,
            proxy_config: proxy_config.map(Arc::new),
            remote_broker,
        });

        let listener = tokio_vsock::VsockListener::bind(tokio_vsock::VsockAddr::new(libc::VMADDR_CID_ANY, TOOLCHAIN_PORT)).map_err(|error| {
            if let Some(h) = sandbox_proxy.take() {
                h.stop();
            }
            if let Some(d) = sandbox_proxy_dir.take() {
                let _ = std::fs::remove_dir_all(&d);
            }
            format!("failed to bind toolchain vsock port {TOOLCHAIN_PORT}: {error}")
        })?;

        let join_handle = tokio::spawn(async move {
            let result = daemon_loop(session, listener, shutdown_rx, connections).await;
            if let Err(err) = result {
                logging::diagnostic(&format!("bunkerbox: vsock daemon: {err}"));
            }
        });

        Ok(Self { join_handle, shutdown: shutdown_tx, sandbox_proxy, sandbox_proxy_dir })
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.join_handle.await;
        if let Some(h) = self.sandbox_proxy {
            h.stop();
        }
        if let Some(d) = self.sandbox_proxy_dir {
            let _ = std::fs::remove_dir_all(&d);
        }
    }
}

async fn daemon_loop(
    session: Arc<VsockSession>, listener: tokio_vsock::VsockListener, mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    connections: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
) -> Result<(), String> {
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _peer)) => {
                        let session = session.clone();
                        let connection = tokio::spawn(async move {
                            if let Err(err) = handle_connection(stream, &session).await {
                                logging::diagnostic(&format!("bunkerbox: toolchain vsock session failed: {err}"));
                            }
                        });
                        let mut active = connections.lock().map_err(|_| "connection task lock poisoned".to_string())?;
                        active.retain(|task| !task.is_finished());
                        active.push(connection);
                    }
                    Err(e) => {
                        logging::diagnostic(&format!("bunkerbox: vsock accept error: {e}"));
                    }
                }
            }
            _ = &mut shutdown_rx => {
                break;
            }
        }
    }

    let tasks = connections.lock().map_err(|_| "connection task lock poisoned".to_string())?.drain(..).collect::<Vec<_>>();
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        let _ = task.await;
    }

    Ok(())
}

async fn handle_connection(stream: tokio_vsock::VsockStream, session: &VsockSession) -> Result<(), String> {
    let (mut reader, mut writer) = tokio::io::split(stream);

    let frame = Frame::read_async(&mut reader).await.map_err(|e| format!("read frame: {e}"))?;
    if matches!(frame.frame_type, FrameType::RemoteRequest) {
        return dispatch_remote_frame(frame, &session.remote_broker, &mut writer).await;
    }
    if !matches!(frame.frame_type, FrameType::ExecReq) {
        return Err(format!("expected ExecReq or RemoteRequest, got {:?}", frame.frame_type as u16));
    }
    let req = ExecRequest::deserialize(&frame.payload)?;

    if let Err(err) = validate_exec_request(&req) {
        logging::diagnostic(&format!("bunkerbox-vscomm: invalid request: {err}"));
        write_frame(&mut writer, &Frame::new(FrameType::Exit, 1i32.to_le_bytes().to_vec())).await?;
        return Ok(());
    }

    if !is_allowed(&session.passthrough, &req.command, &req.args) {
        logging::diagnostic(&format!("bunkerbox-vscomm: command '{}' not whitelisted", req.command));
        write_frame(&mut writer, &Frame::new(FrameType::Exit, 1i32.to_le_bytes().to_vec())).await?;
        return Ok(());
    }

    let command = req.command.clone();
    if let Err(err) = execute_request(&mut writer, session, &req).await {
        logging::diagnostic(&format!("bunkerbox: toolchain command '{command}' failed: {err}"));
        let _ = write_frame(&mut writer, &Frame::new(FrameType::Exit, 1i32.to_le_bytes().to_vec())).await;
        return Ok(());
    }

    Ok(())
}

pub async fn dispatch_remote_frame<W: AsyncWriteExt + Unpin>(frame: Frame, broker: &RemoteBroker, writer: &mut W) -> Result<(), String> {
    let request = crate::vscomm::RemoteRequest::from_frame(frame).map_err(|err| format!("decode remote request: {err}"))?.into_domain()?;
    let request_id = request.request_id();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(64);
    let mut dispatch = Box::pin(broker.dispatch(request, event_tx));
    let mut dispatch_result: Option<Result<(), RemoteDispatchError>> = None;

    loop {
        if dispatch_result.is_some() {
            let Some(event) = event_rx.recv().await else {
                let result = dispatch_result.take().expect("dispatch result is present");
                return result.map_err(|err| format!("remote dispatch failed: {err:?}"));
            };
            let response =
                crate::vscomm::RemoteEvent::from_backend_event(request_id, event).to_frame().map_err(|err| format!("encode remote event: {err}"))?;
            write_frame(writer, &response).await?;
            continue;
        }

        tokio::select! {
            result = &mut dispatch => dispatch_result = Some(result),
            event = event_rx.recv() => {
                let Some(event) = event else { return Err("remote event stream closed before backend completion".to_string()) };
                let response = crate::vscomm::RemoteEvent::from_backend_event(request_id, event)
                    .to_frame()
                    .map_err(|err| format!("encode remote event: {err}"))?;
                write_frame(writer, &response).await?;
            }
        }
    }
}

async fn execute_request<W: AsyncWriteExt + Unpin>(writer: &mut W, session: &VsockSession, req: &ExecRequest) -> Result<(), String> {
    validate_exec_request(req)?;
    let cwd = WorkspaceCwd::resolve(&session.workspace, Path::new(&req.cwd))?;

    let (status_reader, status_writer) = if session.merged_profile.is_some() {
        let (reader, writer) = bwrap_status_pipe()?;
        (Some(reader), Some(writer))
    } else {
        (None, None)
    };
    let mut cmd = build_command(session, req, &cwd)?;
    if let Some(status_writer) = status_writer.as_ref() {
        attach_bwrap_status_fd(&mut cmd, status_writer.as_raw_fd());
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let launcher = if session.merged_profile.is_some() { "bwrap" } else { &req.command };
    let mut child = cmd.spawn().map_err(|e| format!("spawn {launcher} for command '{}': {e}", req.command))?;
    drop(status_writer);

    let child_stdout = child.stdout.take().ok_or_else(|| "no stdout".to_string())?;
    let child_stderr = child.stderr.take().ok_or_else(|| "no stderr".to_string())?;

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(pump_to_channel(child_stdout, ProcessStream::Stdout, event_tx.clone()));
    let stderr_task = tokio::spawn(pump_to_channel(child_stderr, ProcessStream::Stderr, event_tx.clone()));
    let status_task = status_reader.map(|reader| {
        let status_tx = event_tx.clone();
        tokio::task::spawn_blocking(move || monitor_bwrap_status(reader, status_tx))
    });
    drop(event_tx);
    let mut launcher_started = session.merged_profile.is_none();
    let mut launcher_failed = false;
    let mut closed_streams = 0;
    let mut buffered_output = Vec::new();

    while closed_streams < 2 || (session.merged_profile.is_some() && !launcher_started && !launcher_failed) {
        let Some(event) = event_rx.recv().await else { break };
        match event {
            ChildEvent::Output(stream_kind, data) if launcher_failed => {
                let stream = if matches!(stream_kind, ProcessStream::Stdout) { "bwrap stdout" } else { "bwrap stderr" };
                logging::diagnostic_bytes(stream, &data);
            }
            ChildEvent::Output(stream_kind, data) if launcher_started => {
                write_frame(writer, &Frame::new(process_stream_frame_type(stream_kind), data)).await?;
            }
            ChildEvent::Output(stream_kind, data) => buffered_output.push((stream_kind, data)),
            ChildEvent::StreamClosed => closed_streams += 1,
            ChildEvent::LauncherStarted => {
                launcher_started = true;
                for (stream_kind, data) in buffered_output.drain(..) {
                    write_frame(writer, &Frame::new(process_stream_frame_type(stream_kind), data)).await?;
                }
            }
            ChildEvent::LauncherFailed(err) => {
                launcher_failed = true;
                logging::diagnostic(&format!("bwrap setup failed: {err}"));
                for (stream_kind, data) in buffered_output.drain(..) {
                    let stream = if matches!(stream_kind, ProcessStream::Stdout) { "bwrap stdout" } else { "bwrap stderr" };
                    logging::diagnostic_bytes(stream, &data);
                }
            }
        }
    }

    let status = child.wait().await.map_err(|e| format!("wait {}: {e}", req.command))?;
    let exit_code = status.code().unwrap_or(-1);
    write_frame(writer, &Frame::new(FrameType::Exit, exit_code.to_le_bytes().to_vec())).await?;

    stdout_task.await.map_err(|e| format!("stdout task: {e}"))?;
    stderr_task.await.map_err(|e| format!("stderr task: {e}"))?;
    if let Some(status_task) = status_task {
        status_task.await.map_err(|e| format!("bwrap status task: {e}"))?;
    }

    Ok(())
}

fn build_command(session: &VsockSession, req: &ExecRequest, cwd: &WorkspaceCwd) -> Result<Command, String> {
    validate_exec_request(req)?;
    validate_process_path("workspace path", &session.workspace)?;
    validate_process_path("host working directory", cwd.host_path())?;
    let sandbox_cwd = cwd.guest_path();
    validate_process_path("sandbox working directory", &sandbox_cwd)?;
    if let Some(ref merged) = session.merged_profile {
        let mut cmd = Command::new("bwrap");

        cmd.arg("--bind").arg(&session.workspace).arg("/workspace");

        for (name, host_path) in &merged.bin {
            let resolved = if host_path.exists() {
                host_path.clone()
            } else if let Some(found) = find_in_path(name) {
                found
            } else {
                logging::diagnostic(&format!("bunkerbox: warning: binary '{name}' not found, skipping"));
                continue;
            };
            let dest = PathBuf::from("/usr/bin").join(name);
            cmd.arg("--ro-bind").arg(&resolved).arg(&dest);
        }

        cmd.arg("--tmpfs").arg("/home");
        for path in &merged.paths {
            if !path.source.exists() {
                logging::diagnostic(&format!("bunkerbox: warning: profile path '{}' not found, skipping", path.source.display()));
                continue;
            }
            if path.writable {
                cmd.arg("--bind").arg(&path.source).arg(&path.destination);
            } else {
                cmd.arg("--ro-bind").arg(&path.source).arg(&path.destination);
            }
        }

        if let Ok(resolved) = which_sh(&merged.shell) {
            cmd.arg("--ro-bind").arg(&resolved).arg("/bin/sh");
        }

        if matches!(merged.network, NetworkMode::None) {
            cmd.arg("--unshare-net");
        }

        cmd.arg("--unshare-pid");
        cmd.arg("--unshare-user");
        cmd.arg("--uid").arg("0");
        cmd.arg("--gid").arg("0");

        cmd.arg("--proc").arg("/proc");
        cmd.arg("--dev").arg("/dev");
        cmd.arg("--tmpfs").arg("/tmp");

        if sandbox_cwd != Path::new("/") {
            cmd.arg("--dir").arg(&sandbox_cwd);
        }
        cmd.arg("--chdir").arg(&sandbox_cwd);

        cmd.arg("--clearenv");
        cmd.arg("--setenv").arg("PATH").arg("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        cmd.arg("--setenv").arg("HOME").arg("/home");

        if let Some(ref cfg) = session.proxy_config {
            cmd.arg("--dir").arg("/run/bunkerbox");
            cmd.arg("--ro-bind").arg(&cfg.netrelay_path).arg("/run/bunkerbox/netrelay");
            cmd.arg("--ro-bind").arg(&cfg.socket_path).arg("/run/bunkerbox/proxy.sock");
        }

        if session.proxy_config.is_some() {
            let proxy_url = "http://127.0.0.1:20000";
            cmd.arg("--setenv").arg("HTTP_PROXY").arg(proxy_url);
            cmd.arg("--setenv").arg("HTTPS_PROXY").arg(proxy_url);
            cmd.arg("--setenv").arg("http_proxy").arg(proxy_url);
            cmd.arg("--setenv").arg("https_proxy").arg(proxy_url);
        }

        for (key, val) in &merged.env {
            cmd.arg("--setenv").arg(key).arg(val);
        }

        if session.env_mode == EnvMode::Relaxed {
            for (key, val) in &req.env {
                if key == "PATH"
                    || key == "HOME"
                    || key == "VSOCK_CID"
                    || key == "HTTP_PROXY"
                    || key == "HTTPS_PROXY"
                    || key == "http_proxy"
                    || key == "https_proxy"
                    || key.starts_with("BUNKERBOX_")
                    || key.starts_with("XDG_")
                {
                    continue;
                }
                cmd.arg("--setenv").arg(key).arg(val);
            }
        }

        cmd.arg("--json-status-fd").arg(BWRAP_STATUS_FD.to_string());
        cmd.arg("--");
        if session.proxy_config.is_some() {
            cmd.arg("/run/bunkerbox/netrelay");
            cmd.arg("--socket");
            cmd.arg("/run/bunkerbox/proxy.sock");
            cmd.arg("--");
        }
        cmd.arg(&req.command);
        for arg in &req.args {
            cmd.arg(arg);
        }

        Ok(cmd)
    } else {
        let mut cmd = Command::new(&req.command);
        cmd.args(&req.args);
        cmd.current_dir(cwd.host_path());

        if session.env_mode == EnvMode::Relaxed {
            for (key, val) in &req.env {
                if key == "PATH"
                    || key == "HOME"
                    || key == "VSOCK_CID"
                    || key == "HTTP_PROXY"
                    || key == "HTTPS_PROXY"
                    || key == "http_proxy"
                    || key == "https_proxy"
                    || key.starts_with("BUNKERBOX_")
                    || key.starts_with("XDG_")
                {
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
            let full = if args.is_empty() { command.to_string() } else { format!("{} {}", command, args.join(" ")) };
            if entry == full {
                return true;
            }
        }
    }
    false
}

fn bwrap_status_pipe() -> Result<(File, File), String> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("create bwrap status pipe: {}", std::io::Error::last_os_error()));
    }

    let reader = unsafe { File::from_raw_fd(fds[0]) };
    let writer = unsafe { File::from_raw_fd(fds[1]) };
    Ok((reader, writer))
}

fn attach_bwrap_status_fd(cmd: &mut Command, source_fd: RawFd) {
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(source_fd, BWRAP_STATUS_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if source_fd != BWRAP_STATUS_FD {
                libc::close(source_fd);
            }
            Ok(())
        });
    }
}

fn monitor_bwrap_status(reader: File, tx: tokio::sync::mpsc::UnboundedSender<ChildEvent>) {
    let mut started = false;
    for line in BufReader::new(reader).lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                if !started {
                    let _ = tx.send(ChildEvent::LauncherFailed(format!("read status: {err}")));
                }
                return;
            }
        };

        let status: serde_json::Value = match serde_json::from_str(&line) {
            Ok(status) => status,
            Err(err) => {
                if !started {
                    let _ = tx.send(ChildEvent::LauncherFailed(format!("invalid status JSON: {err}")));
                }
                return;
            }
        };

        if status.get("child-pid").is_some() && !started {
            started = true;
            let _ = tx.send(ChildEvent::LauncherStarted);
        } else if !started {
            if let Some(exit_code) = status.get("exit-code") {
                let _ = tx.send(ChildEvent::LauncherFailed(format!("exited before command start with status {exit_code}")));
                return;
            }
        }
    }

    if !started {
        let _ = tx.send(ChildEvent::LauncherFailed("exited before command start".to_string()));
    }
}

async fn pump_to_channel<R: AsyncReadExt + Unpin>(mut reader: R, stream_kind: ProcessStream, tx: tokio::sync::mpsc::UnboundedSender<ChildEvent>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(ChildEvent::Output(stream_kind, buf[..n].to_vec())).is_err() {
                    return;
                }
            }
            Err(_) => break,
        }
    }
    let _ = tx.send(ChildEvent::StreamClosed);
}

fn process_stream_frame_type(stream: ProcessStream) -> FrameType {
    match stream {
        ProcessStream::Stdout => FrameType::Stdout,
        ProcessStream::Stderr => FrameType::Stderr,
    }
}

async fn write_frame<W: AsyncWriteExt + Unpin>(writer: &mut W, frame: &Frame) -> Result<(), String> {
    frame.write_async(writer).await.map_err(|e| format!("write frame: {e}"))
}

fn find_netrelay_binary() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("locate self: {e}"))?;
    let dir = exe.parent().ok_or("no binary directory")?;
    let sibling = dir.join("bunkerbox-netrelay");
    if sibling.is_file() {
        return Ok(sibling);
    }
    Err("bunkerbox-netrelay not found. Run: make dev".into())
}

fn make_proxy_runtime_dir() -> Result<PathBuf, String> {
    let mut rng = rand::thread_rng();
    let base = std::env::temp_dir();
    for _ in 0..10 {
        let random: u32 = rng.gen();
        let path = base.join(format!("bunkerbox-daemon-{}-{:08x}", std::process::id(), random));
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("mkdir {}: {e}", path.display())),
        }
    }
    Err("failed to create exclusive proxy runtime directory after 10 attempts".to_string())
}

#[cfg(test)]
#[path = "daemon_ut.rs"]
mod daemon_tests;
