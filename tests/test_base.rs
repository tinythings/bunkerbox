mod common;
use common::*;

#[test]
fn bwrap_help_works() {
    if !require_bwrap() {
        return;
    }
    let output = run_bwrap(&["--help"]);
    assert_success(&output);
}

#[test]
fn bwrap_version_works() {
    if !require_bwrap() {
        return;
    }
    let output = run_bwrap(&["--version"]);
    assert_success(&output);
}

#[test]
fn bwrap_missing_command_fails() {
    if !require_bwrap() {
        return;
    }
    let output = run_bwrap(&[]);
    assert_failure(&output);
}
