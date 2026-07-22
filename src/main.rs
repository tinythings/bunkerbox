use bunkerbox::cfg::{ProjectConfig, WorkspaceMode};
use bunkerbox::{cfg, cfgsetup, clidef, cmdrun, daemon, kata, overlay, tui, workspace};
use std::cell::RefCell;
use std::ffi::OsString;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;

fn main() {
    if let Err(err) = run() {
        eprintln!("bunkerbox: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
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

    let mut pipe_fds = [-1i32, -1];
    unsafe {
        libc::pipe(pipe_fds.as_mut_ptr());
    }
    let (pipe_r, pipe_w) = (pipe_fds[0], pipe_fds[1]);

    let (cols, rows) = crossterm::terminal::size().map_err(|e| format!("terminal size: {e}"))?;

    let winsize = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };

    let mut master: RawFd = -1;
    let pid = unsafe { libc::forkpty(&mut master, std::ptr::null_mut(), std::ptr::null(), &winsize) };

    if pid == -1 {
        return Err("forkpty failed".to_string());
    }

    if pid == 0 {
        unsafe { libc::close(pipe_r) };

        let ws = workspace::resolve(workspace_mode, quota, exclude.as_deref(), &name)?;
        let wp = ws.path().to_path_buf();
        let path_bytes = wp.to_string_lossy();
        unsafe {
            libc::write(pipe_w, path_bytes.as_bytes().as_ptr() as *const libc::c_void, path_bytes.len());
            libc::close(pipe_w);
        }

        let vsock_enabled = !passthrough.is_empty();
        let code = match kata::run(&config, ws, &container_name, share_dir, &name, vsock_enabled) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("bunkerbox: {e}");
                1
            }
        };
        std::process::exit(code);
    }

    unsafe { libc::close(pipe_w) };

    let tui_result = tui::event_loop(
        master,
        rows,
        cols,
        Some(pipe_r),
        Some(|path_bytes: Vec<u8>| {
            let wp = PathBuf::from(String::from_utf8_lossy(&path_bytes).into_owned());
            if !passthrough.is_empty() {
                *daemon_clone.borrow_mut() = Some(daemon::VsockDaemon::start(passthrough, env_mode, wp, profiles, share_dir_owned, merged_allow)?);
            }
            Ok(())
        }),
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
