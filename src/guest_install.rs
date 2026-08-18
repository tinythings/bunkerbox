pub use crate::remote::{validate_remote_wrapper_name, REMOTE_WRAPPER_STATE_FILE};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{symlink, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const REMOTE_WRAPPER_OWNER: &str = "bunkerbox-remote";
const VSCOMM_OWNER: &str = "bunkerbox-vscomm";
const MAX_REMOTE_WRAPPER_STATE_BYTES: u64 = 64 * 1024;
static NEXT_STATE_TEMP: AtomicU64 = AtomicU64::new(1);

pub fn synchronize_remote_wrappers(
    commands: impl IntoIterator<Item = String>, bin_dir: &Path, executable: &Path, vscomm_path: &Path,
) -> Result<(), String> {
    fs::create_dir_all(bin_dir).map_err(|error| format!("mkdir {}: {error}", bin_dir.display()))?;
    let (previous, _had_state) = read_managed_wrappers(bin_dir)?;
    let current = collect_wrapper_names(commands)?;

    for command in previous.difference(&current) {
        let target = bin_dir.join(command);
        if is_remote_wrapper_target(&target, executable) {
            fs::remove_file(&target).map_err(|error| format!("remove disabled remote wrapper {command}: {error}"))?;
        }
    }

    for command in &current {
        let target = bin_dir.join(command);
        let Some(link) = read_link_if_present(&target)? else {
            symlink(executable, &target).map_err(|error| format!("symlink remote wrapper {command}: {error}"))?;
            continue;
        };

        if link == executable {
            if !previous.contains(command) {
                return Err(format!("cannot install remote wrapper over unmanaged {}", target.display()));
            }
            continue;
        }

        let recognized_remote = is_remote_link(&link, executable) && previous.contains(command);
        let recognized_vscomm = link == vscomm_path || is_stale_vscomm_link(&link, vscomm_path);
        if !recognized_remote && !recognized_vscomm {
            if previous.contains(command) {
                let mut ownership = current.clone();
                ownership.remove(command);
                let _ = write_managed_wrappers(bin_dir, &ownership);
            }
            return Err(format!("cannot install remote wrapper over existing {}", target.display()));
        }
        fs::remove_file(&target).map_err(|error| format!("remove existing remote wrapper {command}: {error}"))?;
        symlink(executable, &target).map_err(|error| format!("symlink remote wrapper {command}: {error}"))?;
    }

    write_managed_wrappers(bin_dir, &current)
}

pub fn is_managed_remote_wrapper(bin_dir: &Path, command: &str) -> Result<bool, String> {
    let command = validate_remote_wrapper_name(command.to_string())?;
    let (managed, had_state) = read_managed_wrappers(bin_dir)?;
    let expected = bin_dir.join(REMOTE_WRAPPER_OWNER);
    Ok(had_state && managed.contains(&command) && is_remote_wrapper_target(&bin_dir.join(command), &expected))
}

pub fn install_vscomm_links(commands: impl IntoIterator<Item = String>, bin_dir: &Path, vscomm_path: &Path, path: &str) -> Result<(), String> {
    fs::create_dir_all(bin_dir).map_err(|error| format!("mkdir {}: {error}", bin_dir.display()))?;

    for command in commands {
        if command.is_empty() {
            continue;
        }
        if command == "bunkerbox" || command.starts_with("bunkerbox-") || command.starts_with(REMOTE_WRAPPER_STATE_FILE) {
            return Err(format!("vscomm command name is reserved: {command}"));
        }
        let target = bin_dir.join(&command);
        if validate_remote_wrapper_name(command.clone()).is_ok() && is_managed_remote_wrapper(bin_dir, &command)? {
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

fn collect_wrapper_names(commands: impl IntoIterator<Item = String>) -> Result<BTreeSet<String>, String> {
    let mut names = BTreeSet::new();
    for command in commands {
        let command = validate_remote_wrapper_name(command)?;
        if !names.insert(command.clone()) {
            return Err(format!("duplicate remote wrapper name: {command}"));
        }
    }
    Ok(names)
}

fn read_managed_wrappers(bin_dir: &Path) -> Result<(BTreeSet<String>, bool), String> {
    let path = bin_dir.join(REMOTE_WRAPPER_STATE_FILE);
    let Some(mut file) = open_private_state(&path)? else { return Ok((BTreeSet::new(), false)) };
    let length = file.metadata().map_err(|error| format!("stat remote wrapper state: {error}"))?.len();
    if length > MAX_REMOTE_WRAPPER_STATE_BYTES {
        return Err("remote wrapper state is too large".to_string());
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents).map_err(|error| format!("read remote wrapper state: {error}"))?;
    let mut names = BTreeSet::new();
    for line in contents.lines() {
        let name = validate_remote_wrapper_name(line.to_string())?;
        if !names.insert(name.clone()) {
            return Err(format!("duplicate remote wrapper state name: {name}"));
        }
    }
    Ok((names, true))
}

fn write_managed_wrappers(bin_dir: &Path, names: &BTreeSet<String>) -> Result<(), String> {
    let mut temporary = None;
    let mut file = None;
    for _ in 0..32 {
        let sequence = NEXT_STATE_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = bin_dir.join(format!("{REMOTE_WRAPPER_STATE_FILE}.tmp-{}-{sequence}", std::process::id()));
        let result = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path);
        match result {
            Ok(value) => {
                temporary = Some(path);
                file = Some(value);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("create remote wrapper state: {error}")),
        }
    }
    let temporary = temporary.ok_or_else(|| "could not create remote wrapper state temporary file".to_string())?;
    let mut file = file.expect("remote wrapper state file exists with its temporary path");
    let contents = names.iter().map(|name| format!("{name}\n")).collect::<String>();
    let result = (|| {
        file.write_all(contents.as_bytes()).map_err(|error| format!("write remote wrapper state: {error}"))?;
        file.sync_all().map_err(|error| format!("sync remote wrapper state: {error}"))?;
        fs::rename(&temporary, bin_dir.join(REMOTE_WRAPPER_STATE_FILE)).map_err(|error| format!("publish remote wrapper state: {error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn open_private_state(path: &Path) -> Result<Option<File>, String> {
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open remote wrapper state: {error}")),
    };
    let metadata = file.metadata().map_err(|error| format!("stat remote wrapper state: {error}"))?;
    if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
        return Err("remote wrapper state is not a private owned regular file".to_string());
    }
    Ok(Some(file))
}

fn read_link_if_present(path: &Path) -> Result<Option<PathBuf>, String> {
    match fs::read_link(path) {
        Ok(link) => Ok(Some(link)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Err(format!("remote wrapper entry is not a symlink: {}", path.display())),
        Err(error) => Err(format!("read remote wrapper entry {}: {error}", path.display())),
    }
}

fn is_remote_wrapper_target(path: &Path, expected: &Path) -> bool {
    fs::read_link(path).ok().is_some_and(|link| is_remote_link(&link, expected))
}

fn is_remote_link(link: &Path, expected: &Path) -> bool {
    link == expected || link.file_name().is_some_and(|name| name == OsStr::new(REMOTE_WRAPPER_OWNER))
}

fn is_stale_vscomm_link(link: &Path, expected: &Path) -> bool {
    link == expected || link.file_name().is_some_and(|name| name == OsStr::new(VSCOMM_OWNER))
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
