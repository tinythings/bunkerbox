use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::envconf::EnvConfig;

/// A copy-on-write workspace using overlayfs and a loop-mounted ext4 image for upper storage.
pub struct CowWorkspace {
    pub mount_point: PathBuf,
    overlay_dir: PathBuf,
    loopback: PathBuf,
    loop_mount: PathBuf,
    bind_mounts: Vec<PathBuf>,
}

/// Persisted state for an active overlay session, saved so it can be synced later.
#[derive(serde::Serialize, serde::Deserialize)]
struct SessionState {
    app_name: String,
    mount_point: String,
    upper_dir: String,
    repo_root: String,
}

impl CowWorkspace {
    /// Creates and mounts an overlay filesystem workspace with quota-limited storage,
    /// bind-mounting excluded paths to separate directories outside the overlay.
    pub fn setup(
        repo_root: &Path, env_config: &EnvConfig, runtime_quota: u64, runtime_exclude: Option<&[String]>, app_name: &str,
    ) -> Result<Self, String> {
        let overlay_dir = repo_root.join(".bunkerbox");
        let loopback = overlay_dir.join("upper.img");
        let loop_mount = overlay_dir.join("upper-mount");
        let upper_dir = loop_mount.join("upper");
        let work_dir = loop_mount.join("work");
        let mount_point = overlay_dir.join("workspace");

        Self::cleanup_stale(&mount_point, &loop_mount, &loopback)?;

        fs::create_dir_all(&overlay_dir).map_err(|e| format!("failed to create {}: {e}", overlay_dir.display()))?;

        Self::ensure_gitignore(repo_root)?;

        let size_mb = env_config.quota_bytes(runtime_quota, repo_root, runtime_exclude)? / (1024 * 1024);
        if size_mb == 0 {
            return Err("workspace quota too small".to_string());
        }
        Self::run_command("dd", &["if=/dev/zero", &format!("of={}", loopback.display()), "bs=1M", &format!("count={size_mb}"), "status=none"])?;
        Self::run_command("mkfs.ext4", &["-F", &loopback.to_string_lossy()])?;

        fs::create_dir_all(&loop_mount).map_err(|e| format!("failed to create {}: {e}", loop_mount.display()))?;
        Self::run_command("mount", &["-o", "loop", &loopback.to_string_lossy(), &loop_mount.to_string_lossy()])?;

        let user = current_user_spec()?;
        Self::run_command("chown", &[&user, &loop_mount.to_string_lossy()])?;

        fs::create_dir_all(&upper_dir).map_err(|e| format!("failed to create {}: {e}", upper_dir.display()))?;
        fs::create_dir_all(&work_dir).map_err(|e| format!("failed to create {}: {e}", work_dir.display()))?;

        fs::create_dir_all(&mount_point).map_err(|e| format!("failed to create {}: {e}", mount_point.display()))?;
        let lowerdir = repo_root.to_string_lossy();
        let upperdir = upper_dir.to_string_lossy();
        let workdir = work_dir.to_string_lossy();
        if Self::run_command_allow_failure("mount", &["-t", "overlay", "overlay", "-o", &format!("lowerdir={lowerdir},upperdir={upperdir},workdir={workdir},redirect_dir=on"), &mount_point.to_string_lossy()]).is_err() {
            Self::run_command("mount", &["-t", "overlay", "overlay", "-o", &format!("lowerdir={lowerdir},upperdir={upperdir},workdir={workdir}"), &mount_point.to_string_lossy()])?;
        }

        let mut bind_mounts: Vec<PathBuf> = Vec::new();
        for pattern in env_config.effective_exclude(runtime_exclude) {
            let src_path = repo_root.join(&pattern);

            if src_path.exists() {
                if let Ok(meta) = fs::symlink_metadata(&src_path) {
                    if meta.file_type().is_symlink() {
                        eprintln!("bunkerbox: warning: {} is a symlink, skipping bind mount", src_path.display());
                        continue;
                    }
                }
            }

            let bind_src = overlay_dir.join("build-workspace").join(&pattern);
            let bind_dst = mount_point.join(&pattern);

            fs::create_dir_all(&bind_src).map_err(|e| format!("failed to create {}: {e}", bind_src.display()))?;
            fs::create_dir_all(&bind_dst).map_err(|e| format!("failed to create {}: {e}", bind_dst.display()))?;

            Self::run_command("mount", &["--bind", &bind_src.to_string_lossy(), &bind_dst.to_string_lossy()])?;

            bind_mounts.push(bind_dst);
        }

        let sessions_dir = overlay_dir.join("sessions");
        fs::create_dir_all(&sessions_dir).map_err(|e| format!("failed to create {}: {e}", sessions_dir.display()))?;
        let state = SessionState {
            app_name: app_name.to_string(),
            mount_point: mount_point.to_string_lossy().to_string(),
            upper_dir: upper_dir.to_string_lossy().to_string(),
            repo_root: repo_root.to_string_lossy().to_string(),
        };
        fs::write(
            sessions_dir.join(format!("{app_name}.json")),
            serde_json::to_string(&state).map_err(|e| format!("failed to serialize session state: {e}"))?,
        ).map_err(|e| format!("failed to write session state: {e}"))?;

        Ok(CowWorkspace { mount_point, overlay_dir, loopback, loop_mount, bind_mounts })
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

    /// Unmounts stale overlay and loop mounts from previous runs, then detaches the loop device.
    fn cleanup_stale(mount_point: &Path, loop_mount: &Path, loopback: &Path) -> Result<(), String> {
        for mnt in parse_bind_mounts_under(mount_point).iter().rev() {
            Self::run_command_allow_failure("umount", &[&mnt.to_string_lossy()])?;
        }

        Self::run_command_allow_failure("umount", &[&mount_point.to_string_lossy()])?;

        Self::run_command_allow_failure("umount", &[&loop_mount.to_string_lossy()])?;

        if loopback.exists() {
            Self::run_command_allow_failure("losetup", &["-d", &Self::find_loop_device(loopback).unwrap_or_default()])?;
        }

        Ok(())
    }

    /// Finds the loop device (e.g. `/dev/loop0`) associated with a backing file via `losetup -j`.
    fn find_loop_device(loopback: &Path) -> Option<String> {
        let output =
            Command::new("sudo").args(["losetup", "-j", &loopback.to_string_lossy()]).stdout(Stdio::piped()).stderr(Stdio::null()).output().ok()?;

        String::from_utf8(output.stdout).ok()?.split(':').next().map(|s| s.trim().to_string())
    }

    /// Runs a command via `sudo`, returning an error on non-zero exit or failure to spawn.
    fn run_command(program: &str, args: &[&str]) -> Result<(), String> {
        let mut all_args: Vec<&str> = vec![program];
        all_args.extend_from_slice(args);
        let status = Command::new("sudo")
            .args(&all_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .map_err(|e| format!("failed to run sudo {program}: {e}"))?;

        if !status.success() {
            return Err(format!("sudo {program} failed with status {status}"));
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

    /// Recursively checks if the overlay upperdir has any unsynced file changes.
    fn has_unsynced_changes(&self) -> bool {
        fn check(dir: &Path) -> bool {
            let Ok(entries) = fs::read_dir(dir) else { return false };
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
                if name.starts_with('.') || name.ends_with(".wh") {
                    continue;
                }
                if path.is_dir() {
                    if check(&path) {
                        return true;
                    }
                    continue;
                }
                if path.is_file() {
                    return true;
                }
            }
            false
        }
        check(&self.loop_mount.join("upper"))
    }
}

/// Parses `/proc/mounts` for bind mounts nested under the given mount point,
/// returning them sorted deepest-first for correct unmount ordering.
fn parse_bind_mounts_under(mount_point: &Path) -> Vec<PathBuf> {
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
            if path.file_name().is_some_and(|n| n.to_string_lossy().ends_with("-manifest.json")) {
                continue;
            }
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
        sync_session(&sessions_dir, name, path)?;
    }

    Ok(())
}

/// Syncs changes from a single session's overlay upperdir to the repo root,
/// then writes a `.synced` marker file.
fn sync_session(sessions_dir: &Path, name: &str, state_path: &Path) -> Result<(), String> {
    let state: SessionState = serde_json::from_str(&fs::read_to_string(state_path).map_err(|e| format!("failed to read session state: {e}"))?).map_err(|e| format!("failed to parse session state: {e}"))?;

    if !is_mounted(Path::new(&state.mount_point)) {
        println!("{name}: session ended, removing state file");
        let _ = fs::remove_file(state_path);
        return Ok(());
    }

    let (added, deleted) = sync_upper(Path::new(&state.upper_dir), Path::new(&state.repo_root), sessions_dir, name)?;

    if added > 0 || deleted > 0 {
        let mut parts = Vec::new();
        if added > 0 {
            parts.push(format!("{added} added"));
        }
        if deleted > 0 {
            parts.push(format!("{deleted} deleted"));
        }
        println!("{name}: {}", parts.join(", "));
        fs::write(sessions_dir.join(format!("{name}.synced")), "1").map_err(|e| format!("failed to write sync marker: {e}"))?;
    } else {
        println!("{name}: no changes");
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

/// Recursively copies non-whited-out files from the overlay upperdir to the repo root,
/// processes whiteouts (deletions) and opaque markers, and tracks state via a manifest.
/// Returns counts of files added and deleted.
fn sync_upper(upper_dir: &Path, repo_root: &Path, sessions_dir: &Path, app_name: &str) -> Result<(usize, usize), String> {
    let manifest_path = sessions_dir.join(format!("{app_name}-manifest.json"));
    let manifest_old = load_manifest(&manifest_path);
    let mut manifest_new = BTreeSet::new();
    let mut count_add = 0;
    let mut count_del = 0;

    sync_upper_dir(upper_dir, upper_dir, repo_root, &mut count_add, &mut count_del, &manifest_old, &mut manifest_new)?;

    for path in manifest_old.difference(&manifest_new) {
        let target = repo_root.join(path);
        if target.exists() {
            if target.is_file() || target.is_symlink() {
                fs::remove_file(&target).map_err(|e| format!("failed to remove {}: {e}", target.display()))?;
            } else if target.is_dir() {
                fs::remove_dir_all(&target).map_err(|e| format!("failed to remove {}: {e}", target.display()))?;
            }
            count_del += 1;
        }
    }

    save_manifest(&manifest_path, &manifest_new)?;
    Ok((count_add, count_del))
}

/// Recursive helper that walks an upperdir directory, syncing files to repo_root,
/// processing whiteouts and opaque markers, and building the new manifest.
fn sync_upper_dir(
    base: &Path, current: &Path, repo_root: &Path,
    count_add: &mut usize, count_del: &mut usize,
    _manifest_old: &BTreeSet<String>, manifest_new: &mut BTreeSet<String>,
) -> Result<(), String> {
    let mut has_opq = false;

    for entry in fs::read_dir(current).map_err(|e| format!("failed to read {}: {e}", current.display()))? {
        let entry = entry.map_err(|e| format!("failed to read entry: {e}"))?;
        let path = entry.path();
        let metadata = entry.metadata().map_err(|e| format!("failed to stat {}: {e}", path.display()))?;

        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        let file_type = metadata.file_type();

        if file_type.is_char_device() {
            if name == ".wh..wh..opq" {
                has_opq = true;
                continue;
            }
            if let Some(target_name) = name.strip_prefix(".wh.") {
                let rel_parent = current.strip_prefix(base).map_err(|e| format!("strip prefix: {e}"))?;
                let target = repo_root.join(rel_parent).join(target_name);
                if target.exists() {
                    if target.is_file() || target.is_symlink() {
                        fs::remove_file(&target).map_err(|e| format!("failed to remove {}: {e}", target.display()))?;
                    } else if target.is_dir() {
                        fs::remove_dir_all(&target).map_err(|e| format!("failed to remove {}: {e}", target.display()))?;
                    }
                    *count_del += 1;
                }
                let _ = fs::remove_file(&path);
            }
            continue;
        }

        if file_type.is_symlink() {
            continue;
        }

        let rel = path.strip_prefix(base).map_err(|e| format!("failed to compute relative path: {e}"))?;
        let rel_str = rel.to_string_lossy().to_string();
        let dest = repo_root.join(rel);

        if metadata.is_dir() {
            sync_upper_dir(base, &path, repo_root, count_add, count_del, _manifest_old, manifest_new)?;
            continue;
        }

        if metadata.is_file() {
            let needs_copy = fs::read(&path)
                .ok()
                .and_then(|src| fs::read(&dest).ok().map(|dest_data| src != dest_data))
                .unwrap_or(true);
            if needs_copy {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
                }
                fs::copy(&path, &dest).map_err(|e| format!("failed to copy {} to {}: {e}", path.display(), dest.display()))?;
                *count_add += 1;
            }
            manifest_new.insert(rel_str);
        }
    }

    if has_opq {
        let rel_dir = current.strip_prefix(base).unwrap_or(Path::new(""));
        let repo_dir = repo_root.join(rel_dir);
        if repo_dir.exists() {
            delete_opaque_lower_files(&repo_dir, repo_root, manifest_new, count_del)?;
        }
        let _ = fs::remove_file(current.join(".wh..wh..opq"));
    }

    Ok(())
}

/// Deletes files from repo_dir that are hidden by an opaque marker — any file not
/// present in the current upperdir manifest is removed from the host repo.
fn delete_opaque_lower_files(
    repo_dir: &Path, repo_root: &Path,
    manifest_new: &BTreeSet<String>, count_del: &mut usize,
) -> Result<(), String> {
    for entry in fs::read_dir(repo_dir).map_err(|e| format!("failed to read {}: {e}", repo_dir.display()))? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else { continue };

        let Ok(rel) = path.strip_prefix(repo_root) else { continue };
        let rel_str = rel.to_string_lossy().to_string();

        if metadata.is_dir() {
            delete_opaque_lower_files(&path, repo_root, manifest_new, count_del)?;
        } else if (metadata.is_file() || metadata.is_symlink()) && !manifest_new.contains(&rel_str) {
            fs::remove_file(&path).map_err(|e| format!("failed to remove {}: {e}", path.display()))?;
            *count_del += 1;
        }
    }
    Ok(())
}

fn load_manifest(path: &Path) -> BTreeSet<String> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_manifest(path: &Path, manifest: &BTreeSet<String>) -> Result<(), String> {
    let sorted: Vec<&String> = manifest.iter().collect();
    let json = serde_json::to_string(&sorted).map_err(|e| format!("failed to serialize manifest: {e}"))?;
    fs::write(path, json).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

/// Cleans up the overlay workspace: warns about unsynced changes, removes session state,
/// and unmounts the overlay, loop device, and bind mounts.
impl Drop for CowWorkspace {
    fn drop(&mut self) {
        let sessions_dir = self.overlay_dir.join("sessions");

        for entry in fs::read_dir(&sessions_dir).into_iter().flatten() {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let synced_marker = sessions_dir.join(format!("{}.synced", path.file_stem().unwrap_or_default().to_string_lossy()));
                if !synced_marker.exists() && self.has_unsynced_changes() {
                    eprintln!("bunkerbox: session ending. Changes not synced. Run 'bunkerbox sync' to save.");
                }
                let _ = fs::remove_file(&path);
                let _ = fs::remove_file(&synced_marker);
            }
        }

        let _ = fs::remove_dir_all(&sessions_dir);

        for mnt in self.bind_mounts.iter().rev() {
            let _ = Self::run_command_allow_failure("umount", &[&mnt.to_string_lossy()]);
        }
        let _ = Self::run_command_allow_failure("umount", &[&self.mount_point.to_string_lossy()]);
        let _ = Self::run_command_allow_failure("umount", &[&self.loop_mount.to_string_lossy()]);
        if let Some(dev) = Self::find_loop_device(&self.loopback) {
            if !dev.is_empty() {
                let _ = Self::run_command_allow_failure("losetup", &["-d", &dev]);
            }
        }
    }
}
