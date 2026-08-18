use bunkerbox::cfg::{ProjectConfig, RemoteToolSpec, WorkspaceMode};
use bunkerbox::remote::{RemoteAdmissionLimits, RemoteEnvironmentPolicy, RemoteToolPolicy};
use bunkerbox::{cfg, cfgsetup, clidef, cmdrun, daemon, kata, logging, loopback, overlay, remote_target, snapshot, tui, vscomm, workspace};
use rand::RngCore;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const WORKSPACE_HANDOFF_MAGIC: &[u8; 4] = b"WS01";
const STARTUP_READY_MAGIC: &[u8; 4] = b"RDY1";
const MAX_WORKSPACE_HANDOFF_BYTES: usize = 64 * 1024;

fn main() {
    if let Err(err) = run() {
        eprintln!("bunkerbox: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let verbose = flag_present("verbose");
    let log_file = option_from_args("log")?.map(|v| v.to_string_lossy().into_owned());
    logging::configure(verbose, log_file);

    let workspace_override = workspace_mode_from_args()?;

    if cfg::RuntimeConfig::invoked_name()? != clidef::APPNAME {
        let share_dir = share_dir_from_args()?;
        if let Some(config) = cfg::RuntimeConfig::for_invoked_name(&share_dir)? {
            return tokio::runtime::Runtime::new().map_err(|e| format!("tokio: {e}"))?.block_on(async move {
                tokio::task::spawn_blocking(move || run_packaged_runtime(config, workspace_override, &share_dir))
                    .await
                    .map_err(|error| format!("packaged runtime thread failed: {error}"))?
            });
        }
    }

    let mut cli = clidef::cli(env!("CARGO_PKG_VERSION"));
    let matches = cli.clone().get_matches();

    if matches.get_flag("help") {
        cli.print_help().map_err(|err| err.to_string())?;
        println!();
        return Ok(());
    }

    if matches.get_flag("version") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    match matches.subcommand() {
        Some(("setup", submatches)) => {
            if submatches.get_flag("help") {
                print_subcommand_help("setup")?;
                return Ok(());
            }
            cmdrun::run_sequence("setup")
        }
        Some(("install-image", submatches)) => {
            if submatches.get_flag("help") {
                print_subcommand_help("install-image")?;
                return Ok(());
            }
            cmdrun::run_sequence("install-image")
        }
        Some(("prepare", submatches)) => {
            if submatches.get_flag("help") {
                print_subcommand_help("prepare")?;
                return Ok(());
            }
            workspace::prepare(submatches.get_flag("reset"))
        }
        Some(("config", submatches)) => {
            if submatches.get_flag("help") {
                print_subcommand_help("config")?;
                return Ok(());
            }
            let runtime = share_dir_from_args()
                .ok()
                .and_then(|d| cfg::RuntimeConfig::load_from_share_dir(&d))
                .or_else(|| cfg::RuntimeConfig::load_from_share_dir(Path::new(cfg::DEFAULT_SHARE_DIR)));
            cfgsetup::run(runtime.as_ref())
        }
        Some(("run", submatches)) => {
            if submatches.get_flag("help") {
                print_subcommand_help("run")?;
                return Ok(());
            }
            let name = submatches.get_one::<String>("name").ok_or_else(|| "missing sequence name".to_string())?;
            cmdrun::run_sequence(name)
        }
        Some(("list", submatches)) => {
            if submatches.get_flag("help") {
                print_subcommand_help("list")?;
                return Ok(());
            }
            list_sequences()
        }
        Some(("sync", submatches)) => {
            if submatches.get_flag("help") {
                print_subcommand_help("sync")?;
                return Ok(());
            }
            let repo_root = workspace::project_root()?;
            overlay::sync_sessions(&repo_root, None)
        }
        Some((name, _)) => Err(format!("unknown command: {name}")),
        None => {
            cli.print_help().map_err(|err| err.to_string())?;
            println!();
            Ok(())
        }
    }
}

fn share_dir_from_args() -> Result<PathBuf, String> {
    let path = match option_from_args("share")? {
        Some(value) => PathBuf::from(value),
        None => PathBuf::from(cfg::DEFAULT_SHARE_DIR),
    };
    path.canonicalize().map_err(|err| format!("failed to resolve share directory {}: {err}", path.display()))
}

fn workspace_mode_from_args() -> Result<Option<WorkspaceMode>, String> {
    option_from_args("workspace")?
        .map(|value| match value.to_string_lossy().as_ref() {
            "share" | "cow" => Ok(WorkspaceMode::Cow),
            "clone" | "isolated" => Ok(WorkspaceMode::Isolated),
            "direct" => Ok(WorkspaceMode::Direct),
            value => Err(format!("invalid --workspace value: {value}")),
        })
        .transpose()
}

fn option_from_args(name: &str) -> Result<Option<OsString>, String> {
    let long = format!("--{name}");
    let prefix = format!("--{name}=");
    let mut args = std::env::args_os().skip(1);

    while let Some(arg) = args.next() {
        if arg == long.as_str() {
            return args.next().map(Some).ok_or_else(|| format!("--{name} requires a value"));
        }

        if let Some(arg) = arg.to_str() {
            if let Some(value) = arg.strip_prefix(&prefix) {
                return Ok(Some(OsString::from(value)));
            }
        }
    }

    Ok(None)
}

fn flag_present(name: &str) -> bool {
    let long = format!("--{name}");
    let prefix = format!("--{name}=");
    for arg in std::env::args_os().skip(1) {
        if arg == long.as_str() {
            return true;
        }
        if let Some(s) = arg.to_str() {
            if s.starts_with(&prefix) {
                return true;
            }
        }
    }
    false
}

fn run_packaged_runtime(config: cfg::RuntimeConfig, workspace_override: Option<WorkspaceMode>, share_dir: &Path) -> Result<(), String> {
    if config.oci.as_os_str().is_empty() {
        return Err("runtime config missing oci".to_string());
    }

    if config.image.trim().is_empty() {
        return Err("runtime config missing image".to_string());
    }

    let workspace_mode = workspace_override.or(config.workspace).unwrap_or_default();
    let quota = config.workspace_quota_bytes();
    let exclude = config.workspace_exclude.clone();
    let name = cfg::RuntimeConfig::invoked_name()?;
    let container_name = format!("bunkerbox-{name}");

    let repo_root = workspace::project_root()?;
    let env = ProjectConfig::load_or_create(&repo_root)?;
    let remote_backend = remote_target::RemoteTargetConfig::load_default()?.resolve_for_project(&repo_root)?;
    let artifact_policy = remote_backend.artifacts().clone();
    let artifact_limits = remote_backend.target().map(|target| target.resources().artifact_limits()).unwrap_or_default();
    artifact_policy.validate_limits(artifact_limits)?;
    if let remote_target::BackendMode::Ssh = remote_backend.backend() {
        let target = remote_backend.target().ok_or_else(|| "SSH backend selection has no target".to_string())?;
        for tool in &env.project.remote.tools {
            if !target.tools().contains_key(&tool.name) {
                return Err(format!("SSH target '{}' does not configure remote tool '{}'", target.name(), tool.name));
            }
        }
    }

    let merged_allow: Vec<String> = config.allow.clone().unwrap_or_default().into_iter().chain(env.image.allow.clone().unwrap_or_default()).collect();

    let passthrough = env.project.passthrough.clone();
    let env_mode = env.project.env;
    let profiles = env.profiles.clone();
    let remote_environment = RemoteEnvironmentPolicy::from_names(env.project.remote.environment.clone())?;
    let remote_environment_names = remote_environment.allowed_names().map(str::to_string).collect::<Vec<_>>();
    let remote_tool_policies =
        env.project.remote.tools.iter().map(|tool| (tool.name.clone(), RemoteToolPolicy::new(tool.allow_args))).collect::<Vec<_>>();
    let configured_remote_tool_names = env.project.remote.tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>();
    let remote_tool_names = remote_tool_names(&env.project.remote.tools);
    let share_dir_owned = share_dir.to_path_buf();

    let mut sock_fds = [-1i32, -1];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sock_fds.as_mut_ptr()) } != 0 {
        return Err(format!("status socketpair: {}", std::io::Error::last_os_error()));
    }
    let (parent_fd, child_fd) = (sock_fds[0], sock_fds[1]);

    let mut setup_fds = [-1i32, -1];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, setup_fds.as_mut_ptr()) } != 0 {
        return Err(format!("workspace setup socketpair: {}", std::io::Error::last_os_error()));
    }
    let (setup_parent_fd, setup_child_fd) = (setup_fds[0], setup_fds[1]);

    let (cols, rows) = crossterm::terminal::size().map_err(|e| format!("terminal size: {e}"))?;

    let winsize = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };

    let mut master: RawFd = -1;
    let pid = unsafe { libc::forkpty(&mut master, std::ptr::null_mut(), std::ptr::null(), &winsize) };

    if pid == -1 {
        return Err("forkpty failed".to_string());
    }

    if pid == 0 {
        unsafe { libc::close(parent_fd) };
        unsafe { libc::close(setup_parent_fd) };

        let status_fd = child_fd;
        bunkerbox::logging::set_status_fd(status_fd);
        bunkerbox::logging::log("Starting...");

        let mut setup_child = unsafe { File::from_raw_fd(setup_child_fd) };
        if let Err(error) = ensure_sudo() {
            logging::log(&format!("Startup failed: {error}"));
            return Err(error);
        }
        write_startup_ready(&mut setup_child)?;
        let (workspace_path, remote_session) = read_run_handoff(&mut setup_child)?;
        let code = match kata::run(
            &config,
            kata::WorkspaceBinding {
                path: &workspace_path,
                remote_session,
                remote_tools: &remote_tool_names,
                remote_environment: &remote_environment_names,
            },
            &container_name,
            share_dir,
            &name,
            true,
            status_fd,
        ) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("bunkerbox: {e}");

                let msg = e.to_string();
                let payload = bunkerbox::vscomm::encode_ui_payload("popup", "info", "Error", &msg);
                let mut buf = b"@".to_vec();
                buf.extend_from_slice(&payload);
                buf.push(b'\n');
                unsafe {
                    libc::write(status_fd, buf.as_ptr() as *const libc::c_void, buf.len());
                }
                std::thread::sleep(std::time::Duration::from_millis(500));

                1
            }
        };
        unsafe { libc::close(status_fd) };
        bunkerbox::logging::log("exiting");
        unsafe { libc::exit(code) };
    }

    unsafe { libc::close(child_fd) };
    unsafe { libc::close(setup_child_fd) };

    let mut startup_fds = [-1i32, -1];
    if unsafe { libc::pipe(startup_fds.as_mut_ptr()) } != 0 {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
            libc::close(master);
        }
        return Err(format!("startup status pipe: {}", std::io::Error::last_os_error()));
    }
    let (startup_status_read, startup_status_write) = (startup_fds[0], startup_fds[1]);

    let overlay: Arc<Mutex<tui::OverlayState>> = Arc::new(Mutex::new(tui::OverlayState::new()));
    let status_listener = match start_status_listener(overlay.clone()) {
        Ok(listener) => listener,
        Err(error) => {
            unsafe {
                libc::close(startup_status_read);
                libc::close(startup_status_write);
                libc::kill(pid, libc::SIGTERM);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
                libc::close(master);
            }
            return Err(error);
        }
    };

    let setup_handle = tokio::runtime::Handle::current().clone();
    let setup_thread =
        std::thread::spawn(move || -> Result<(workspace::WorkspaceHandle, Arc<loopback::RunRemoteSession>, daemon::VsockDaemon), String> {
            let mut setup_parent = unsafe { File::from_raw_fd(setup_parent_fd) };
            let startup_status = unsafe { File::from_raw_fd(startup_status_write) };
            logging::set_status_fd(startup_status.as_raw_fd());
            let _runtime_guard = setup_handle.enter();

            if let Err(error) = read_startup_ready(&mut setup_parent) {
                logging::log(&format!("Startup failed: {error}"));
                return Err(error);
            }

            let setup_result = (|| -> Result<(workspace::WorkspaceHandle, Arc<loopback::RunRemoteSession>, daemon::VsockDaemon), String> {
                logging::log("Preparing workspace...");
                let workspace = workspace::resolve(workspace_mode, quota, exclude.as_deref(), &name)?;
                let remote_session = new_session_id();
                let target = new_target_id();
                logging::log("Preparing remote session...");
                loopback::cleanup_stale_roots(&std::env::temp_dir())?;
                let snapshot_root = std::env::temp_dir().join(format!("bunkerbox-snapshots-{}-{}", std::process::id(), remote_session.to_hex()));
                let jobs_root = std::env::temp_dir().join(format!("bunkerbox-loopback-{}-{}", std::process::id(), remote_session.to_hex()));
                let snapshot_store = snapshot::SnapshotStore::new(&snapshot_root);
                let exclusion_policy = snapshot::SnapshotExclusionPolicy::from_remote_config(&env, exclude.as_deref())?;
                let snapshot_builder = snapshot::SnapshotBuilder::new(snapshot_store.clone(), snapshot::SnapshotLimits::default(), exclusion_policy);
                let session = Arc::new(loopback::RunRemoteSession::new(
                    bunkerbox::remote::WorkspaceSessionId(remote_session.0),
                    target,
                    workspace.path().to_path_buf(),
                    snapshot_store,
                    snapshot_builder,
                    jobs_root,
                )?);
                let tools = loopback::resolve_fixed_tools(configured_remote_tool_names.clone());
                logging::log("Starting remote daemon...");
                let global_active = config.remote_max_active_builds()?;
                let target_active = remote_backend.target().map(|target| target.resources().max_active_builds()).unwrap_or(1);
                let remote_config = match remote_backend.backend() {
                    remote_target::BackendMode::Loopback => daemon::RemoteDaemonConfig::loopback(session.clone(), Vec::new(), tools),
                    remote_target::BackendMode::Ssh => {
                        let target = remote_backend.target().cloned().ok_or_else(|| "SSH backend selection has no target".to_string())?;
                        daemon::RemoteDaemonConfig::ssh(session.clone(), target)?
                    }
                };
                let daemon = daemon::VsockDaemon::start_with_remote(
                    passthrough,
                    env_mode,
                    workspace.path().to_path_buf(),
                    profiles,
                    share_dir_owned,
                    merged_allow,
                    remote_config
                        .with_artifacts(artifact_policy, artifact_limits)
                        .with_policy(remote_tool_policies, remote_environment)
                        .with_admission_limits(RemoteAdmissionLimits::new(global_active, target_active)?),
                )?;
                if let Err(error) = write_run_handoff(&mut setup_parent, workspace.path(), remote_session) {
                    tokio::runtime::Handle::current().block_on(daemon.shutdown());
                    return Err(error);
                }
                Ok((workspace, session, daemon))
            })();

            if let Err(error) = &setup_result {
                logging::log(&format!("Startup failed: {error}"));
            }
            setup_result
        });

    let tui_result = tui::event_loop(master, rows, cols, parent_fd, startup_status_read, overlay);
    let tui_error = tui_result.err();
    if tui_error.is_some() {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }

    let setup_result = match setup_thread.join() {
        Ok(result) => result,
        Err(_) => Err("workspace setup thread panicked".to_string()),
    };

    let mut status: i32 = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    unsafe { libc::close(master) };
    unsafe { libc::close(startup_status_read) };

    let (setup_state, setup_error) = match setup_result {
        Ok((workspace, remote_session, daemon)) => {
            tokio::runtime::Handle::current().block_on(daemon.shutdown());
            drop(remote_session);
            drop(workspace);
            (true, None)
        }
        Err(error) => (false, Some(error)),
    };

    tokio::runtime::Handle::current().block_on(status_listener.shutdown());

    if let Some(error) = tui_error {
        return Err(error);
    }
    if let Some(error) = setup_error {
        return Err(error);
    }
    debug_assert!(setup_state);
    if status != 0 {
        return Err(format!("child exited with status {status}"));
    }

    Ok(())
}

fn new_session_id() -> vscomm::WorkspaceSessionId {
    loop {
        let mut bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut bytes);
        if bytes != [0; 16] {
            return vscomm::WorkspaceSessionId(bytes);
        }
    }
}

fn new_target_id() -> bunkerbox::remote::RemoteTargetId {
    loop {
        let mut bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut bytes);
        if bytes != [0; 16] {
            return bunkerbox::remote::RemoteTargetId(bytes);
        }
    }
}

fn remote_tool_names(entries: &[RemoteToolSpec]) -> Vec<String> {
    entries.iter().map(|tool| tool.name.clone()).collect()
}

fn write_run_handoff(file: &mut File, path: &Path, session_id: vscomm::WorkspaceSessionId) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("workspace handoff path must be absolute".to_string());
    }
    let mut payload = path.as_os_str().as_bytes().to_vec();
    if payload.contains(&0) {
        return Err("workspace path contains NUL".to_string());
    }
    payload.push(0);
    payload.extend_from_slice(&session_id.0);
    let frame = encode_workspace_handoff(&payload)?;
    io::Write::write_all(file, &frame).map_err(|err| format!("write run handoff: {err}"))
}

fn write_startup_ready(file: &mut File) -> Result<(), String> {
    io::Write::write_all(file, STARTUP_READY_MAGIC).map_err(|err| format!("write startup readiness: {err}"))
}

fn read_startup_ready(file: &mut File) -> Result<(), String> {
    let mut ready = [0u8; STARTUP_READY_MAGIC.len()];
    io::Read::read_exact(file, &mut ready).map_err(|err| format!("read startup readiness: {err}"))?;
    if &ready != STARTUP_READY_MAGIC {
        return Err("startup readiness has an invalid type".to_string());
    }
    Ok(())
}

fn ensure_sudo() -> Result<(), String> {
    if !std::process::Command::new("sudo")
        .arg("-n")
        .arg("true")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
    {
        let pass = bunkerbox::logging::prompt_password("Sudo password", "Enter your sudo password")?;
        let mut child = std::process::Command::new("sudo")
            .arg("-S")
            .arg("-v")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|error| format!("failed to run sudo: {error}"))?;
        io::Write::write_all(child.stdin.as_mut().ok_or_else(|| "failed to open sudo stdin".to_string())?, pass.as_bytes())
            .map_err(|error| format!("failed to write sudo password: {error}"))?;
        drop(child.stdin.take());
        if !child.wait().map_err(|error| format!("sudo failed: {error}"))?.success() {
            return Err("sudo: authentication failed".to_string());
        }
    }

    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_secs(240));
        let _ = std::process::Command::new("sudo")
            .arg("-n")
            .arg("-v")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    });
    Ok(())
}

fn read_run_handoff(file: &mut File) -> Result<(PathBuf, vscomm::WorkspaceSessionId), String> {
    let payload = read_workspace_handoff(file)?;
    let bytes = payload.as_os_str().as_bytes();
    if bytes.len() < 17 || bytes[bytes.len() - 17] != 0 {
        return Err("run handoff is malformed".to_string());
    }
    let path = PathBuf::from(OsString::from_vec(bytes[..bytes.len() - 17].to_vec()));
    if !path.is_absolute() {
        return Err("run handoff path must be absolute".to_string());
    }
    let mut session = [0u8; 16];
    session.copy_from_slice(&bytes[bytes.len() - 16..]);
    if session == [0; 16] {
        return Err("run handoff session is zero".to_string());
    }
    Ok((path, vscomm::WorkspaceSessionId(session)))
}

fn read_workspace_handoff(file: &mut File) -> Result<PathBuf, String> {
    let mut header = [0u8; 8];
    io::Read::read_exact(file, &mut header).map_err(|err| format!("read workspace handoff header: {err}"))?;

    let payload_len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if payload_len > MAX_WORKSPACE_HANDOFF_BYTES {
        return Err(format!("workspace handoff is too large: {payload_len} bytes"));
    }

    let mut payload = vec![0u8; payload_len];
    io::Read::read_exact(file, &mut payload).map_err(|err| format!("read workspace handoff: {err}"))?;

    let mut frame = header.to_vec();
    frame.extend_from_slice(&payload);
    decode_workspace_handoff(&frame)
}

fn encode_workspace_handoff(path: &[u8]) -> Result<Vec<u8>, String> {
    if path.len() > MAX_WORKSPACE_HANDOFF_BYTES {
        return Err(format!("workspace handoff is too large: {} bytes", path.len()));
    }

    let length = u32::try_from(path.len()).map_err(|_| "workspace handoff length overflow".to_string())?;
    let mut frame = Vec::with_capacity(8 + path.len());
    frame.extend_from_slice(WORKSPACE_HANDOFF_MAGIC);
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(path);
    Ok(frame)
}

fn decode_workspace_handoff(frame: &[u8]) -> Result<PathBuf, String> {
    if frame.len() < 8 {
        return Err("workspace handoff is truncated".to_string());
    }
    if &frame[..4] != WORKSPACE_HANDOFF_MAGIC {
        return Err("workspace handoff has an invalid type".to_string());
    }

    let payload_len = u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]) as usize;
    if payload_len > MAX_WORKSPACE_HANDOFF_BYTES {
        return Err(format!("workspace handoff is too large: {payload_len} bytes"));
    }
    if frame.len() != 8 + payload_len {
        return Err("workspace handoff length does not match payload".to_string());
    }

    Ok(PathBuf::from(OsString::from_vec(frame[8..].to_vec())))
}

struct StatusListener {
    join_handle: tokio::task::JoinHandle<()>,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

impl StatusListener {
    async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.join_handle.await;
    }
}

fn start_status_listener(overlay: Arc<Mutex<tui::OverlayState>>) -> Result<StatusListener, String> {
    let listener = tokio_vsock::VsockListener::bind(tokio_vsock::VsockAddr::new(libc::VMADDR_CID_ANY, vscomm::TUI_STATUS_PORT))
        .map_err(|e| format!("failed to bind TUI status vsock port {}: {e}", vscomm::TUI_STATUS_PORT))?;
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let join_handle = tokio::spawn(async move {
        status_listener(listener, overlay, shutdown_rx).await;
    });
    Ok(StatusListener { join_handle, shutdown })
}

async fn status_listener(
    listener: tokio_vsock::VsockListener, overlay: Arc<Mutex<tui::OverlayState>>, mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    loop {
        let (mut stream, _peer) = tokio::select! {
            result = listener.accept() => match result {
                Ok(c) => c,
                Err(_) => continue,
            },
            _ = &mut shutdown_rx => break,
        };

        let overlay = overlay.clone();
        tokio::spawn(async move {
            let Ok(frame) = vscomm::Frame::read_async(&mut stream).await else {
                return;
            };
            if !matches!(frame.frame_type, vscomm::FrameType::UiCommand) {
                return;
            }

            if let Some((widget, cmd, opts, val)) = vscomm::decode_ui_payload(&frame.payload) {
                if widget == "error" && cmd == "show" {
                    let title = if opts.is_empty() { "Bunkerbox error" } else { opts };
                    logging::diagnostic(&format!("TUI error [{title}]: {val}"));
                }
                tui::dispatch_ui_command(&mut overlay.lock().unwrap(), widget, cmd, opts, val);
            }
        });
    }
}

fn print_subcommand_help(name: &str) -> Result<(), String> {
    let mut cli = clidef::cli(env!("CARGO_PKG_VERSION"));
    let subcommand = cli.find_subcommand_mut(name).ok_or_else(|| format!("unknown command: {name}"))?;

    subcommand.print_help().map_err(|err| err.to_string())?;
    println!();
    Ok(())
}

fn list_sequences() -> Result<(), String> {
    for name in cmdrun::sequence_names()? {
        println!("{name}");
    }

    Ok(())
}

#[cfg(test)]
#[path = "main_ut.rs"]
mod main_tests;
