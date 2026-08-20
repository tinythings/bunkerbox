mod platform;
mod process;
mod storage;
mod worker;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use storage::WorkerStateLimits;

struct WorkerConfig {
    root: PathBuf,
    limits: WorkerStateLimits,
    build_timeout: Duration,
    max_output_bytes: u64,
}

fn main() {
    match parse_args(std::env::args().skip(1).collect()) {
        Ok(config) => {
            if let Err(error) = worker::run_stdio_with_config(&config.root, config.limits, config.build_timeout, config.max_output_bytes) {
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

fn parse_args(args: Vec<String>) -> Result<WorkerConfig, String> {
    let mut stdio = false;
    let mut root = None;
    let mut limits = WorkerStateLimits::default();
    let mut build_timeout = Duration::from_secs(30);
    let mut max_output_bytes = 64 * 1024 * 1024;
    let mut seen = BTreeSet::new();
    let mut index = 0;
    while index < args.len() {
        if !seen.insert(args[index].clone()) {
            return Err(format!("duplicate {}", args[index]));
        }
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
            "--build-timeout-ms" => {
                build_timeout = Duration::from_millis(parse_value(&args, &mut index, "--build-timeout-ms")?);
                if build_timeout.is_zero() {
                    return Err("--build-timeout-ms must be positive".to_string());
                }
            }
            "--max-output-bytes" => {
                max_output_bytes = parse_value(&args, &mut index, "--max-output-bytes")?;
                if max_output_bytes == 0 || max_output_bytes > process::WORKER_MAX_OUTPUT_BYTES {
                    return Err(format!("--max-output-bytes must be between 1 and {}", process::WORKER_MAX_OUTPUT_BYTES));
                }
            }
            "--max-worker-uploads" => limits.max_uploads = parse_count(&args, &mut index, "--max-worker-uploads")?,
            "--max-worker-upload-bytes" => limits.max_upload_bytes = parse_value(&args, &mut index, "--max-worker-upload-bytes")?,
            "--max-worker-jobs" => limits.max_jobs = parse_count(&args, &mut index, "--max-worker-jobs")?,
            "--max-worker-job-bytes" => limits.max_job_bytes = parse_value(&args, &mut index, "--max-worker-job-bytes")?,
            "--max-worker-artifact-spools" => limits.max_artifact_spools = parse_count(&args, &mut index, "--max-worker-artifact-spools")?,
            "--max-worker-artifact-spool-bytes" => {
                limits.max_artifact_spool_bytes = parse_value(&args, &mut index, "--max-worker-artifact-spool-bytes")?
            }
            "--max-worker-state-entries" => limits.max_state_entries = parse_count(&args, &mut index, "--max-worker-state-entries")?,
            value => return Err(format!("unknown worker argument: {value}")),
        }
    }
    if !stdio {
        return Err("--stdio is required".to_string());
    }
    let root = root.ok_or_else(|| "--workspace-root is required".to_string())?;
    if limits.max_uploads == 0
        || limits.max_upload_bytes == 0
        || limits.max_jobs == 0
        || limits.max_job_bytes == 0
        || limits.max_artifact_spools == 0
        || limits.max_artifact_spool_bytes == 0
        || limits.max_state_entries == 0
    {
        return Err("worker state limits must be positive".to_string());
    }
    limits.validate()?;
    Ok(WorkerConfig { root, limits, build_timeout, max_output_bytes })
}

fn parse_value(args: &[String], index: &mut usize, flag: &str) -> Result<u64, String> {
    let value = args.get(*index + 1).ok_or_else(|| format!("{flag} requires a value"))?;
    let value = value.parse::<u64>().map_err(|_| format!("{flag} requires an unsigned integer"))?;
    *index += 2;
    Ok(value)
}

fn parse_count(args: &[String], index: &mut usize, flag: &str) -> Result<usize, String> {
    usize::try_from(parse_value(args, index, flag)?).map_err(|_| format!("{flag} is too large"))
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

#[cfg(test)]
#[path = "main_ut.rs"]
mod tests;
