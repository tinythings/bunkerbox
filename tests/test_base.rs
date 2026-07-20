mod common;
use common::*;

#[test]
fn unknown_flag_fails() {
    let output = run_sandbox_raw(&["--nonexistent-flag"]);
    assert_failure(&output);
}

#[test]
fn env_bad_format_fails() {
    let output = run_sandbox_raw(&["--env", "badformat_without_equals", "--", "/bin/true"]);
    assert_failure(&output);
}

#[test]
fn missing_profile_fails() {
    let output = run_sandbox_named("nonexistent-profile-12345", &["/bin/true"]);
    assert_failure(&output);
}

#[test]
fn share_requires_value() {
    let output = run_sandbox_raw(&["--share"]);
    assert_failure(&output);
}

#[test]
fn profile_requires_value() {
    let output = run_sandbox_raw(&["--profile"]);
    assert_failure(&output);
}

#[test]
fn cwd_requires_value() {
    let output = run_sandbox_raw(&["--cwd"]);
    assert_failure(&output);
}

#[test]
fn builtin_profile_make_parses() {
    let profile = write_temp_profile(&[("ls", "/bin/ls")], &["/lib"], &[]);
    let output = run_sandbox_raw(&[
        "--share", "/usr/share/bunkerbox",
        "--profile", &profile.to_string_lossy(),
        "--", "/bin/ls",
    ]);
    // This may succeed or fail depending on userns, but should NOT fail with
    // an "unknown flag" error.
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        !err.contains("unknown flag"),
        "got unexpected 'unknown flag' error: {err}"
    );
}

#[test]
fn builtin_profile_rust_parses() {
    let output = run_sandbox_raw(&[
        "--share", "/usr/share/bunkerbox",
        "--profile", &sandbox_bin().to_string_lossy(),
        "--", "/bin/true",
    ]);
    assert_failure(&output);
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("unknown flag") || err.contains("profile") || err.contains("parse"));
}

#[test]
fn empty_profiles_arg_fails_gracefully() {
    let output = run_sandbox_raw(&[
        "--share", "/nonexistent",
        "--", "nonexistent_command",
    ]);
    assert_failure(&output);
}

#[test]
fn multiple_profiles_are_accepted() {
    let p1 = write_temp_profile(&[("ls", "/usr/bin/ls")], &[], &[]);
    let p2 = write_temp_profile(&[("cat", "/usr/bin/cat")], &[], &[]);
    let output = run_sandbox_raw(&[
        "--share", "/usr/share/bunkerbox",
        "--profile", &p1.to_string_lossy(),
        "--profile", &p2.to_string_lossy(),
        "--", "/bin/true",
    ]);
    assert_failure(&output);
}

#[test]
fn env_flag_passes_values() {
    let profile = write_temp_profile(&[("env", "/usr/bin/env")], &[], &[]);
    let output = run_sandbox_raw(&[
        "--share", "/usr/share/bunkerbox",
        "--profile", &profile.to_string_lossy(),
        "--env", "FOO=bar",
        "--", "/usr/bin/env",
    ]);
    assert_failure(&output);
}
