use std::process::{Command, Output};

pub fn has_bwrap() -> bool {
    Command::new("bwrap").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

pub fn require_bwrap() -> bool {
    if !has_bwrap() {
        eprintln!("SKIP: bwrap not available");
        false
    } else {
        true
    }
}

pub fn run_bwrap(args: &[&str]) -> Output {
    Command::new("bwrap").args(args).output().expect("spawn bwrap")
}

pub fn assert_success(output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "expected success, stderr: {stderr}");
}

pub fn assert_failure(output: &Output) {
    assert!(!output.status.success(), "expected failure");
}
