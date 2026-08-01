use super::{validate_exec_request, ExecRequest, FrameType, TOOLCHAIN_PORT, TUI_STATUS_PORT};

#[test]
fn execution_and_tui_channels_are_distinct() {
    assert_ne!(TOOLCHAIN_PORT, TUI_STATUS_PORT);
    assert_ne!(FrameType::ExecReq as u16, FrameType::UiCommand as u16);
}

#[test]
fn reject_nul_in_request_argument() {
    let request = ExecRequest { cwd: "/workspace".into(), command: "cargo".into(), args: vec!["build\0".into()], env: Vec::new() };

    let err = validate_exec_request(&request).unwrap_err();
    assert_eq!(err, "request argument 0 contains a NUL byte");
}

#[test]
fn reject_invalid_request_environment_key() {
    let request = ExecRequest { cwd: "/workspace".into(), command: "cargo".into(), args: Vec::new(), env: vec![("BAD=KEY".into(), "value".into())] };

    let err = validate_exec_request(&request).unwrap_err();
    assert_eq!(err, "request environment key 0 contains '='");
}

#[test]
fn accept_valid_cargo_request() {
    let request = ExecRequest {
        cwd: "/workspace".into(),
        command: "cargo".into(),
        args: vec!["build".into(), "--target-dir".into(), "target/debug".into()],
        env: vec![("CARGO_TERM_COLOR".into(), "always".into())],
    };

    assert!(validate_exec_request(&request).is_ok());
}
