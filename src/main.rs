mod clidef;
mod cmdrun;
mod kata;
mod runtime;
mod workspace;

use std::path::PathBuf;

fn main() {
    if let Err(err) = run() {
        eprintln!("bunkerbox: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let share_dir = share_dir_from_args()?;

    if let Some(config) = runtime::load_for_invoked_name(&share_dir)? {
        return run_packaged_runtime(config);
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
        Some((name, _)) => Err(format!("unknown command: {name}")),
        None => {
            cli.print_help().map_err(|err| err.to_string())?;
            println!();
            Ok(())
        }
    }
}

fn share_dir_from_args() -> Result<PathBuf, String> {
    let mut args = std::env::args_os().skip(1);

    while let Some(arg) = args.next() {
        if arg == "--share" {
            return args.next().map(PathBuf::from).ok_or_else(|| "--share requires a path".to_string());
        }
    }

    Ok(PathBuf::from(runtime::DEFAULT_SHARE_DIR))
}

fn run_packaged_runtime(config: runtime::RuntimeConfig) -> Result<(), String> {
    if config.oci.as_os_str().is_empty() {
        return Err("runtime config missing oci".to_string());
    }

    if config.image.trim().is_empty() {
        return Err("runtime config missing image".to_string());
    }

    let workspace = workspace::ensure()?;
    let name = runtime::invoked_name()?;
    let container_name = format!("bunkerbox-{name}");

    kata::run(&config, &workspace, &container_name)
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
