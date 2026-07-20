#[path = "../sandbox/mod.rs"]
mod sandbox;

use std::path::PathBuf;

use sandbox::{ns, mount, exec, MergedProfile, resolve_profile};

fn main() {
    if let Err(err) = run() {
        eprintln!("bunkerbox-sandbox: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut share_dir = PathBuf::from("/usr/share/bunkerbox");
    let mut profile_args: Vec<String> = Vec::new();
    let mut cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let mut envs: Vec<(String, String)> = Vec::new();
    let mut cmd_args: Vec<String> = Vec::new();

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    let mut after_dash = false;

    while i < args.len() {
        if after_dash {
            cmd_args.push(args[i].clone());
            i += 1;
            continue;
        }
        match args[i].as_str() {
            "--" => {
                after_dash = true;
            }
            "--share" => {
                i += 1;
                share_dir = PathBuf::from(args.get(i).ok_or("--share requires a value")?);
            }
            "--profile" => {
                i += 1;
                profile_args.push(args.get(i).ok_or("--profile requires a value")?.clone());
            }
            "--cwd" => {
                i += 1;
                cwd = PathBuf::from(args.get(i).ok_or("--cwd requires a value")?);
            }
            "--env" => {
                i += 1;
                let raw = args.get(i).ok_or("--env requires KEY=VALUE")?.clone();
                if let Some((key, val)) = raw.split_once('=') {
                    envs.push((key.to_string(), val.to_string()));
                } else {
                    return Err(format!("--env: invalid format '{}', expected KEY=VALUE", raw));
                }
            }
            other => {
                return Err(format!("unknown flag: {other}"));
            }
        }
        i += 1;
    }

    let profiles: Vec<_> = profile_args
        .iter()
        .map(|p| resolve_profile(p, &share_dir))
        .collect::<Result<Vec<_>, _>>()?;

    let merged = MergedProfile::from_profiles(&profiles);
    let command = cmd_args.first().ok_or("no command specified")?.clone();
    let rest_args: Vec<String> = cmd_args[1..].to_vec();

    ns::unshare_all()?;
    ns::map_current_user()?;

    let exit_code = ns::fork_and_wait(move || {
        mount::build_sandbox_root(&merged)?;
        exec::drop_privileges()?;
        exec::exec_command(&command, &rest_args, &cwd, &envs)?;
        Ok(())
    })?;

    std::process::exit(exit_code);
}
