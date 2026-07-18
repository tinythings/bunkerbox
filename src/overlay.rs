use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::envconf::EnvConfig;

/// A copy-on-write workspace using overlayfs and a loop-mounted ext4 image for upper storage.
pub struct CowWorkspace {
    pub mount_point: PathBuf,
    loopback: PathBuf,
    loop_mount: PathBuf,
    lower_root: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
struct CowPaths {
    overlay_dir: PathBuf,
    loopback: PathBuf,
    mounts_dir: PathBuf,
    loop_mount: PathBuf,
    upper_dir: PathBuf,
    work_dir: PathBuf,
    mount_point: PathBuf,
    lower_root: PathBuf,
}

/// Persisted state for an active overlay session, saved so it can be synced later.
#[derive(serde::Serialize, serde::Deserialize)]
struct SessionState {
    app_name: String,
    mount_point: String,
    upper_dir: String,
    work_dir: String,
    repo_root: String,
    loopback: String,
    #[serde(default)]
    lower_root: String,
}

impl CowWorkspace {
    /// Creates and mounts an overlay filesystem workspace with quota-limited storage,
    /// bind-mounting excluded paths to separate directories outside the overlay.
    /// The overlay is mounted at `repo_root` itself with a read-only bind-mount of the
    /// raw repo serving as the lowerdir.
    pub fn setup(
        repo_root: &Path, env_config: &EnvConfig, runtime_quota: u64, _runtime_exclude: Option<&[String]>, app_name: &str,
    ) -> Result<Self, String> {
        let paths = cow_paths(repo_root);

        fs::create_dir_all(&paths.overlay_dir).map_err(|e| format!("failed to create {}: {e}", paths.overlay_dir.display()))?;

        Self::ensure_gitignore(repo_root)?;

        Self::cleanup_stale(repo_root)?;

        let sessions_dir = paths.overlay_dir.join("sessions");
        let session_path = sessions_dir.join(format!("{app_name}.json"));
        let reusing = paths.loopback.exists();

        fs::create_dir_all(&paths.loop_mount).map_err(|e| format!("failed to create {}: {e}", paths.loop_mount.display()))?;

        if reusing {
            if session_path.exists() {
                eprintln!("bunkerbox: recovering previous session...");
            }
            mount_loopback(&paths.loopback, &paths.loop_mount)?;
        } else {
            let size_mb = env_config.strict_cow_quota_bytes(repo_root)?.max(runtime_quota) / (1024 * 1024);
            if size_mb == 0 {
                return Err("workspace quota too small".to_string());
            }
            run_command("dd", &["if=/dev/zero", &format!("of={}", paths.loopback.display()), "bs=1M", &format!("count={size_mb}"), "status=none"])?;
            run_command("mkfs.ext4", &["-F", &paths.loopback.to_string_lossy()])?;
            mount_loopback(&paths.loopback, &paths.loop_mount)?;

            let user = current_user_spec()?;
            run_command("chown", &[&user, &paths.loop_mount.to_string_lossy()])?;
        }

        fs::create_dir_all(&paths.upper_dir).map_err(|e| format!("failed to create {}: {e}", paths.upper_dir.display()))?;
        fs::create_dir_all(&paths.work_dir).map_err(|e| format!("failed to create {}: {e}", paths.work_dir.display()))?;

        fs::create_dir_all(&paths.lower_root).map_err(|e| format!("failed to create {}: {e}", paths.lower_root.display()))?;
        run_command("mount", &["--bind", &repo_root.to_string_lossy(), &paths.lower_root.to_string_lossy()])?;
        run_command("mount", &["-o", "remount,bind,ro", &paths.lower_root.to_string_lossy()])?;

        fs::create_dir_all(&paths.mount_point).map_err(|e| format!("failed to create {}: {e}", paths.mount_point.display()))?;
        let lowerdir = paths.lower_root.to_string_lossy();
        let upperdir = paths.upper_dir.to_string_lossy();
        let workdir = paths.work_dir.to_string_lossy();
        run_command(
            "mount",
            &[
                "-t",
                "overlay",
                "overlay",
                "-o",
                &format!("lowerdir={lowerdir},upperdir={upperdir},workdir={workdir}"),
                &paths.mount_point.to_string_lossy(),
            ],
        )?;

        let sessions_dir = paths.overlay_dir.join("sessions");
        fs::create_dir_all(&sessions_dir).map_err(|e| format!("failed to create {}: {e}", sessions_dir.display()))?;
        let state = SessionState {
            app_name: app_name.to_string(),
            mount_point: paths.mount_point.to_string_lossy().to_string(),
            upper_dir: paths.upper_dir.to_string_lossy().to_string(),
            work_dir: paths.work_dir.to_string_lossy().to_string(),
            repo_root: repo_root.to_string_lossy().to_string(),
            loopback: paths.loopback.to_string_lossy().to_string(),
            lower_root: paths.lower_root.to_string_lossy().to_string(),
        };
        fs::write(
            &sessions_dir.join(format!("{app_name}.json")),
            serde_json::to_string(&state).map_err(|e| format!("failed to serialize session state: {e}"))?,
        )
        .map_err(|e| format!("failed to write session state: {e}"))?;

        Ok(CowWorkspace { mount_point: paths.mount_point, loopback: paths.loopback, loop_mount: paths.loop_mount, lower_root: paths.lower_root })
    }

    /// Ensures `.bunkerbox/` is listed in the repo's `.gitignore` file.
    fn ensure_gitignore(repo_root: &Path) -> Result<(), String> {
        let gitignore = repo_root.join(".gitignore");
        let mut exists = false;

        if gitignore.exists() {
            let contents = fs::read_to_string(&gitignore).unwrap_or_default();
            if contents.lines().any(|l| l.trim() == ".bunkerbox/" || l.trim() == ".bunkerbox") {
                exists = true;
            }
        }

        if !exists {
            let mut contents = if gitignore.exists() { fs::read_to_string(&gitignore).unwrap_or_default() } else { String::new() };
            if !contents.is_empty() && !contents.ends_with('\n') {
                contents.push('\n');
            }
            contents.push_str(".bunkerbox/\n");
            fs::write(&gitignore, contents).map_err(|e| format!("failed to update .gitignore: {e}"))?;
        }

        Ok(())
    }

    /// Unmounts stale overlay and loop mounts from previous runs, cleans up temporary
    /// directories, and detaches stale loop devices. Reads session state files to discover
    /// stale `lower_root` and loop mount paths from previous PIDs.
    fn cleanup_stale(repo_root: &Path) -> Result<(), String> {
        let overlay_dir = repo_root.join(".bunkerbox");
        let child_mounts = parse_mounts_under(&overlay_dir);
        for mnt in child_mounts.iter().rev() {
            if run_command_allow_failure("umount", &[&mnt.to_string_lossy()]).is_err()
                && run_command_allow_failure("umount", &["-l", &mnt.to_string_lossy()]).is_err()
            {
                run_command_allow_failure("umount", &["-f", &mnt.to_string_lossy()]).ok();
            }
        }

        let sessions_dir = repo_root.join(".bunkerbox/sessions");
        if sessions_dir.is_dir() {
            if let Ok(entries) = fs::read_dir(&sessions_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "json") {
                        if let Ok(data) = fs::read_to_string(&path) {
                            if let Ok(state) = serde_json::from_str::<SessionState>(&data) {
                                if !state.lower_root.is_empty() {
                                    run_command_allow_failure("umount", &[&state.lower_root]).ok();
                                    run_command_allow_failure("umount", &["-l", &state.lower_root]).ok();
                                    let _ = fs::remove_dir(Path::new(&state.lower_root));
                                }
                                if !state.upper_dir.is_empty() {
                                    if let Some(loop_mnt) = Path::new(&state.upper_dir).parent() {
                                        run_command_allow_failure("umount", &[&loop_mnt.to_string_lossy()]).ok();
                                        run_command_allow_failure("umount", &["-l", &loop_mnt.to_string_lossy()]).ok();
                                        let _ = fs::create_dir_all(loop_mnt);
                                    }
                                }
                                if !state.mount_point.is_empty() {
                                    let _ = fs::create_dir_all(Path::new(&state.mount_point));
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

fn cow_paths(repo_root: &Path) -> CowPaths {
    let overlay_dir = repo_root.join(".bunkerbox");
    let mounts_dir = overlay_dir.join("mounts");
    let loop_mount = mounts_dir.join("loop");
    CowPaths {
        loopback: overlay_dir.join("upper.img"),
        upper_dir: loop_mount.join("upper"),
        work_dir: loop_mount.join("work"),
        mount_point: overlay_dir.join("workspace"),
        lower_root: mounts_dir.join("lower"),
        overlay_dir,
        mounts_dir,
        loop_mount,
    }
}

/// Finds the loop device (e.g. `/dev/loop0`) associated with a backing file via `losetup -j`.
fn find_loop_device(loopback: &Path) -> Option<String> {
    let output =
        Command::new("sudo").args(["losetup", "-j", &loopback.to_string_lossy()]).stdout(Stdio::piped()).stderr(Stdio::null()).output().ok()?;

    String::from_utf8(output.stdout).ok()?.split(':').next().map(|s| s.trim().to_string())
}

fn mount_loopback(loopback: &Path, loop_mount: &Path) -> Result<(), String> {
    run_command_allow_failure("losetup", &["-d", &find_loop_device(loopback).unwrap_or_default()]).ok();
    run_command_allow_failure("e2fsck", &["-p", &loopback.to_string_lossy()]).ok();
    run_command("mount", &["-o", "loop", &loopback.to_string_lossy(), &loop_mount.to_string_lossy()])
}

/// Runs a command via `sudo`, returning an error on non-zero exit or failure to spawn.
/// Captures stderr and includes it in the error message for diagnostics.
fn run_command(program: &str, args: &[&str]) -> Result<(), String> {
    let mut all_args: Vec<&str> = vec![program];
    all_args.extend_from_slice(args);
    let output = Command::new("sudo")
        .args(&all_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("failed to run sudo {program}: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("sudo {program} failed with status {}", output.status)
        } else {
            format!("sudo {program} failed with status {}: {stderr}", output.status)
        };
        return Err(detail);
    }
    Ok(())
}

/// Runs a command via `sudo`, ignoring exit status and stderr. Fails only on spawn errors.
fn run_command_allow_failure(program: &str, args: &[&str]) -> Result<(), String> {
    let mut all_args: Vec<&str> = vec![program];
    all_args.extend_from_slice(args);
    Command::new("sudo")
        .args(&all_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("failed to run sudo {program}: {e}"))?;
    Ok(())
}

impl CowWorkspace {
    /// Recursively checks if the overlay upperdir has any unsynced file changes.
    fn has_unsynced_changes(&self) -> bool {
        fs::read_dir(self.loop_mount.join("upper")).map(|mut entries| entries.next().is_some()).unwrap_or(false)
    }
}

/// Parses `/proc/mounts` for all mounts nested under the given mount point,
/// returning them sorted deepest-first for correct unmount ordering.
fn parse_mounts_under(mount_point: &Path) -> Vec<PathBuf> {
    let Ok(contents) = fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    let prefix = mount_point.to_string_lossy();
    let mut mounts: Vec<PathBuf> = Vec::new();
    for line in contents.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            let mnt = parts[1];
            if mnt.starts_with(prefix.as_ref()) && mnt != prefix.as_ref() {
                mounts.push(PathBuf::from(mnt));
            }
        }
    }
    mounts.sort_by_key(|b| std::cmp::Reverse(b.as_os_str().len()));
    mounts
}

/// Returns the current user's `uid:gid` string via the `id` command.
fn current_user_spec() -> Result<String, String> {
    let uid = run_id_arg("-u")?;
    let gid = run_id_arg("-g")?;
    if uid.is_empty() || gid.is_empty() || !uid.chars().all(|ch| ch.is_ascii_digit()) || !gid.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(format!("failed to determine current uid/gid: {uid}:{gid}"));
    }
    Ok(format!("{uid}:{gid}"))
}

/// Runs `id <arg>` and returns the trimmed stdout.
fn run_id_arg(arg: &str) -> Result<String, String> {
    let output = Command::new("id").arg(arg).output().map_err(|e| format!("id {arg}: {e}"))?;
    if !output.status.success() {
        return Err(format!("id {arg} failed"));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("id {arg}: {e}")).map(|s| s.trim().to_string())
}

/// Syncs unsaved changes from all active overlay sessions back to the repo root.
/// If `app_name` is provided, only that session is synced.
pub fn sync_sessions(repo_root: &Path, app_name: Option<&str>) -> Result<(), String> {
    let sessions_dir = repo_root.join(".bunkerbox/sessions");

    if !sessions_dir.is_dir() {
        println!("No active sessions.");
        return Ok(());
    }

    let mut sessions = Vec::new();
    for entry in fs::read_dir(&sessions_dir).map_err(|e| format!("failed to read {}: {e}", sessions_dir.display()))? {
        let path = entry.map_err(|e| format!("failed to read session entry: {e}"))?.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            if let Some(name) = path.file_stem().map(|s| s.to_string_lossy().to_string()) {
                if app_name.is_none_or(|n| n == name) {
                    sessions.push((name, path));
                }
            }
        }
    }

    if sessions.is_empty() {
        println!("No matching sessions found.");
        return Ok(());
    }

    for (name, path) in &sessions {
        sync_session(name, path)?;
    }

    Ok(())
}

/// Syncs changes from a single session's overlay upperdir to the lowerdir (or repo root
/// for legacy sessions). Kernel duplicate overlays at the lowerdir are unmounted first
/// so writes reach the real filesystem instead of the shared upperdir.
fn sync_session(name: &str, state_path: &Path) -> Result<(), String> {
    let state: SessionState = serde_json::from_str(&fs::read_to_string(state_path).map_err(|e| format!("failed to read session state: {e}"))?)
        .map_err(|e| format!("failed to parse session state: {e}"))?;

    if is_mounted(Path::new(&state.mount_point)) {
        return Err(format!("{name}: session is still active; stop the VM before syncing"));
    }

    let loop_mount = Path::new(&state.upper_dir).parent().ok_or_else(|| format!("{name}: invalid upper_dir in session state"))?.to_path_buf();

    fs::create_dir_all(&loop_mount).map_err(|e| format!("failed to create {}: {e}", loop_mount.display()))?;

    let mounted_here = is_mounted(&loop_mount);
    if !mounted_here {
        mount_loopback(Path::new(&state.loopback), &loop_mount)?;
    }

    let sync_target = Path::new(&state.repo_root);
    let sync_result = (|| -> Result<usize, String> {
        let count = sync_upper(Path::new(&state.upper_dir), sync_target)?;
        flush_dir(Path::new(&state.upper_dir))?;
        flush_dir(Path::new(&state.work_dir))?;
        Ok(count)
    })();

    if !mounted_here {
        run_command_allow_failure("umount", &[&loop_mount.to_string_lossy()]).ok();
        run_command_allow_failure("umount", &["-l", &loop_mount.to_string_lossy()]).ok();
    }

    let count = sync_result?;

    if count == 0 {
        println!("{name}: no changes");
    } else {
        println!("{name}: {count} changes synced");
    }
    Ok(())
}

/// Checks whether a path is currently mounted by scanning `/proc/mounts`.
fn is_mounted(path: &Path) -> bool {
    let Ok(contents) = fs::read_to_string("/proc/mounts") else {
        return false;
    };
    let target = path.to_string_lossy();
    for line in contents.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[1] == target.as_ref() {
            return true;
        }
    }
    false
}

/// Recursively copies all non-whited-out files from the overlay upperdir to the
/// destination root, applying overlay whiteouts as deletions.
/// Returns the count of changes applied.
fn sync_upper(upper_dir: &Path, dest_root: &Path) -> Result<usize, String> {
    let mut count = 0;
    sync_upper_dir(upper_dir, upper_dir, dest_root, &mut count)?;
    Ok(count)
}

/// Recursive helper for `sync_upper` that walks a directory tree and copies files.
fn sync_upper_dir(base: &Path, current: &Path, dest_root: &Path, count: &mut usize) -> Result<(), String> {
    let relative = current.strip_prefix(base).map_err(|e| format!("failed to compute relative path: {e}"))?;
    let dest_dir = dest_root.join(relative);
    let mut entries = Vec::new();
    let mut opaque = false;

    for entry in fs::read_dir(current).map_err(|e| format!("failed to read {}: {e}", current.display()))? {
        let entry = entry.map_err(|e| format!("failed to read entry: {e}"))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name == ".wh..wh..opq" {
            opaque = true;
        }
        entries.push((name.to_string(), path));
    }

    if opaque && dest_dir.exists() {
        *count += clear_dir_contents(&dest_dir)?;
    }

    for (name, path) in entries {
        if name == ".wh..wh..opq" {
            continue;
        }
        if let Some(target_name) = name.strip_prefix(".wh.") {
            let target = dest_dir.join(target_name);
            if remove_path_if_exists(&target)? {
                *count += 1;
            }
            continue;
        }

        let metadata = fs::symlink_metadata(&path).map_err(|e| format!("failed to stat {}: {e}", path.display()))?;
        let dest = dest_root.join(path.strip_prefix(base).map_err(|e| format!("failed to compute relative path: {e}"))?);

        if metadata.file_type().is_symlink() {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
            }
            let target = fs::read_link(&path).map_err(|e| format!("failed to read symlink {}: {e}", path.display()))?;
            let needs_link = fs::read_link(&dest).map(|current| current != target).unwrap_or(true);
            if needs_link {
                remove_path_if_exists(&dest)?;
                symlink(&target, &dest).map_err(|e| format!("failed to create symlink {}: {e}", dest.display()))?;
                *count += 1;
            }
            continue;
        }

        if metadata.is_dir() {
            fs::create_dir_all(&dest).map_err(|e| format!("failed to create {}: {e}", dest.display()))?;
            sync_upper_dir(base, &path, dest_root, count)?;
            continue;
        }

        if metadata.is_file() {
            let needs_copy = fs::read(&path).ok().and_then(|src| fs::read(&dest).ok().map(|dest_data| src != dest_data)).unwrap_or(true);
            if needs_copy {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
                }
                fs::copy(&path, &dest).map_err(|e| format!("failed to copy {} to {}: {e}", path.display(), dest.display()))?;
                fs::set_permissions(&dest, fs::Permissions::from_mode(metadata.permissions().mode()))
                    .map_err(|e| format!("failed to chmod {}: {e}", dest.display()))?;
                *count += 1;
            }
        }
    }
    Ok(())
}

fn flush_dir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    clear_dir_contents(dir)?;
    Ok(())
}

fn clear_dir_contents(dir: &Path) -> Result<usize, String> {
    let mut count = 0;
    if !dir.exists() {
        return Ok(0);
    }
    for entry in fs::read_dir(dir).map_err(|e| format!("failed to read {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("failed to read entry: {e}"))?;
        if remove_path_if_exists(&entry.path())? {
            count += 1;
        }
    }
    Ok(count)
}

fn remove_path_if_exists(path: &Path) -> Result<bool, String> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(false);
    };

    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).map_err(|e| format!("failed to remove {}: {e}", path.display()))?;
    } else {
        fs::remove_file(path).map_err(|e| format!("failed to remove {}: {e}", path.display()))?;
    }

    Ok(true)
}

/// Cleans up the overlay workspace: warns about unsynced changes and unmounts overlay,
/// overlay, loop device, and lower-dir bind mount in reverse order while preserving session state.
impl Drop for CowWorkspace {
    fn drop(&mut self) {
        if self.has_unsynced_changes() {
            eprintln!("bunkerbox: session ending. Changes not synced. Run 'bunkerbox sync' to save.");
        }

        let _ = run_command_allow_failure("umount", &[&self.mount_point.to_string_lossy()]);
        let _ = run_command_allow_failure("umount", &[&self.loop_mount.to_string_lossy()]);

        for mnt in parse_mounts_under(&self.lower_root).iter().rev() {
            let _ = run_command_allow_failure("umount", &[&mnt.to_string_lossy()]);
        }
        let _ = run_command_allow_failure("umount", &[&self.lower_root.to_string_lossy()]);
        let _ = run_command_allow_failure("umount", &["-l", &self.lower_root.to_string_lossy()]);
        let _ = fs::remove_dir(&self.lower_root);
        let _ = fs::remove_dir(&self.loop_mount);
        if let Some(dev) = find_loop_device(&self.loopback) {
            if !dev.is_empty() {
                let _ = run_command_allow_failure("losetup", &["-d", &dev]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{cow_paths, flush_dir, sync_upper};
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    #[test]
    fn sync_upper_applies_whiteouts_and_opaque_dirs() {
        let temp = tempdir().unwrap();
        let upper = temp.path().join("upper");
        let dest = temp.path().join("dest");

        fs::create_dir_all(upper.join("dir")).unwrap();
        fs::create_dir_all(dest.join("dir")).unwrap();
        fs::write(dest.join("keep.txt"), "old").unwrap();
        fs::write(dest.join("remove.txt"), "gone").unwrap();
        fs::write(dest.join("dir/old.txt"), "old dir file").unwrap();

        fs::write(upper.join("keep.txt"), "new").unwrap();
        fs::write(upper.join(".wh.remove.txt"), "").unwrap();
        fs::write(upper.join("dir/.wh..wh..opq"), "").unwrap();
        fs::write(upper.join("dir/new.txt"), "new dir file").unwrap();

        let count = sync_upper(&upper, &dest).unwrap();

        assert_eq!(count, 4);
        assert_eq!(fs::read_to_string(dest.join("keep.txt")).unwrap(), "new");
        assert!(!dest.join("remove.txt").exists());
        assert!(!dest.join("dir/old.txt").exists());
        assert_eq!(fs::read_to_string(dest.join("dir/new.txt")).unwrap(), "new dir file");
    }

    #[test]
    fn flush_dir_clears_existing_contents() {
        let temp = tempdir().unwrap();
        let dir = temp.path().join("upper");

        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("file.txt"), "x").unwrap();
        fs::write(dir.join("nested/file.txt"), "y").unwrap();

        flush_dir(&dir).unwrap();

        let remaining = fs::read_dir(&dir).unwrap().count();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn cow_paths_stay_inside_project_bunkerbox() {
        let repo_root = Path::new("/repo");
        let paths = cow_paths(repo_root);

        assert_eq!(paths.overlay_dir, Path::new("/repo/.bunkerbox"));
        assert_eq!(paths.loopback, Path::new("/repo/.bunkerbox/upper.img"));
        assert_eq!(paths.mounts_dir, Path::new("/repo/.bunkerbox/mounts"));
        assert_eq!(paths.loop_mount, Path::new("/repo/.bunkerbox/mounts/loop"));
        assert_eq!(paths.upper_dir, Path::new("/repo/.bunkerbox/mounts/loop/upper"));
        assert_eq!(paths.work_dir, Path::new("/repo/.bunkerbox/mounts/loop/work"));
        assert_eq!(paths.mount_point, Path::new("/repo/.bunkerbox/workspace"));
        assert_eq!(paths.lower_root, Path::new("/repo/.bunkerbox/mounts/lower"));
        assert!(!paths.overlay_dir.join("build-workspace").exists());
    }
}
