use super::*;
use bunkerbox::vscomm::{Frame, RemoteEvent, RemoteEventKind, RemoteOperation};

fn snapshot_id() -> bunkerbox::remote::RemoteSnapshotId {
    bunkerbox::remote::RemoteSnapshotId::from_bytes([9; 16])
}

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
fn parses_cargo_toolchain_and_arguments_without_joining() {
    assert_eq!(
        parse_command(&["build".into(), "cargo".into(), "+nightly".into(), "build".into(), "literal $(value)".into()]),
        Ok(RemoteCommand::Build { tool: "cargo".into(), args: vec!["+nightly".into(), "build".into(), "literal $(value)".into()] })
    );
}

#[test]
fn build_request_preserves_logical_cwd_and_arguments() {
    let request = build_request(
        RemoteCommand::Build { tool: "make".into(), args: vec!["release mode".into(), "$(literal)".into()] },
        "src".into(),
        RequestId([1; 16]),
        WorkspaceSessionId([2; 16]),
        snapshot_id(),
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
fn cargo_build_request_preserves_structured_toolchain_arguments() {
    let request = build_request(
        RemoteCommand::Build { tool: "cargo".into(), args: vec!["+nightly".into(), "build".into(), "$(literal)".into()] },
        "crates/app".into(),
        RequestId([1; 16]),
        WorkspaceSessionId([2; 16]),
        snapshot_id(),
    )
    .unwrap();
    let RemoteOperation::Build(build) = request.operation else { panic!("expected build") };
    assert_eq!(build.tool.as_str(), "cargo");
    assert_eq!(build.cwd.as_str(), "crates/app");
    assert_eq!(build.argv, ["+nightly", "build", "$(literal)"]);
    assert!(build.env.is_empty());
}

#[test]
fn sync_success_uses_existing_remote_helper_and_returns_status() {
    let request_id = RequestId([3; 16]);
    let mut stream = MemoryStream::new(vec![RemoteEvent {
        request_id,
        kind: RemoteEventKind::SyncCompleted { snapshot_id: bunkerbox::vscomm::RemoteSnapshotId([9; 16]) },
    }]);
    let status = execute_remote_request_to(
        &mut stream,
        build_request(RemoteCommand::Sync, String::new(), request_id, WorkspaceSessionId([2; 16]), snapshot_id()).unwrap(),
        &mut Vec::new(),
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(status, RemoteCompletion::Synced(snapshot_id()));
    assert!(Frame::read(&mut io::Cursor::new(stream.output)).is_ok());
}

#[test]
fn standalone_sync_uses_diagnostic_non_retaining_request() {
    let session = WorkspaceSessionId([2; 16]);
    sync_snapshot_using(session, |request| {
        let bunkerbox::vscomm::RemoteOperation::Sync(sync) = request.operation else { panic!("expected sync") };
        assert!(!sync.retain_capability);
        Ok(RemoteCompletion::Completed(0))
    })
    .unwrap();
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
            snapshot_id(),
        )
        .unwrap(),
        &mut stdout,
        &mut stderr,
    )
    .unwrap();

    assert_eq!(status, RemoteCompletion::Completed(17));
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
            build_request(RemoteCommand::Sync, String::new(), request_id, WorkspaceSessionId([2; 16]), snapshot_id()).unwrap(),
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
        snapshot_id(),
    )
    .unwrap();
    assert_eq!(execute_remote_request_to(&mut stream, request, &mut Vec::new(), &mut Vec::new()).unwrap_err(), "authorization rejected");

    let mut stream = MemoryStream::new(vec![RemoteEvent { request_id: RequestId([8; 16]), kind: RemoteEventKind::Completed { exit_code: 0 } }]);
    let request = build_request(RemoteCommand::Sync, String::new(), request_id, WorkspaceSessionId([2; 16]), snapshot_id()).unwrap();
    assert_eq!(execute_remote_request_to(&mut stream, request, &mut Vec::new(), &mut Vec::new()).unwrap_err(), "remote event request ID mismatch");
}

#[test]
fn logical_cwd_is_workspace_relative_only() {
    assert_eq!(logical_workspace_cwd(Path::new("/workspace/project/src")).unwrap(), "project/src");
    assert!(logical_workspace_cwd(Path::new("/tmp/project")).is_err());
}

#[test]
fn configured_remote_make_installation_precedes_native_path_resolution() {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("bunkerbox-remote");
    std::fs::write(&executable, b"remote").unwrap();
    install_remote_make_link(root.path(), &executable, true).unwrap();
    assert_eq!(std::fs::read_link(root.path().join("make")).unwrap(), executable);
}

#[test]
fn disabled_remote_make_removes_only_its_managed_link() {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("bunkerbox-remote");
    std::fs::write(&executable, b"remote").unwrap();
    install_remote_make_link(root.path(), &executable, true).unwrap();
    install_remote_make_link(root.path(), &executable, false).unwrap();
    assert!(!root.path().join("make").exists());

    std::fs::write(root.path().join("make"), b"native").unwrap();
    install_remote_make_link(root.path(), &executable, false).unwrap();
    assert_eq!(std::fs::read(root.path().join("make")).unwrap(), b"native");
}

#[test]
fn configured_remote_make_does_not_overwrite_unmanaged_entry() {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("bunkerbox-remote");
    std::fs::write(&executable, b"remote").unwrap();
    std::fs::write(root.path().join("make"), b"native").unwrap();
    assert!(install_remote_make_link(root.path(), &executable, true).is_err());
    assert_eq!(std::fs::read(root.path().join("make")).unwrap(), b"native");
}

#[test]
fn configured_remote_cargo_does_not_overwrite_unmanaged_entry() {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("bunkerbox-remote");
    std::fs::write(&executable, b"remote").unwrap();
    std::fs::write(root.path().join("cargo"), b"native").unwrap();
    assert!(install_remote_cargo_link(root.path(), &executable, true).is_err());
    assert_eq!(std::fs::read(root.path().join("cargo")).unwrap(), b"native");
}

#[test]
fn transparent_build_syncs_first_and_reuses_that_capability() {
    let session = WorkspaceSessionId([2; 16]);
    let capability = snapshot_id();
    let mut requests = Vec::new();
    let result = run_build_with_sync_using(
        "src".into(),
        "make".into(),
        vec!["release".into(), "space arg".into()],
        vec![("CC".into(), "cc".into())],
        session,
        |request| {
            requests.push(request.clone());
            if requests.len() == 1 {
                Ok(RemoteCompletion::Synced(capability))
            } else {
                Ok(RemoteCompletion::Completed(17))
            }
        },
    )
    .unwrap();

    assert_eq!(result, 17);
    assert_eq!(requests.len(), 2);
    assert!(matches!(&requests[0].operation, bunkerbox::vscomm::RemoteOperation::Sync(_)));
    let bunkerbox::vscomm::RemoteOperation::Build(ref build) = requests[1].operation else { panic!("expected build") };
    assert_eq!(build.tool.as_str(), "make");
    assert_eq!(build.cwd.as_str(), "src");
    assert_eq!(build.argv, ["release", "space arg"]);
    assert_eq!(build.snapshot_id, bunkerbox::vscomm::RemoteSnapshotId([9; 16]));
    assert_eq!(build.env, [("CC".into(), "cc".into())]);
}

#[test]
fn transparent_build_does_not_build_after_sync_failure() {
    let mut calls = 0;
    let result = run_build_with_sync_using("src".into(), "make".into(), vec!["release".into()], Vec::new(), WorkspaceSessionId([2; 16]), |_| {
        calls += 1;
        Err("sync failed".to_string())
    });
    assert_eq!(result, Err("sync failed".to_string()));
    assert_eq!(calls, 1);
}
