mod platform;
mod process;
mod storage;
mod worker;

use std::path::PathBuf;

fn main() {
    match parse_args(std::env::args().skip(1).collect()) {
        Ok(root) => {
            if let Err(error) = worker::run_stdio(&root) {
                write_diagnostic(&error);
                std::process::exit(70);
            }
        }
        Err(error) => {
            write_diagnostic(&error);
            std::process::exit(64);
        }
    }
}

fn parse_args(args: Vec<String>) -> Result<PathBuf, String> {
    let mut stdio = false;
    let mut root = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--stdio" => {
                if stdio {
                    return Err("duplicate --stdio".to_string());
                }
                stdio = true;
                index += 1;
            }
            "--workspace-root" => {
                if root.is_some() {
                    return Err("duplicate --workspace-root".to_string());
                }
                let value = args.get(index + 1).ok_or_else(|| "--workspace-root requires a path".to_string())?;
                if value.is_empty() || !std::path::Path::new(value).is_absolute() {
                    return Err("--workspace-root must be an absolute path".to_string());
                }
                root = Some(PathBuf::from(value));
                index += 2;
            }
            value => return Err(format!("unknown worker argument: {value}")),
        }
    }
    if !stdio {
        return Err("--stdio is required".to_string());
    }
    root.ok_or_else(|| "--workspace-root is required".to_string())
}

fn write_diagnostic(message: &str) {
    let mut message = message.as_bytes().to_vec();
    const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;
    if message.len() > MAX_DIAGNOSTIC_BYTES {
        message.truncate(MAX_DIAGNOSTIC_BYTES);
    }
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let _ = std::io::Write::write_all(&mut stderr, &message);
    let _ = std::io::Write::write_all(&mut stderr, b"\n");
}
