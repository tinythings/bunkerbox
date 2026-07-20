use std::fs;
use std::io::Write;
use std::process;

use nix::sched::CloneFlags;
use nix::sys::wait::WaitStatus;
use nix::unistd::{fork, ForkResult};

pub fn unshare_all() -> Result<(), String> {
    let flags = CloneFlags::CLONE_NEWUSER
        | CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_NEWPID
        | CloneFlags::CLONE_NEWNET
        | CloneFlags::CLONE_NEWIPC
        | CloneFlags::CLONE_NEWUTS;

    nix::sched::unshare(flags).map_err(|e| format!("unshare failed: {e}"))?;
    Ok(())
}

pub fn map_current_user() -> Result<(), String> {
    let uid = nix::unistd::getuid();
    let gid = nix::unistd::getgid();

    fs::write("/proc/self/setgroups", b"deny").map_err(|e| format!("setgroups: {e}"))?;

    fs::write("/proc/self/uid_map", format!("0 {} 1\n", uid.as_raw()))
        .map_err(|e| format!("uid_map: {e}"))?;

    fs::write("/proc/self/gid_map", format!("0 {} 1\n", gid.as_raw()))
        .map_err(|e| format!("gid_map: {e}"))?;

    Ok(())
}

pub fn fork_and_wait<F>(child_fn: F) -> Result<i32, String>
where
    F: FnOnce() -> Result<(), String>,
{
    match unsafe { fork() }.map_err(|e| format!("fork: {e}"))? {
        ForkResult::Child => {
            match child_fn() {
                Ok(()) => {
                    process::exit(0);
                }
                Err(e) => {
                    let _ = writeln!(std::io::stderr(), "bunkerbox-sandbox: child error: {e}");
                    process::exit(1);
                }
            }
        }
        ForkResult::Parent { child } => {
            // Wait for child exit and propagate exit code.
            loop {
                match nix::sys::wait::waitpid(child, None).map_err(|e| format!("waitpid: {e}"))? {
                    WaitStatus::Exited(_pid, code) => return Ok(code),
                    WaitStatus::Signaled(_pid, signal, _coredump) => {
                        return Ok(128 + signal as i32);
                    }
                    WaitStatus::Stopped(_, _) => continue,
                    WaitStatus::Continued(_) => continue,
                    _ => continue,
                }
            }
        }
    }
}
