mod cmdrun;

use std::env;
use std::ffi::OsString;

fn main() {
    if let Err(err) = run() {
        eprintln!("bunkerbox: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<OsString> = env::args_os().collect();

    match args.get(1).and_then(|arg| arg.to_str()) {
        Some("--setup") => cmdrun::run_sequence("setup"),
        Some("--image") => cmdrun::run_sequence("image"),
        Some("run") => {
            let sequence = args
                .get(2)
                .and_then(|arg| arg.to_str())
                .ok_or_else(|| "missing sequence name".to_string())?;
            cmdrun::run_sequence(sequence)
        }
        Some("--help") | Some("-h") => {
            usage()?;
            Ok(())
        }
        Some(other) => Err(format!("unknown argument: {other}\n\nRun `bunkerbox --help` for usage.")),
        None => {
            usage()?;
            Ok(())
        }
    }
}

fn usage() -> Result<(), String> {
    println!("Usage:");
    println!("  bunkerbox --setup       Run setup sequence");
    println!("  bunkerbox --image       Run image sequence");
    println!("  bunkerbox run <name>    Run named YAML sequence");
    println!("  bunkerbox --help        Show help");
    println!();
    println!("Sequences:");

    for name in cmdrun::sequence_names()? {
        println!("  {name}");
    }

    Ok(())
}
