mod cfg;
mod cfgsetup;
mod clidef;
mod cmdrun;
mod daemon;
mod kata;
mod overlay;
mod vscomm;
mod workspace;

use cfg::{ProjectConfig, WorkspaceMode};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

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
            cfgsetup::run()
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
    let name = cfg::RuntimeConfig::invoked_name()?;
    let container_name = format!("bunkerbox-{name}");

    let ws = workspace::resolve(workspace_mode, quota, config.workspace_exclude.as_deref(), &name)?;
    let workspace_path = ws.path().to_path_buf();

    let repo_root = workspace::project_root()?;
    let env = ProjectConfig::load_or_create(&repo_root)?;

    let daemon = if !env.project.passthrough.is_empty() {
        Some(daemon::VsockDaemon::start(env.project.passthrough.clone(), env.project.env, workspace_path)?)
    } else {
        None
    };

    let result = kata::run(&config, ws, &container_name, share_dir, &name, daemon.is_some());

    if let Some(d) = daemon {
        tokio::runtime::Handle::current().block_on(d.shutdown());
    }

    result
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
