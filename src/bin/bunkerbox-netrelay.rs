use bunkerbox::netrelay::{bind_relay_listener, relay};
use std::env;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;

fn main() {
    let result = run();
    if let Err(err) = result {
        eprintln!("bunkerbox-netrelay: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();

    let mut socket_path: Option<PathBuf> = None;
    let mut target_start = None;

    let mut i = 1;
    while i < args.len() {
        if args[i] == "--socket" && i + 1 < args.len() {
            socket_path = Some(PathBuf::from(args[i + 1].clone()));
            i += 2;
        } else if args[i] == "--" {
            target_start = Some(i + 1);
            break;
        } else {
            i += 1;
        }
    }

    let socket_path = socket_path.ok_or_else(|| {
        eprintln!("usage: bunkerbox-netrelay --socket <PATH> -- <COMMAND> [ARGS...]");
        "missing --socket".to_string()
    })?;

    let start_idx = target_start.ok_or_else(|| {
        eprintln!("usage: bunkerbox-netrelay --socket <PATH> -- <COMMAND> [ARGS...]");
        "missing -- separator".to_string()
    })?;

    let target_args: Vec<String> = args[start_idx..].to_vec();
    if target_args.is_empty() {
        eprintln!("usage: bunkerbox-netrelay --socket <PATH> -- <COMMAND> [ARGS...]");
        return Err("no target command".to_string());
    }

    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("tokio: {e}"))?;
    let _guard = rt.enter();

    let listener = rt.block_on(bind_relay_listener())?;

    let status = rt.block_on(relay(listener, socket_path, &target_args))?;

    match status.code() {
        Some(code) => std::process::exit(code),
        None => {
            let sig = status.signal().unwrap_or(1);
            std::process::exit(128i32.wrapping_add(sig));
        }
    }
}
