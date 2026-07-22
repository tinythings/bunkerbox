mod common;
use common::*;

fn wrap(extra: &[&str], cmd: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
    args.push("--proc".into());
    args.push("/proc".into());
    args.push("--dev".into());
    args.push("/dev".into());
    args.push("--tmpfs".into());
    args.push("/tmp".into());
    args.push("--".into());
    for c in cmd {
        args.push(c.to_string());
    }
    args
}

fn call(extra: &[&str], cmd: &[&str]) -> std::process::Output {
    let args = wrap(extra, cmd);
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_bwrap(&refs)
}

#[test]
fn bwrap_runs_true() {
    if !require_bwrap() {
        return;
    }
    let output = call(
        &[
            "--ro-bind",
            "/usr/bin/true",
            "/usr/bin/true",
            "--ro-bind",
            "/lib",
            "/lib",
            "--ro-bind",
            "/lib64",
            "/lib64",
            "--ro-bind",
            "/usr/lib",
            "/usr/lib",
        ],
        &["/usr/bin/true"],
    );
    assert_success(&output);
}

#[test]
fn bwrap_exit_code_propagated() {
    if !require_bwrap() {
        return;
    }
    let output = call(
        &[
            "--ro-bind",
            "/usr/bin/sh",
            "/usr/bin/sh",
            "--ro-bind",
            "/lib",
            "/lib",
            "--ro-bind",
            "/lib64",
            "/lib64",
            "--ro-bind",
            "/usr/lib",
            "/usr/lib",
        ],
        &["/usr/bin/sh", "-c", "exit 42"],
    );
    assert_eq!(output.status.code(), Some(42));
}

#[test]
fn bwrap_blocks_unlisted_binary() {
    if !require_bwrap() {
        return;
    }
    let output = call(&[], &["/usr/bin/env"]);
    assert_failure(&output);
}

#[test]
fn bwrap_network_blocked() {
    if !require_bwrap() {
        return;
    }
    let output = call(
        &[
            "--ro-bind",
            "/usr/bin/ping",
            "/usr/bin/ping",
            "--ro-bind",
            "/lib",
            "/lib",
            "--ro-bind",
            "/lib64",
            "/lib64",
            "--ro-bind",
            "/usr/lib",
            "/usr/lib",
            "--unshare-net",
        ],
        &["/usr/bin/ping", "-c", "1", "1.1.1.1"],
    );
    assert_failure(&output);
}

#[test]
fn bwrap_ro_bind_works() {
    if !require_bwrap() {
        return;
    }
    let output = call(
        &[
            "--ro-bind",
            "/usr/bin/sh",
            "/usr/bin/sh",
            "--ro-bind",
            "/usr/bin/echo",
            "/usr/bin/echo",
            "--ro-bind",
            "/lib",
            "/lib",
            "--ro-bind",
            "/lib64",
            "/lib64",
            "--ro-bind",
            "/usr/lib",
            "/usr/lib",
        ],
        &["/usr/bin/sh", "-c", "echo ok"],
    );
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("ok"));
}

#[test]
fn bwrap_rw_bind_works() {
    if !require_bwrap() {
        return;
    }
    let output = call(
        &[
            "--ro-bind",
            "/usr/bin/sh",
            "/usr/bin/sh",
            "--ro-bind",
            "/usr/bin/touch",
            "/usr/bin/touch",
            "--ro-bind",
            "/lib",
            "/lib",
            "--ro-bind",
            "/lib64",
            "/lib64",
            "--ro-bind",
            "/usr/lib",
            "/usr/lib",
            "--bind",
            "/tmp",
            "/tmp",
        ],
        &["/usr/bin/sh", "-c", "touch /tmp/test_bwrap_rw && echo done"],
    );
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("done"));
}

#[test]
fn bwrap_command_with_args() {
    if !require_bwrap() {
        return;
    }
    let output = call(
        &[
            "--ro-bind",
            "/usr/bin/sh",
            "/usr/bin/sh",
            "--ro-bind",
            "/lib",
            "/lib",
            "--ro-bind",
            "/lib64",
            "/lib64",
            "--ro-bind",
            "/usr/lib",
            "/usr/lib",
        ],
        &["/usr/bin/sh", "-c", "echo hello world"],
    );
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("hello world"));
}

#[test]
fn bwrap_not_found_binary_fails() {
    if !require_bwrap() {
        return;
    }
    let output = call(&[], &["/usr/bin/doesnotexist"]);
    assert_failure(&output);
}

#[test]
fn bwrap_clearenv_works() {
    if !require_bwrap() {
        return;
    }
    let output = call(
        &[
            "--ro-bind",
            "/usr/bin/env",
            "/usr/bin/env",
            "--ro-bind",
            "/lib",
            "/lib",
            "--ro-bind",
            "/lib64",
            "/lib64",
            "--ro-bind",
            "/usr/lib",
            "/usr/lib",
            "--clearenv",
            "--setenv",
            "FOO",
            "bar",
        ],
        &["/usr/bin/env"],
    );
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("FOO=bar"));
    assert!(!stdout.contains("USER="));
}

#[test]
fn bwrap_only_sees_mounted_paths() {
    if !require_bwrap() {
        return;
    }
    let output = call(
        &[
            "--ro-bind",
            "/usr/bin/ls",
            "/usr/bin/ls",
            "--ro-bind",
            "/lib",
            "/lib",
            "--ro-bind",
            "/lib64",
            "/lib64",
            "--ro-bind",
            "/usr/lib",
            "/usr/lib",
        ],
        &["/usr/bin/ls", "/"],
    );
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("dev"));
    assert!(stdout.contains("proc"));
    assert!(!stdout.contains("boot"));
}

#[test]
fn bwrap_missing_args_fails() {
    if !require_bwrap() {
        return;
    }
    let output = run_bwrap(&["--nonexistent"]);
    assert_failure(&output);
}
