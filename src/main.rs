use bunkerbox::cfg::{ProjectConfig, WorkspaceMode};
use bunkerbox::{cfg, cfgsetup, clidef, cmdrun, daemon, kata, logging, overlay, tui, vscomm, workspace};
use std::cell::RefCell;
use std::ffi::OsString;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

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
    let daemon_holder = Rc::new(RefCell::new(None));
    let daemon_clone = daemon_holder.clone();

    let mut sock_fds = [-1i32, -1];
    unsafe {
        libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sock_fds.as_mut_ptr());
    }
    let (parent_fd, child_fd) = (sock_fds[0], sock_fds[1]);

    let (cols, rows) = crossterm::terminal::size().map_err(|e| format!("terminal size: {e}"))?;

    let winsize = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };

    let mut master: RawFd = -1;
    let pid = unsafe { libc::forkpty(&mut master, std::ptr::null_mut(), std::ptr::null(), &winsize) };

    if pid == -1 {
        return Err("forkpty failed".to_string());
    }

    if pid == 0 {
        unsafe { libc::close(parent_fd) };

        let status_fd = child_fd;
        bunkerbox::logging::set_status_fd(status_fd);
        bunkerbox::logging::log("Starting...");

        if !std::process::Command::new("sudo")
            .arg("-n")
            .arg("-v")
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
        let wp = ws.path().to_path_buf();
        {
            let mut msg = wp.to_string_lossy().into_owned().into_bytes();
            msg.push(b'\n');
            unsafe {
                libc::write(status_fd, msg.as_ptr() as *const libc::c_void, msg.len());
            }
        }

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

    let overlay: Arc<Mutex<tui::OverlayState>> = Arc::new(Mutex::new(tui::OverlayState::new()));
    let overlay_clone = overlay.clone();

    let tui_result = tui::event_loop(
        master,
        rows,
        cols,
        parent_fd,
        move |path_bytes: Vec<u8>| {
            let wp = PathBuf::from(String::from_utf8_lossy(&path_bytes).into_owned());
            if !passthrough.is_empty() {
                *daemon_clone.borrow_mut() = Some(daemon::VsockDaemon::start(passthrough, env_mode, wp, profiles, share_dir_owned, merged_allow)?);
            }

            let overlay = overlay_clone;
            tokio::spawn(async move {
                status_listener(overlay).await;
            });

            Ok(())
        },
        overlay,
    );

    let mut status: i32 = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    unsafe { libc::close(master) };

    if let Some(d) = daemon_holder.borrow_mut().take() {
        tokio::runtime::Handle::current().block_on(d.shutdown());
    }

    tui_result?;

    if status != 0 {
        return Err(format!("child exited with status {status}"));
    }

    Ok(())
}

async fn status_listener(overlay: Arc<Mutex<tui::OverlayState>>) {
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio_vsock::VsockListener;

    let listener = loop {
        match VsockListener::bind(tokio_vsock::VsockAddr::new(libc::VMADDR_CID_ANY, vscomm::STATUS_PORT)) {
            Ok(l) => break l,
            Err(e) => {
                eprintln!("bunkerbox: vsock status listener bind failed ({e}), retrying...");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    };

    loop {
        let (mut stream, _peer) = match listener.accept().await {
            Ok(c) => c,
            Err(_) => continue,
        };

        let overlay = overlay.clone();
        tokio::spawn(async move {
            let mut header = [0u8; 6];
            if stream.read_exact(&mut header).await.is_err() {
                return;
            }

            let frame_type_raw = u16::from_le_bytes([header[0], header[1]]);
            let payload_len = u32::from_le_bytes([header[2], header[3], header[4], header[5]]) as usize;

            let Some(ft) = vscomm::FrameType::from_u16(frame_type_raw) else {
                return;
            };

            if !matches!(ft, vscomm::FrameType::UiCommand) {
                return;
            }

            let mut payload = vec![0u8; payload_len];
            if payload_len > 0 && stream.read_exact(&mut payload).await.is_err() {
                return;
            }

            if let Some((widget, cmd, opts, val)) = vscomm::decode_ui_payload(&payload) {
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
