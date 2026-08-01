use bunkerbox::cfg::{ProjectConfig, WorkspaceMode};
use bunkerbox::{cfg, cfgsetup, clidef, cmdrun, daemon, kata, logging, overlay, tui, vscomm, workspace};
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const WORKSPACE_HANDOFF_MAGIC: &[u8; 4] = b"WS01";
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
            let rt = tokio::runtime::Runtime::new().map_err(|e| format!("tokio: {e}"))?;
            let _guard = rt.enter();
            return run_packaged_runtime(config, workspace_override, &share_dir);
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

    let merged_allow: Vec<String> = config.allow.clone().unwrap_or_default().into_iter().chain(env.image.allow.clone().unwrap_or_default()).collect();

    let passthrough = env.project.passthrough.clone();
    let env_mode = env.project.env;
    let profiles = env.profiles.clone();
    let share_dir_owned = share_dir.to_path_buf();
    let daemon_holder: Arc<Mutex<Option<daemon::VsockDaemon>>> = Arc::new(Mutex::new(None));

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

        if !std::process::Command::new("sudo")
            .arg("-n")
            .arg("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
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
                .map_err(|e| format!("failed to run sudo: {e}"))?;

            use std::io::Write;
            child.stdin.as_mut().unwrap().write_all(pass.as_bytes()).map_err(|e| format!("failed to write sudo password: {e}"))?;
            drop(child.stdin.take());

            let status = child.wait().map_err(|e| format!("sudo failed: {e}"))?;
            if !status.success() {
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

        let ws = workspace::resolve(workspace_mode, quota, exclude.as_deref(), &name)?;
        write_workspace_handoff(setup_child_fd, ws.path())?;

        let vsock_enabled = !passthrough.is_empty();
        let code = match kata::run(&config, ws, &container_name, share_dir, &name, vsock_enabled, status_fd) {
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

    let overlay: Arc<Mutex<tui::OverlayState>> = Arc::new(Mutex::new(tui::OverlayState::new()));
    let status_listener = start_status_listener(overlay.clone())?;

    let setup_handle = tokio::runtime::Handle::current().clone();
    let daemon_slot = daemon_holder.clone();
    let setup_thread = std::thread::spawn(move || -> Result<(), String> {
        let workspace = read_workspace_handoff(setup_parent_fd)?;
        if passthrough.is_empty() {
            return Ok(());
        }

        let _guard = setup_handle.enter();
        let daemon = daemon::VsockDaemon::start(passthrough, env_mode, workspace, profiles, share_dir_owned, merged_allow)?;
        *daemon_slot.lock().map_err(|_| "daemon state lock poisoned".to_string())? = Some(daemon);
        Ok(())
    });

    let tui_result = tui::event_loop(master, rows, cols, parent_fd, overlay);

    let mut status: i32 = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    unsafe { libc::close(master) };

    let setup_result = match setup_thread.join() {
        Ok(result) => result,
        Err(_) => Err("workspace setup thread panicked".to_string()),
    };

    if let Some(d) = daemon_holder.lock().map_err(|_| "daemon state lock poisoned".to_string())?.take() {
        tokio::runtime::Handle::current().block_on(d.shutdown());
    }

    tokio::runtime::Handle::current().block_on(status_listener.shutdown());

    tui_result?;
    setup_result?;

    if status != 0 {
        return Err(format!("child exited with status {status}"));
    }

    Ok(())
}

fn write_workspace_handoff(fd: RawFd, path: &Path) -> Result<(), String> {
    let bytes = path.as_os_str().as_bytes();
    let frame = encode_workspace_handoff(bytes)?;
    let mut file = unsafe { File::from_raw_fd(fd) };
    io::Write::write_all(&mut file, &frame).map_err(|err| format!("write workspace handoff: {err}"))
}

fn read_workspace_handoff(fd: RawFd) -> Result<PathBuf, String> {
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut header = [0u8; 8];
    io::Read::read_exact(&mut file, &mut header).map_err(|err| format!("read workspace handoff header: {err}"))?;

    let payload_len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if payload_len > MAX_WORKSPACE_HANDOFF_BYTES {
        return Err(format!("workspace handoff is too large: {payload_len} bytes"));
    }

    let mut payload = vec![0u8; payload_len];
    io::Read::read_exact(&mut file, &mut payload).map_err(|err| format!("read workspace handoff: {err}"))?;

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
