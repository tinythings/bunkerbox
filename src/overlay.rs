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

impl CowWorkspace {
    pub fn setup(repo_root: &Path, env_config: &EnvConfig, runtime_quota: u64, app_name: &str) -> Result<Self, String> {
        let overlay_dir = repo_root.join(".bunkerbox");
        let loopback = overlay_dir.join("upper.img");
        let loop_mount = overlay_dir.join("upper-mount");
        let upper_dir = loop_mount.join("upper");
        let work_dir = loop_mount.join("work");
        let mount_point = PathBuf::from(format!("/tmp/bunkerbox-ws-{app_name}"));

        Self::cleanup_stale(&mount_point, &loop_mount, &loopback)?;

        fs::create_dir_all(&overlay_dir)
            .map_err(|e| format!("failed to create {}: {e}", overlay_dir.display()))?;

        Self::ensure_gitignore(repo_root)?;

        let quota_bytes = env_config.quota_bytes(runtime_quota, repo_root)?;

        let size_mb = quota_bytes / (1024 * 1024);
        if size_mb == 0 {
            return Err("workspace quota too small".to_string());
        }
        Self::run_command(
            "dd",
            &[
                "if=/dev/zero",
                &format!("of={}", loopback.display()),
                "bs=1M",
                &format!("count={size_mb}"),
                "status=none",
            ],
        )?;
        Self::run_command("mkfs.ext4", &["-F", &loopback.to_string_lossy()])?;

        fs::create_dir_all(&loop_mount)
            .map_err(|e| format!("failed to create {}: {e}", loop_mount.display()))?;
        Self::run_command(
            "mount",
            &["-o", "loop", &loopback.to_string_lossy(), &loop_mount.to_string_lossy()],
        )?;

        let user = current_user_spec()?;
        Self::run_command("chown", &[&user, &loop_mount.to_string_lossy()])?;

        fs::create_dir_all(&upper_dir)
            .map_err(|e| format!("failed to create {}: {e}", upper_dir.display()))?;
        fs::create_dir_all(&work_dir)
            .map_err(|e| format!("failed to create {}: {e}", work_dir.display()))?;

        fs::create_dir_all(&mount_point)
            .map_err(|e| format!("failed to create {}: {e}", mount_point.display()))?;
        let lowerdir = repo_root.to_string_lossy();
        let upperdir = upper_dir.to_string_lossy();
        let workdir = work_dir.to_string_lossy();
        let opts = format!("lowerdir={lowerdir},upperdir={upperdir},workdir={workdir},redirect_dir=on");
        let result = Self::run_command_allow_failure(
            "mount",
            &["-t", "overlay", "overlay", "-o", &opts, &mount_point.to_string_lossy()],
        );
        if result.is_err() {
            let opts_no_redirect = format!("lowerdir={lowerdir},upperdir={upperdir},workdir={workdir}");
            Self::run_command(
                "mount",
                &["-t", "overlay", "overlay", "-o", &opts_no_redirect, &mount_point.to_string_lossy()],
            )?;
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

            fs::create_dir_all(&bind_src)
                .map_err(|e| format!("failed to create {}: {e}", bind_src.display()))?;
            fs::create_dir_all(&bind_dst)
                .map_err(|e| format!("failed to create {}: {e}", bind_dst.display()))?;

            Self::run_command(
                "mount",
                &["--bind", &bind_src.to_string_lossy(), &bind_dst.to_string_lossy()],
            )?;

            bind_mounts.push(bind_dst);
        }

        Ok(CowWorkspace {
            mount_point,
            overlay_dir,
            loopback,
            loop_mount,
            bind_mounts,
        })
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
            let mut contents = if gitignore.exists() {
                fs::read_to_string(&gitignore).unwrap_or_default()
            } else {
                String::new()
            };
            if !contents.is_empty() && !contents.ends_with('\n') {
                contents.push('\n');
            }
            contents.push_str(".bunkerbox/\n");
            fs::write(&gitignore, contents)
                .map_err(|e| format!("failed to update .gitignore: {e}"))?;
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
            Self::run_command_allow_failure(
                "losetup",
                &["-d", &Self::find_loop_device(loopback).unwrap_or_default()],
            )?;
        }

        Ok(())
    }

    fn find_loop_device(loopback: &Path) -> Option<String> {
        let output = Command::new("sudo")
            .args(["losetup", "-j", &loopback.to_string_lossy()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;

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
    mounts.sort_by(|a, b| b.as_os_str().len().cmp(&a.as_os_str().len()));
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
    let output = Command::new("id")
        .arg(arg)
        .output()
        .map_err(|e| format!("id {arg}: {e}"))?;
    if !output.status.success() {
        return Err(format!("id {arg} failed"));
    }
    String::from_utf8(output.stdout)
        .map_err(|e| format!("id {arg}: {e}"))
        .map(|s| s.trim().to_string())
}

impl Drop for CowWorkspace {
    fn drop(&mut self) {
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
