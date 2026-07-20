mod common;
use common::*;

fn require_sandbox() {
    let sandbox = sandbox_bin();
    assert!(
        sandbox.exists(),
        "bunkerbox-sandbox binary not found at {}. Build with: cargo build --bin bunkerbox-sandbox",
        sandbox.display()
    );
}

const SYSTEM_LIBS: &[&str] = &["/lib", "/lib64", "/usr/lib"];

#[test]
fn sandbox_runs_sh_through_make_profile() {
    require_sandbox();
    if !require_user_ns() {
        return;
    }

    let output = run_sandbox_named("make", &["/bin/sh", "-c", "echo ok"]);
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("ok"));
}

#[test]
fn sandbox_blocks_unlisted_binary() {
    require_sandbox();
    if !require_user_ns() {
        return;
    }

    let profile = write_temp_profile(&[("ls", "/usr/bin/ls")], SYSTEM_LIBS, &[]);
    let output = run_sandbox(&profile, &["/bin/true"]);
    assert_failure(&output);
}

#[test]
fn sandbox_blocks_absolute_path_gimp() {
    require_sandbox();
    if !require_user_ns() {
        return;
    }

    let output = run_sandbox_named("make", &["/usr/bin/gimp"]);
    assert_failure(&output);
}

#[test]
fn sandbox_blocks_arbitrary_host_usr_bin() {
    require_sandbox();
    if !require_user_ns() {
        return;
    }

    let output = run_sandbox_named("make", &["/bin/ls", "/usr/bin"]);
    assert_failure(&output);
}

#[test]
fn sandbox_only_sees_root_contents() {
    require_sandbox();
    if !require_user_ns() {
        return;
    }

    let output = run_sandbox_named("make", &["/bin/ls", "/"]);
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("dev"));
    assert!(stdout.contains("proc"));
    assert!(!stdout.contains("etc/resolv.conf"));
}

#[test]
fn sandbox_network_is_blocked() {
    require_sandbox();
    if !require_user_ns() {
        return;
    }

    let profile = write_temp_profile(&[("ping", "/usr/bin/ping")], SYSTEM_LIBS, &[]);
    let output = run_sandbox(&profile, &["ping", "-c", "1", "1.1.1.1"]);
    assert_failure(&output);
}

#[test]
fn sandbox_custom_profile_runs_allowed_binary() {
    require_sandbox();
    if !require_user_ns() {
        return;
    }

    let profile = write_temp_profile(&[("sh", "/usr/bin/sh")], SYSTEM_LIBS, &[]);
    let output = run_sandbox(&profile, &["sh", "-c", "echo allowed"]);
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("allowed"));
}

#[test]
fn sandbox_custom_profile_blocks_unrelated_binary() {
    require_sandbox();
    if !require_user_ns() {
        return;
    }

    let profile = write_temp_profile(&[("sh", "/usr/bin/sh")], SYSTEM_LIBS, &[]);
    let output = run_sandbox(&profile, &["/usr/bin/env"]);
    assert_failure(&output);
}

#[test]
fn sandbox_not_found_binary_fails() {
    require_sandbox();
    if !require_user_ns() {
        return;
    }

    let profile = write_temp_profile(&[("sh", "/usr/bin/sh")], SYSTEM_LIBS, &[]);
    let output = run_sandbox(&profile, &["/usr/bin/doesnotexist"]);
    assert_failure(&output);
}

#[test]
fn sandbox_command_with_args_works() {
    require_sandbox();
    if !require_user_ns() {
        return;
    }

    let profile = write_temp_profile(&[("sh", "/usr/bin/sh")], SYSTEM_LIBS, &[]);
    let output = run_sandbox(&profile, &["sh", "-c", "echo hello world && echo ok"]);
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello world"));
    assert!(stdout.contains("ok"));
}

#[test]
fn sandbox_fails_without_profiles() {
    require_sandbox();

    let output = run_sandbox_raw(&[
        "--share", "/usr/share/bunkerbox",
        "--", "/bin/true",
    ]);
    assert_failure(&output);
}

#[test]
fn sandbox_merges_two_profiles() {
    require_sandbox();
    if !require_user_ns() {
        return;
    }

    let p1 = write_temp_profile(&[("sh", "/usr/bin/sh")], SYSTEM_LIBS, &[]);
    let p2 = write_temp_profile(&[("true", "/usr/bin/true")], SYSTEM_LIBS, &[]);

    let sandbox = sandbox_bin();
    let output = std::process::Command::new(&sandbox)
        .arg("--share").arg("/usr/share/bunkerbox")
        .arg("--profile").arg(&p1)
        .arg("--profile").arg(&p2)
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg("true && echo merged")
        .output()
        .expect("spawn sandbox");

    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("merged"));
}

#[test]
fn sandbox_ro_dir_is_accessible() {
    require_sandbox();
    if !require_user_ns() {
        return;
    }

    let profile = write_temp_profile(
        &[("sh", "/usr/bin/sh")],
        SYSTEM_LIBS,
        &[],
    );
    let output = run_sandbox(&profile, &["sh", "-c", "ls /lib64/ld-linux-x86-64.so.2"]);
    assert_success(&output);
}

#[test]
fn sandbox_exit_code_is_propagated() {
    require_sandbox();
    if !require_user_ns() {
        return;
    }

    let profile = write_temp_profile(&[("sh", "/usr/bin/sh")], SYSTEM_LIBS, &[]);
    let output = run_sandbox(&profile, &["sh", "-c", "exit 42"]);
    assert_eq!(output.status.code(), Some(42));
}

#[test]
fn sandbox_binary_symlink_resolves() {
    require_sandbox();
    if !require_user_ns() {
        return;
    }

    let output = run_sandbox_named("make", &["/bin/sh", "-c", "echo symlink_ok"]);
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("symlink_ok"));
}
