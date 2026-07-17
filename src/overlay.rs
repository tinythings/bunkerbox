use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::envconf::EnvConfig;

pub struct CowWorkspace {
    pub mount_point: PathBuf,
    overlay_dir: PathBuf,
    loopback: PathBuf,
    loop_mount: PathBuf,
    bind_mounts: Vec<PathBuf>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SessionState {
    app_name: String,
    mount_point: String,
    repo_root: String,
}

impl CowWorkspace {
    pub fn setup(repo_root: &Path, env_config: &EnvConfig, runtime_quota: u64, app_name: &str) -> Result<Self, String> {
        let overlay_dir = repo_root.join(".bunkerbox");
        let loopback = overlay_dir.join("upper.img");
        let loop_mount = overlay_dir.join("upper-mount");
        let upper_dir = loop_mount.join("upper");
        let work_dir = loop_mount.join("work");
        let mount_point = overlay_dir.join("workspace");

        Self::cleanup_stale(&mount_point, &loop_mount, &loopback)?;

        fs::create_dir_all(&overlay_dir).map_err(|e| format!("failed to create {}: {e}", overlay_dir.display()))?;

        Self::ensure_gitignore(repo_root)?;

        let quota_bytes = env_config.quota_bytes(runtime_quota, repo_root)?;

        let size_mb = quota_bytes / (1024 * 1024);
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
        let opts = format!("lowerdir={lowerdir},upperdir={upperdir},workdir={workdir},redirect_dir=on");
        let result = Self::run_command_allow_failure("mount", &["-t", "overlay", "overlay", "-o", &opts, &mount_point.to_string_lossy()]);
        if result.is_err() {
            let opts_no_redirect = format!("lowerdir={lowerdir},upperdir={upperdir},workdir={workdir}");
            Self::run_command("mount", &["-t", "overlay", "overlay", "-o", &opts_no_redirect, &mount_point.to_string_lossy()])?;
        }

        let mut bind_mounts: Vec<PathBuf> = Vec::new();
        let build_workspace = overlay_dir.join("build-workspace");
        for pattern in env_config.effective_exclude() {
            let src_path = repo_root.join(&pattern);

            if src_path.exists() {
                if let Ok(meta) = fs::symlink_metadata(&src_path) {
                    if meta.file_type().is_symlink() {
                        eprintln!("bunkerbox: warning: {} is a symlink, skipping bind mount", src_path.display());
                        continue;
                    }
                }
            }

            let bind_src = build_workspace.join(&pattern);
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
            repo_root: repo_root.to_string_lossy().to_string(),
        };
        let state_json = serde_json::to_string(&state).map_err(|e| format!("failed to serialize session state: {e}"))?;
        let state_path = sessions_dir.join(format!("{app_name}.json"));
        fs::write(&state_path, state_json).map_err(|e| format!("failed to write session state: {e}"))?;

        Ok(CowWorkspace { mount_point, overlay_dir, loopback, loop_mount, bind_mounts })
    }

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

    fn find_loop_device(loopback: &Path) -> Option<String> {
        let output =
            Command::new("sudo").args(["losetup", "-j", &loopback.to_string_lossy()]).stdout(Stdio::piped()).stderr(Stdio::null()).output().ok()?;

        let stdout = String::from_utf8(output.stdout).ok()?;
        stdout.split(':').next().map(|s| s.trim().to_string())
    }

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
}

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

fn current_user_spec() -> Result<String, String> {
    let uid = run_id_arg("-u")?;
    let gid = run_id_arg("-g")?;
    if uid.is_empty() || gid.is_empty() || !uid.chars().all(|ch| ch.is_ascii_digit()) || !gid.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(format!("failed to determine current uid/gid: {uid}:{gid}"));
    }
    Ok(format!("{uid}:{gid}"))
}

fn run_id_arg(arg: &str) -> Result<String, String> {
    let output = Command::new("id").arg(arg).output().map_err(|e| format!("id {arg}: {e}"))?;
    if !output.status.success() {
        return Err(format!("id {arg} failed"));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("id {arg}: {e}")).map(|s| s.trim().to_string())
}

pub fn sync_sessions(repo_root: &Path, app_name: Option<&str>) -> Result<(), String> {
    let sessions_dir = repo_root.join(".bunkerbox/sessions");

    if !sessions_dir.is_dir() {
        println!("No active sessions.");
        return Ok(());
    }

    let mut sessions = Vec::new();
    for entry in fs::read_dir(&sessions_dir).map_err(|e| format!("failed to read {}: {e}", sessions_dir.display()))? {
        let entry = entry.map_err(|e| format!("failed to read session entry: {e}"))?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            if let Some(file_stem) = path.file_stem() {
                let name = file_stem.to_string_lossy().to_string();
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

fn sync_session(sessions_dir: &Path, name: &str, state_path: &Path) -> Result<(), String> {
    let contents = fs::read_to_string(state_path).map_err(|e| format!("failed to read session state: {e}"))?;
    let state: SessionState = serde_json::from_str(&contents).map_err(|e| format!("failed to parse session state: {e}"))?;

    let mount_point = Path::new(&state.mount_point);
    let repo_root = Path::new(&state.repo_root);

    if !is_mounted(mount_point) {
        println!("{name}: session ended, removing state file");
        let _ = fs::remove_file(state_path);
        return Ok(());
    }

    let (changed, created) = sync_files(mount_point, repo_root)?;

    if changed == 0 && created == 0 {
        println!("{name}: no changes");
    } else {
        println!("{name}: {changed} files changed, {created} new");
        let synced_path = sessions_dir.join(format!("{name}.synced"));
        fs::write(&synced_path, "1").map_err(|e| format!("failed to write sync marker: {e}"))?;
    }

    Ok(())
}

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

fn sync_files(mount_point: &Path, repo_root: &Path) -> Result<(usize, usize), String> {
    let mut changed = 0;
    let mut created = 0;
    sync_dir(mount_point, repo_root, mount_point, &mut changed, &mut created)?;
    Ok((changed, created))
}

fn sync_dir(base: &Path, repo_root: &Path, current: &Path, changed: &mut usize, created: &mut usize) -> Result<(), String> {
    for entry in fs::read_dir(current).map_err(|e| format!("failed to read {}: {e}", current.display()))? {
        let entry = entry.map_err(|e| format!("failed to read entry: {e}"))?;
        let path = entry.path();
        let metadata = entry.metadata().map_err(|e| format!("failed to stat {}: {e}", path.display()))?;

        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if name == ".bunkerbox" || name == ".git" || name == ".bunker" {
            continue;
        }

        let rel = path.strip_prefix(base).map_err(|e| format!("failed to compute relative path: {e}"))?;
        let dest = repo_root.join(rel);

        if metadata.file_type().is_symlink() {
            continue;
        }

        if metadata.is_dir() {
            fs::create_dir_all(&dest).map_err(|e| format!("failed to create {}: {e}", dest.display()))?;
            sync_dir(base, repo_root, &path, changed, created)?;
        } else if metadata.is_file() {
            let copy = match fs::metadata(&dest) {
                Ok(dest_meta) => dest_meta.len() != metadata.len() || dest_meta.modified().ok() != metadata.modified().ok(),
                Err(_) => true,
            };

            if copy {
                if dest.exists() {
                    *changed += 1;
                } else {
                    *created += 1;
                }
                fs::copy(&path, &dest).map_err(|e| format!("failed to copy {} to {}: {e}", path.display(), dest.display()))?;
            }
        }
    }
    Ok(())
}

impl Drop for CowWorkspace {
    fn drop(&mut self) {
        let sessions_dir = self.overlay_dir.join("sessions");

        for entry in fs::read_dir(&sessions_dir).into_iter().flatten() {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let synced_marker = sessions_dir.join(format!("{}.synced", path.file_stem().unwrap_or_default().to_string_lossy()));
                if !synced_marker.exists() {
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
