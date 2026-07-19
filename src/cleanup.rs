use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Global flag set by SIGINT/SIGTERM handlers so callers can respond early.
pub static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Best-effort cleanup for a Bunkerbox project at startup or after interruption.
///
/// - Kills leftover processes that have this project's paths in their command line.
/// - Removes stale containerd tasks/containers for the project.
/// - Unmounts overlay/loopback workspace and deletes the partial `upper.img` so
///   the next run can recreate it from scratch.
///
/// All operations are allowed to fail; the goal is to leave the project in a
/// recoverable state.
pub fn cleanup_project(repo_root: &Path, container_name: &str) {
    eprintln!("bunkerbox: cleaning up stale state for project {}...", repo_root.display());

    let overlay_dir = repo_root.join(".bunkerbox");
    if overlay_dir.exists() {
        let _ = kill_processes_with_pattern(&overlay_dir.to_string_lossy());
    }

    let _ = crate::kata::remove_stale_container(container_name);

    let mount_point = overlay_dir.join("workspace");
    let loop_mount = overlay_dir.join("upper-mount");
    let loopback = overlay_dir.join("upper.img");

    let _ = run_allow_failure("umount", &[&mount_point.to_string_lossy()]);
    let _ = run_allow_failure("umount", &[&loop_mount.to_string_lossy()]);

    if loopback.exists() {
        if let Some(dev) = find_loop_device(&loopback) {
            if !dev.is_empty() {
                let _ = run_allow_failure("losetup", &["-d", &dev]);
            }
        }
        let _ = fs::remove_file(&loopback);
    }

    // Remove any stale sessions whose state files reference mounts that no longer exist.
    let sessions_dir = overlay_dir.join("sessions");
    if sessions_dir.is_dir() {
        for entry in fs::read_dir(&sessions_dir).into_iter().flatten() {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                if let Ok(text) = fs::read_to_string(&path) {
                    if text.contains(&repo_root.to_string_lossy().to_string()) {
                        let _ = fs::remove_file(&path);
                    }
                }
            }
        }
    }
}

/// Installs process-wide SIGINT and SIGTERM handlers that set
/// [`SHUTDOWN_REQUESTED`] and best-effort clean up the project.
pub fn install_signal_handlers(repo_root: PathBuf, container_name: String) {
    let handler = move || {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
        cleanup_project(&repo_root, &container_name);
        std::process::exit(130);
    };

    if let Err(err) = ctrlc::set_handler(handler) {
        eprintln!("bunkerbox: warning: could not install signal handler: {err}");
    }
}

fn run_allow_failure(program: &str, args: &[&str]) -> Result<(), String> {
    Command::new("sudo")
        .arg(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("failed to run sudo {program}: {e}"))?;
    Ok(())
}

fn find_loop_device(loopback: &Path) -> Option<String> {
    let output = Command::new("sudo")
        .args(["losetup", "-j", &loopback.to_string_lossy()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    String::from_utf8(output.stdout).ok()?.split(':').next().map(|s| s.trim().to_string())
}

/// Sends SIGTERM, briefly waits, then SIGKILL to any process whose command line
/// contains `pattern`. Uses the short process basename list to reduce false positives.
fn kill_processes_with_pattern(pattern: &str) -> Result<(), String> {
    let pids = find_matching_pids(pattern)?;
    if pids.is_empty() {
        return Ok(());
    }

    for pid in &pids {
        let _ = Command::new("kill").arg("-TERM").arg(pid.to_string()).status();
    }

    // Give processes a moment to exit before escalating to SIGKILL.
    std::thread::sleep(Duration::from_millis(500));

    for pid in &pids {
        if is_process_alive(*pid) {
            let _ = Command::new("kill").arg("-KILL").arg(pid.to_string()).status();
        }
    }

    Ok(())
}

fn find_matching_pids(pattern: &str) -> Result<Vec<u32>, String> {
    let mut pids = Vec::new();
    for entry in fs::read_dir("/proc").map_err(|e| format!("failed to read /proc: {e}"))? {
        let entry = entry.map_err(|e| format!("proc entry: {e}"))?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else { continue };
        let Ok(pid) = name_str.parse::<u32>() else { continue };

        let cmdline = fs::read_to_string(entry.path().join("cmdline")).unwrap_or_default();
        if cmdline.is_empty() {
            continue;
        }
        // /proc/PID/cmdline uses NUL separators; join them so we can match paths.
        let normalized = cmdline.replace('\0', " ");
        if normalized.contains(pattern) {
            pids.push(pid);
        }
    }
    Ok(pids)
}

fn is_process_alive(pid: u32) -> bool {
    fs::metadata(format!("/proc/{pid}")).is_ok()
}
