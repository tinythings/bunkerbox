use super::*;

fn ids() -> (RequestId, WorkspaceSessionId) {
    (RequestId([1; 16]), WorkspaceSessionId([2; 16]))
}

fn build(argv: Vec<String>, env: Vec<(String, String)>) -> Result<RemoteBuild, String> {
    RemoteBuild::new(WorkspaceRelativePath::new("src").unwrap(), RemoteTool::new("make").unwrap(), argv, env)
}

fn raw_build_frame(argv_count: u16, arg: Option<&str>, env_count: u16, env: Option<(&str, &str)>) -> Frame {
    let mut writer = WireWriter::new(*b"BBR1");
    writer.u16(REMOTE_PROTOCOL_VERSION);
    writer.u8(2);
    writer.u8(0);
    writer.bytes(&[1; 16]);
    writer.bytes(&[2; 16]);
    writer.u16(0);
    writer.u16(4);
    writer.bytes(b"make");
    writer.u16(argv_count);
    if let Some(arg) = arg {
        writer.u16(arg.len() as u16);
        writer.bytes(arg.as_bytes());
    }
    writer.u16(env_count);
    if let Some((key, value)) = env {
        writer.u16(key.len() as u16);
        writer.bytes(key.as_bytes());
        writer.u16(value.len() as u16);
        writer.bytes(value.as_bytes());
    }
    writer.into_frame(FrameType::RemoteRequest).unwrap()
}

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

#[test]
fn remote_sync_round_trips() {
    let (request_id, session_id) = ids();
    let request = RemoteRequest::sync(request_id, session_id);
    let decoded = RemoteRequest::from_frame(request.to_frame().unwrap()).unwrap();
    assert_eq!(decoded, request);
}

#[test]
fn remote_build_round_trips_structured_arguments_and_environment() {
    let (request_id, session_id) = ids();
    let request = RemoteRequest::build(
        request_id,
        session_id,
        build(vec!["release mode".into(), "$(not-a-shell-command)".into()], vec![("MODE".into(), "debug value".into())]).unwrap(),
    );
    let decoded = RemoteRequest::from_frame(request.to_frame().unwrap()).unwrap();
    assert_eq!(decoded, request);
}

#[test]
fn every_remote_event_round_trips() {
    let request_id = ids().0;
    let events = vec![
        RemoteEventKind::SyncProgress { completed_bytes: 4, total_bytes: Some(9) },
        RemoteEventKind::Stdout(b"out".to_vec()),
        RemoteEventKind::Stderr(b"err".to_vec()),
        RemoteEventKind::Error { code: RemoteErrorCode::Failed, message: "failed".into() },
        RemoteEventKind::Cancelled,
        RemoteEventKind::Completed { exit_code: 17 },
    ];

    for kind in events {
        let event = RemoteEvent { request_id, kind };
        assert_eq!(RemoteEvent::from_frame(event.to_frame().unwrap()).unwrap(), event);
    }
}

#[test]
fn supported_remote_version_is_encoded() {
    let frame = RemoteRequest::sync(ids().0, ids().1).to_frame().unwrap();
    assert_eq!(u16::from_le_bytes([frame.payload[4], frame.payload[5]]), REMOTE_PROTOCOL_VERSION);
}

#[test]
fn unknown_remote_version_is_rejected() {
    let mut frame = RemoteRequest::sync(ids().0, ids().1).to_frame().unwrap();
    frame.payload[4..6].copy_from_slice(&(REMOTE_PROTOCOL_VERSION + 1).to_le_bytes());
    assert!(RemoteRequest::from_frame(frame).unwrap_err().contains("unsupported remote protocol version"));
}

#[test]
fn unknown_remote_operation_is_rejected() {
    let mut frame = RemoteRequest::sync(ids().0, ids().1).to_frame().unwrap();
    frame.payload[6] = 99;
    assert!(RemoteRequest::from_frame(frame).unwrap_err().contains("unknown remote operation"));
}

#[test]
fn unknown_remote_event_kind_is_rejected() {
    let event = RemoteEvent { request_id: ids().0, kind: RemoteEventKind::Cancelled };
    let mut frame = event.to_frame().unwrap();
    frame.payload[6] = 99;
    assert!(RemoteEvent::from_frame(frame).unwrap_err().contains("unknown remote event kind"));
}

#[test]
fn truncated_remote_payload_is_rejected() {
    assert!(RemoteRequest::from_frame(Frame::new(FrameType::RemoteRequest, b"BBR1".to_vec())).is_err());
}

#[test]
fn oversized_remote_string_is_rejected() {
    assert!(RemoteTool::new("x".repeat(MAX_REMOTE_TOOL_BYTES + 1)).is_err());
    let mut writer = WireWriter::new(*b"BBR1");
    writer.u16(REMOTE_PROTOCOL_VERSION);
    writer.u8(2);
    writer.u8(0);
    writer.bytes(&[1; 16]);
    writer.bytes(&[2; 16]);
    writer.u16((MAX_REMOTE_STRING_BYTES + 1) as u16);
    let frame = writer.into_frame(FrameType::RemoteRequest).unwrap();
    assert!(RemoteRequest::from_frame(frame).is_err());
}

#[test]
fn excessive_argv_count_is_rejected() {
    assert!(build(vec!["arg".into(); MAX_REMOTE_ARG_COUNT + 1], Vec::new()).is_err());
    assert!(RemoteRequest::from_frame(raw_build_frame((MAX_REMOTE_ARG_COUNT + 1) as u16, None, 0, None)).is_err());
}

#[test]
fn oversized_individual_argument_is_rejected() {
    assert!(build(vec!["x".repeat(MAX_REMOTE_ARG_BYTES + 1)], Vec::new()).is_err());
    assert!(RemoteRequest::from_frame(raw_build_frame(1, Some(&"x".repeat(MAX_REMOTE_ARG_BYTES + 1)), 0, None)).is_err());
}

#[test]
fn excessive_environment_count_is_rejected() {
    let env = (0..MAX_REMOTE_ENV_COUNT + 1).map(|i| (format!("KEY{i}"), "value".into())).collect();
    assert!(build(Vec::new(), env).is_err());
    assert!(RemoteRequest::from_frame(raw_build_frame(0, None, (MAX_REMOTE_ENV_COUNT + 1) as u16, None)).is_err());
}

#[test]
fn oversized_environment_key_and_value_are_rejected() {
    assert!(build(Vec::new(), vec![("K".repeat(MAX_REMOTE_ENV_KEY_BYTES + 1), "value".into())]).is_err());
    assert!(build(Vec::new(), vec![("KEY".into(), "V".repeat(MAX_REMOTE_ENV_VALUE_BYTES + 1))]).is_err());
    assert!(RemoteRequest::from_frame(raw_build_frame(0, None, 1, Some((&"K".repeat(MAX_REMOTE_ENV_KEY_BYTES + 1), "value")))).is_err());
    assert!(RemoteRequest::from_frame(raw_build_frame(0, None, 1, Some(("KEY", &"V".repeat(MAX_REMOTE_ENV_VALUE_BYTES + 1))))).is_err());
}

#[test]
fn invalid_remote_cwd_is_rejected() {
    assert!(WorkspaceRelativePath::new("/absolute").is_err());
    assert!(WorkspaceRelativePath::new("foo/../bar").is_err());
    assert!(WorkspaceRelativePath::new("foo/./bar").is_err());
    assert!(WorkspaceRelativePath::new("foo//bar").is_err());
}

#[test]
fn existing_exec_request_wire_format_is_unchanged() {
    let request =
        ExecRequest { cwd: "/workspace".into(), command: "make".into(), args: vec!["release".into()], env: vec![("MODE".into(), "debug".into())] };
    let encoded = request.serialize();
    assert_eq!(encoded, b"/workspace\0make\0release\0\0MODE=debug\0\0");
    let decoded = ExecRequest::deserialize(&encoded).unwrap();
    assert_eq!(decoded.cwd, request.cwd);
    assert_eq!(decoded.command, request.command);
    assert_eq!(decoded.args, request.args);
    assert_eq!(decoded.env, request.env);
}
