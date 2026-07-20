use std::fs;
use std::os::unix;
use std::path::{Path, PathBuf};

use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::unistd;

use crate::sandbox::MergedProfile;

const SANDBOX_BIN_DIR: &str = "/usr/local/bunkerbox-bin";

pub fn build_sandbox_root(profile: &MergedProfile) -> Result<PathBuf, String> {
    let root = std::env::temp_dir().join(format!("bunkerbox-sandbox-{}", std::process::id()));
    fs::create_dir_all(&root).map_err(|e| format!("mkdir {}: {e}", root.display()))?;

    mount_root_tmpfs(&root)?;

    create_dirs_in_root(&root, &["dev", "proc", "tmp", "home", "old_root"])?;
    create_dirs_in_root(
        &root,
        &[
            "lib", "lib64", "usr", "etc", "workspace", SANDBOX_BIN_DIR.trim_start_matches('/'),
        ],
    )?;

    mount_dev(&root)?;
    mount_proc(&root)?;
    mount_tmp_tmpfs(&root)?;
    mount_home_tmpfs(&root)?;

    bind_all_ro(&root, &profile.ro_dirs)?;
    bind_all_rw(&root, &profile.rw_dirs)?;

    setup_binaries(&root, &profile.binaries)?;
    setup_shell(&root, &profile.shell)?;

    pivot_to(&root)?;

    Ok(root)
}

fn mount_root_tmpfs(root: &Path) -> Result<(), String> {
    mount(Some("none"), root, Some("tmpfs"), MsFlags::empty(), None::<&str>)
        .map_err(|e| format!("mount tmpfs root {}: {e}", root.display()))
}

fn create_dirs_in_root(root: &Path, dirs: &[&str]) -> Result<(), String> {
    for dir in dirs {
        let path = root.join(dir);
        fs::create_dir_all(&path).map_err(|e| format!("mkdir {}: {e}", path.display()))?;
    }
    Ok(())
}

fn mount_dev(root: &Path) -> Result<(), String> {
    let target = root.join("dev");
    mount(
        Some("/dev"),
        &target,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC | MsFlags::MS_RDONLY,
        None::<&str>,
    )
    .map_err(|e| format!("mount /dev: {e}"))
}

fn mount_proc(root: &Path) -> Result<(), String> {
    let target = root.join("proc");
    mount(Some("proc"), &target, Some("proc"), MsFlags::empty(), None::<&str>)
        .map_err(|e| format!("mount /proc: {e}"))
}

fn mount_tmp_tmpfs(root: &Path) -> Result<(), String> {
    let target = root.join("tmp");
    mount(Some("none"), &target, Some("tmpfs"), MsFlags::empty(), Some("mode=1777"))
        .map_err(|e| format!("mount /tmp: {e}"))
}

fn mount_home_tmpfs(root: &Path) -> Result<(), String> {
    let target = root.join("home");
    mount(Some("none"), &target, Some("tmpfs"), MsFlags::empty(), Some("mode=0755"))
        .map_err(|e| format!("mount /home: {e}"))?;

    if let Ok(user) = std::env::var("USER") {
        let user_home = target.join(&user);
        fs::create_dir_all(&user_home)
            .map_err(|e| format!("mkdir {}: {e}", user_home.display()))?;
        unistd::chown(&user_home, Some(unistd::Uid::from_raw(0)), Some(unistd::Gid::from_raw(0)))
            .map_err(|e| format!("chown {}: {e}", user_home.display()))?;
    }

    Ok(())
}

fn bind_all_ro(root: &Path, dirs: &[String]) -> Result<(), String> {
    for dir in dirs {
        let host_path = Path::new(dir);
        if !host_path.exists() {
            eprintln!("bunkerbox-sandbox: warning: ro path does not exist, skipping: {dir}");
            continue;
        }
        bind_ro(root, host_path)?;
    }
    Ok(())
}

fn bind_all_rw(root: &Path, dirs: &[String]) -> Result<(), String> {
    for dir in dirs {
        let host_path = Path::new(dir);
        if !host_path.exists() {
            eprintln!("bunkerbox-sandbox: warning: rw path does not exist, skipping: {dir}");
            continue;
        }
        bind_rw(root, host_path)?;
    }
    Ok(())
}

fn bind_ro(root: &Path, host_path: &Path) -> Result<(), String> {
    let rel = host_path.strip_prefix("/").unwrap_or(host_path);
    let target = root.join(rel);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    if !target.exists() {
        if host_path.is_dir() {
            fs::create_dir_all(&target)
                .map_err(|e| format!("mkdir {}: {e}", target.display()))?;
        } else {
            fs::File::create(&target)
                .map_err(|e| format!("create {}: {e}", target.display()))?;
        }
    }
    mount(
        Some(host_path),
        &target,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_RDONLY,
        None::<&str>,
    )
    .map_err(|e| format!("bind ro {} -> {}: {e}", host_path.display(), target.display()))
}

fn bind_rw(root: &Path, host_path: &Path) -> Result<(), String> {
    let rel = host_path.strip_prefix("/").unwrap_or(host_path);
    let target = root.join(rel);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    if !target.exists() {
        if host_path.is_dir() {
            fs::create_dir_all(&target)
                .map_err(|e| format!("mkdir {}: {e}", target.display()))?;
        } else {
            fs::File::create(&target)
                .map_err(|e| format!("create {}: {e}", target.display()))?;
        }
    }
    mount(Some(host_path), &target, None::<&str>, MsFlags::MS_BIND, None::<&str>)
        .map_err(|e| format!("bind rw {} -> {}: {e}", host_path.display(), target.display()))
}

fn setup_binaries(root: &Path, binaries: &std::collections::BTreeMap<String, PathBuf>) -> Result<(), String> {
    let bin_dir = root.join(SANDBOX_BIN_DIR.trim_start_matches('/'));
    for (name, host_path) in binaries {
        let real = host_path
            .canonicalize()
            .map_err(|e| format!("canonicalize {}: {e}", host_path.display()))?;

        let target_path = root.join(real.strip_prefix("/").unwrap_or(&real));
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        fs::File::create(&target_path)
            .map_err(|e| format!("create {}: {e}", target_path.display()))?;
        mount(Some(&real), &target_path, None::<&str>, MsFlags::MS_BIND | MsFlags::MS_RDONLY, None::<&str>)
            .map_err(|e| format!("bind binary {} -> {}: {e}", real.display(), target_path.display()))?;

        let symlink = bin_dir.join(name);
        if symlink.exists() {
            let _ = fs::remove_file(&symlink);
        }
        unix::fs::symlink(&real, &symlink)
            .map_err(|e| format!("symlink {} -> {}: {e}", symlink.display(), real.display()))?;
    }
    Ok(())
}

fn setup_shell(root: &Path, shell: &Path) -> Result<(), String> {
    let real = shell
        .canonicalize()
        .map_err(|e| format!("canonicalize shell {}: {e}", shell.display()))?;

    let target = root.join("bin/sh");
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::File::create(&target)
        .map_err(|e| format!("create {}: {e}", target.display()))?;
    mount(Some(&real), &target, None::<&str>, MsFlags::MS_BIND | MsFlags::MS_RDONLY, None::<&str>)
        .map_err(|e| format!("bind shell {} -> {}: {e}", real.display(), target.display()))
}

fn pivot_to(root: &Path) -> Result<(), String> {
    let old_root = root.join("old_root");
    let new_root_c = std::ffi::CString::new(root.to_string_lossy().as_bytes())
        .map_err(|_| "new_root path contains null")?;
    let old_root_c = std::ffi::CString::new(old_root.to_string_lossy().as_bytes())
        .map_err(|_| "old_root path contains null")?;

    unsafe {
        if libc::syscall(libc::SYS_pivot_root, new_root_c.as_ptr(), old_root_c.as_ptr()) < 0 {
            return Err(format!("pivot_root: {}", std::io::Error::last_os_error()));
        }
    }

    unistd::chdir("/").map_err(|e| format!("chdir /: {e}"))?;

    umount2("old_root", MntFlags::MNT_DETACH)
        .map_err(|e| format!("umount2 old_root: {e}"))?;

    let _ = fs::remove_dir("/old_root");
    Ok(())
}
