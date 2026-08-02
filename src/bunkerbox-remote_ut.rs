use super::*;
use bunkerbox::vscomm::{Frame, RemoteEvent, RemoteEventKind};

struct MemoryStream {
    input: io::Cursor<Vec<u8>>,
    output: Vec<u8>,
}

impl MemoryStream {
    fn new(events: Vec<RemoteEvent>) -> Self {
        let mut input = Vec::new();
        for event in events {
            event.to_frame().unwrap().write(&mut input).unwrap();
        }
        Self { input: io::Cursor::new(input), output: Vec::new() }
    }
}

impl Read for MemoryStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.input.read(buf)
    }
}

impl Write for MemoryStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.output.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn parses_sync_command() {
    assert_eq!(parse_command(&["sync".into()]), Ok(RemoteCommand::Sync));
}

#[test]
fn parses_build_tool_and_args_without_joining() {
    assert_eq!(
        parse_command(&["build".into(), "make".into(), "release mode".into(), "$(literal)".into()]),
        Ok(RemoteCommand::Build { tool: "make".into(), args: vec!["release mode".into(), "$(literal)".into()] })
    );
}

#[test]
fn build_request_preserves_logical_cwd_and_arguments() {
    let request = build_request(
        RemoteCommand::Build { tool: "make".into(), args: vec!["release mode".into(), "$(literal)".into()] },
        "src".into(),
        RequestId([1; 16]),
        WorkspaceSessionId([2; 16]),
    )
    .unwrap();
    let frame = request.to_frame().unwrap();
    let decoded = RemoteRequest::from_frame(frame).unwrap();
    let bunkerbox::vscomm::RemoteOperation::Build(build) = decoded.operation else { panic!("expected build") };
    assert_eq!(build.cwd.as_str(), "src");
    assert_eq!(build.tool.as_str(), "make");
    assert_eq!(build.argv, ["release mode", "$(literal)"]);
}

#[test]
fn sync_success_uses_existing_remote_helper_and_returns_status() {
    let request_id = RequestId([3; 16]);
    let mut stream = MemoryStream::new(vec![RemoteEvent { request_id, kind: RemoteEventKind::Completed { exit_code: 0 } }]);
    let status = execute_remote_request_to(
        &mut stream,
        build_request(RemoteCommand::Sync, String::new(), request_id, WorkspaceSessionId([2; 16])).unwrap(),
        &mut Vec::new(),
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(status, 0);
    assert!(Frame::read(&mut io::Cursor::new(stream.output)).is_ok());
}

#[test]
fn build_success_preserves_output_bytes_and_nonzero_exit_code() {
    let request_id = RequestId([6; 16]);
    let mut stream = MemoryStream::new(vec![
        RemoteEvent { request_id, kind: RemoteEventKind::Stdout(vec![b'o', b'\n', 0xff]) },
        RemoteEvent { request_id, kind: RemoteEventKind::Stderr(b"err\n".to_vec()) },
        RemoteEvent { request_id, kind: RemoteEventKind::Completed { exit_code: 17 } },
    ]);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    let status = execute_remote_request_to(
        &mut stream,
        build_request(
            RemoteCommand::Build { tool: "make".into(), args: vec!["release".into()] },
            "src".into(),
            request_id,
            WorkspaceSessionId([2; 16]),
        )
        .unwrap(),
        &mut stdout,
        &mut stderr,
    )
    .unwrap();

    assert_eq!(status, 17);
    assert_eq!(stdout, vec![b'o', b'\n', 0xff]);
    assert_eq!(stderr, b"err\n");
}

#[test]
fn remote_failures_return_errors_without_local_fallback() {
    for kind in
        [RemoteEventKind::Error { code: bunkerbox::vscomm::RemoteErrorCode::Failed, message: "unavailable".into() }, RemoteEventKind::Cancelled]
    {
        let request_id = RequestId([4; 16]);
        let mut stream = MemoryStream::new(vec![RemoteEvent { request_id, kind }]);
        let result = execute_remote_request_to(
            &mut stream,
            build_request(RemoteCommand::Sync, String::new(), request_id, WorkspaceSessionId([2; 16])).unwrap(),
            &mut Vec::new(),
            &mut Vec::new(),
        );
        assert!(result.is_err());
    }
}

#[test]
fn rejected_tool_and_mismatched_response_fail_closed() {
    let request_id = RequestId([7; 16]);
    let rejected = RemoteEvent {
        request_id,
        kind: RemoteEventKind::Error { code: bunkerbox::vscomm::RemoteErrorCode::Failed, message: "authorization rejected".into() },
    };
    let mut stream = MemoryStream::new(vec![rejected]);
    let request = build_request(
        RemoteCommand::Build { tool: "cargo".into(), args: vec!["build".into()] },
        String::new(),
        request_id,
        WorkspaceSessionId([2; 16]),
    )
    .unwrap();
    assert_eq!(execute_remote_request_to(&mut stream, request, &mut Vec::new(), &mut Vec::new()).unwrap_err(), "authorization rejected");

    let mut stream = MemoryStream::new(vec![RemoteEvent { request_id: RequestId([8; 16]), kind: RemoteEventKind::Completed { exit_code: 0 } }]);
    let request = build_request(RemoteCommand::Sync, String::new(), request_id, WorkspaceSessionId([2; 16])).unwrap();
    assert_eq!(execute_remote_request_to(&mut stream, request, &mut Vec::new(), &mut Vec::new()).unwrap_err(), "remote event request ID mismatch");
}

#[test]
fn logical_cwd_is_workspace_relative_only() {
    assert_eq!(logical_workspace_cwd(Path::new("/workspace/project/src")).unwrap(), "project/src");
    assert!(logical_workspace_cwd(Path::new("/tmp/project")).is_err());
}
