#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub fn sandbox_bin() -> PathBuf {
    let test_exe = std::env::current_exe().expect("current_exe");
    test_exe.parent().unwrap().parent().unwrap().join("bunkerbox-sandbox")
}

pub fn has_user_namespaces() -> bool {
    if std::env::var("BUNKERBOX_TEST_SKIP_USER_NS").is_ok() {
        return false;
    }
    let sandbox = sandbox_bin();
    if !sandbox.exists() {
        return false;
    }
    let output = Command::new(&sandbox)
        .args(["--share", "/nonexistent", "--profile", "make", "--", "/bin/true"])
        .output();
    match output {
        Ok(o) => {
            if o.status.success() {
                return true;
            }
            let stderr = String::from_utf8_lossy(&o.stderr);
            if stderr.contains("unshare failed")
                || stderr.contains("Operation not permitted")
                || stderr.contains("uid_map")
                || stderr.contains("gid_map")
            {
                return false;
            }
            o.status.code() == Some(1) && stderr.is_empty()
        }
        Err(_) => false,
    }
}

pub fn assume_user_ns() {
    if !has_user_namespaces() {
        eprintln!("SKIP: user namespaces not available. Set BUNKERBOX_TEST_SKIP_USER_NS=1 to explicitly skip.");
        if std::env::var("BUNKERBOX_TEST_REQUIRE_USER_NS").is_ok() {
            panic!("BUNKERBOX_TEST_REQUIRE_USER_NS is set and user namespaces are unavailable");
        }
    }
}

pub fn require_user_ns() -> bool {
    if has_user_namespaces() {
        true
    } else {
        eprintln!("SKIP: user namespaces not available. Set BUNKERBOX_TEST_SKIP_USER_NS=1 to explicitly skip.");
        false
    }
}

pub fn write_temp_profile(binaries: &[(&str, &str)], ro_dirs: &[&str], rw_dirs: &[&str]) -> PathBuf {
    let mut yaml = String::from("name: temp-test\n\nbinaries:\n");
    for (name, path) in binaries {
        yaml.push_str(&format!("  {name}: {path}\n"));
    }
    yaml.push_str("\nro_dirs:\n");
    for dir in ro_dirs {
        yaml.push_str(&format!("  - {dir}\n"));
    }
    yaml.push_str("\nrw_dirs:\n");
    for dir in rw_dirs {
        yaml.push_str(&format!("  - {dir}\n"));
    }
    yaml.push_str("\nnetwork: none\nshell: /bin/sh\n");

    let dir = std::env::temp_dir().join(format!("bunkerbox-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("profile.yaml");
    std::fs::write(&path, yaml).expect("write temp profile");
    path
}

pub fn run_sandbox(profile: &Path, args: &[&str]) -> Output {
    let sandbox = sandbox_bin();
    let mut cmd = Command::new(&sandbox);
    cmd.arg("--share").arg("/usr/share/bunkerbox");
    cmd.arg("--profile").arg(profile);
    cmd.arg("--");
    for a in args {
        cmd.arg(a);
    }
    cmd.output().expect("spawn sandbox")
}

pub fn run_sandbox_named(profile_name: &str, args: &[&str]) -> Output {
    let sandbox = sandbox_bin();
    let mut cmd = Command::new(&sandbox);
    cmd.arg("--share").arg("/usr/share/bunkerbox");
    cmd.arg("--profile").arg(profile_name);
    cmd.arg("--");
    for a in args {
        cmd.arg(a);
    }
    cmd.output().expect("spawn sandbox")
}

pub fn run_sandbox_raw(args: &[&str]) -> Output {
    Command::new(sandbox_bin())
        .args(args)
        .output()
        .expect("spawn sandbox")
}

pub fn assert_success(output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "expected success, stderr: {stderr}");
}

pub fn assert_failure(output: &Output) {
    assert!(!output.status.success(), "expected failure");
}
