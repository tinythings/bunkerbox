use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

const REMOTE_MAKE_OWNER: &str = "bunkerbox-remote";

pub fn install_remote_make_link(bin_dir: &Path, executable: &Path, enabled: bool) -> Result<(), String> {
    let target = bin_dir.join("make");
    let managed = is_managed_remote_make_link(&target);

    if enabled {
        if let Ok(link) = fs::read_link(&target) {
            if link == executable {
                return Ok(());
            }
        }
        if fs::symlink_metadata(&target).is_ok() {
            if !managed {
                return Err(format!("cannot install remote make wrapper over existing {}", target.display()));
            }
            fs::remove_file(&target).map_err(|error| format!("remove existing remote make wrapper: {error}"))?;
        }
        symlink(executable, &target).map_err(|error| format!("symlink remote make wrapper: {error}"))?;
    } else if managed {
        fs::remove_file(&target).map_err(|error| format!("remove disabled remote make wrapper: {error}"))?;
    }
    Ok(())
}

pub fn install_vscomm_links(commands: impl IntoIterator<Item = String>, bin_dir: &Path, vscomm_path: &Path, path: &str) -> Result<(), String> {
    fs::create_dir_all(bin_dir).map_err(|error| format!("mkdir {}: {error}", bin_dir.display()))?;

    for command in commands {
        if command.is_empty() {
            continue;
        }
        let target = bin_dir.join(&command);
        if command == "make" && is_managed_remote_make_link(&target) {
            continue;
        }
        if command_exists_in_path_except(&command, vscomm_path, path) {
            continue;
        }
        if let Ok(link) = fs::read_link(&target) {
            if link == vscomm_path {
                continue;
            }
        }
        if fs::symlink_metadata(&target).is_ok() {
            fs::remove_file(&target).map_err(|error| format!("remove existing {command} link: {error}"))?;
        }
        symlink(vscomm_path, &target).map_err(|error| format!("symlink {command}: {error}"))?;
    }

    Ok(())
}

fn is_managed_remote_make_link(target: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(target) else { return false };
    if !metadata.file_type().is_symlink() {
        return false;
    }
    fs::read_link(target).ok().and_then(|link| link.file_name().map(OsStr::to_owned)).is_some_and(|name| name == REMOTE_MAKE_OWNER)
}

fn command_exists_in_path_except(command: &str, except: &Path, path: &str) -> bool {
    for directory in path.split(':') {
        let candidate = PathBuf::from(directory).join(command);
        if candidate == except {
            continue;
        }
        if candidate.is_file() {
            let Ok(metadata) = fs::metadata(&candidate) else { continue };
            if metadata.permissions().mode() & 0o111 != 0 {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
#[path = "guest_install_ut.rs"]
mod tests;
