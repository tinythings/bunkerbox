use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use caps::CapSet;

pub fn drop_privileges() -> Result<(), String> {
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1u64, 0u64, 0u64, 0u64) < 0 {
            return Err(format!("prctl PR_SET_NO_NEW_PRIVS: {}", std::io::Error::last_os_error()));
        }
    }

    for set in &[CapSet::Ambient, CapSet::Bounding, CapSet::Effective, CapSet::Permitted, CapSet::Inheritable] {
        caps::clear(None, *set).map_err(|e| format!("clear caps {:?}: {e}", set))?;
    }

    Ok(())
}

pub fn exec_command(command: &str, args: &[String], cwd: &Path, env: &[(String, String)]) -> Result<(), String> {
    std::env::set_current_dir(cwd).map_err(|e| format!("chdir to {}: {e}", cwd.display()))?;

    std::env::set_var("PATH", "/usr/local/bunkerbox-bin");
    for (key, val) in env {
        std::env::set_var(key, val);
    }

    let err = Command::new(command)
        .args(args)
        .stdin(std::process::Stdio::null())
        .exec();

    Err(format!("exec {}: {}", command, err))
}
