use super::{dispatch_remote_frame, is_allowed, RemoteBroker, RemoteDispatchError};
use super::{monitor_bwrap_status, ChildEvent};
use crate::cfg::EnvMode;
use crate::remote::{
    AuthorizedRemoteRequest, RemoteAuthorizationPolicy, RemoteBackend, RemoteBackendError, RemoteBackendEvent, RemoteExecutionContext, RemoteFuture,
    RemoteRequest, RemoteSnapshotAuthority, RemoteSnapshotId, RemoteTargetId, RequestId, WorkspaceRelativePath, WorkspaceSessionId,
};
use crate::vscomm::{
    Frame, RemoteBuild as WireRemoteBuild, RemoteRequest as WireRemoteRequest, RemoteTool as WireRemoteTool, RequestId as WireRequestId,
    WorkspaceRelativePath as WireWorkspaceRelativePath, WorkspaceSessionId as WireWorkspaceSessionId,
};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;

#[test]
fn bwrap_status_reports_command_start() {
    let mut status = tempfile::NamedTempFile::new().unwrap();
    writeln!(status, "{{\"child-pid\":1234}}").unwrap();
    writeln!(status, "{{\"exit-code\":0}}").unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    monitor_bwrap_status(status.reopen().unwrap(), tx);

    assert!(matches!(rx.try_recv().unwrap(), ChildEvent::LauncherStarted));
    assert!(rx.try_recv().is_err());
}

#[test]
fn bwrap_status_reports_setup_failure_without_child() {
    let mut status = tempfile::NamedTempFile::new().unwrap();
    writeln!(status, "{{\"exit-code\":1}}").unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    monitor_bwrap_status(status.reopen().unwrap(), tx);

    assert!(matches!(rx.try_recv().unwrap(), ChildEvent::LauncherFailed(_)));
}

struct RecordingBackend {
    calls: Mutex<Vec<AuthorizedRemoteRequest>>,
    emit: Vec<RemoteBackendEvent>,
    result: Option<RemoteBackendError>,
}

struct TestSnapshotAuthority;

impl RemoteSnapshotAuthority for TestSnapshotAuthority {
    fn snapshot_available(&self, _session: WorkspaceSessionId, _snapshot_id: RemoteSnapshotId) -> bool {
        true
    }
}

struct RejectSnapshotAuthority;

impl RemoteSnapshotAuthority for RejectSnapshotAuthority {
    fn snapshot_available(&self, _session: WorkspaceSessionId, _snapshot_id: RemoteSnapshotId) -> bool {
        false
    }
}

impl RemoteBackend for RecordingBackend {
    fn execute<'a>(
        &'a self, request: AuthorizedRemoteRequest, events: mpsc::Sender<RemoteBackendEvent>,
    ) -> RemoteFuture<'a, Result<(), RemoteBackendError>> {
        self.calls.lock().unwrap().push(request);
        let emit = self.emit.clone();
        let result = self.result.clone();
        Box::pin(async move {
            for event in emit {
                events.send(event).await.map_err(|_| RemoteBackendError::Cancelled)?;
            }
            result.map_or(Ok(()), Err)
        })
    }
}

struct StreamingBackend {
    event_count: usize,
    release: Arc<tokio::sync::Notify>,
}

impl RemoteBackend for StreamingBackend {
    fn execute<'a>(
        &'a self, _request: AuthorizedRemoteRequest, events: mpsc::Sender<RemoteBackendEvent>,
    ) -> RemoteFuture<'a, Result<(), RemoteBackendError>> {
        let event_count = self.event_count;
        let release = self.release.clone();
        Box::pin(async move {
            for index in 0..event_count {
                events.send(RemoteBackendEvent::Stdout(vec![index as u8])).await.map_err(|_| RemoteBackendError::Cancelled)?;
            }
            release.notified().await;
            events.send(RemoteBackendEvent::Completed { exit_code: 0 }).await.map_err(|_| RemoteBackendError::Cancelled)
        })
    }
}

struct HangingBackend;

impl RemoteBackend for HangingBackend {
    fn execute<'a>(
        &'a self, _request: AuthorizedRemoteRequest, events: mpsc::Sender<RemoteBackendEvent>,
    ) -> RemoteFuture<'a, Result<(), RemoteBackendError>> {
        Box::pin(async move {
            events.send(RemoteBackendEvent::Stdout(b"first".to_vec())).await.map_err(|_| RemoteBackendError::Cancelled)?;
            std::future::pending::<Result<(), RemoteBackendError>>().await
        })
    }
}

struct FailingWriter;

impl AsyncWrite for FailingWriter {
    fn poll_write(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>, _buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "writer closed")))
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn remote_request(tool: &str) -> RemoteRequest {
    RemoteRequest::build(
        RequestId([1; 16]),
        WorkspaceSessionId([2; 16]),
        crate::remote::RemoteBuild::new(
            WorkspaceRelativePath::new("src").unwrap(),
            crate::remote::RemoteTool::new(tool).unwrap(),
            vec!["build".into(), "--release".into()],
            vec![("CC".into(), "cc".into())],
            RemoteSnapshotId::from_bytes([9; 16]),
        )
        .unwrap(),
    )
}

fn remote_broker(backend: Arc<dyn RemoteBackend>) -> RemoteBroker {
    let target = RemoteTargetId([3; 16]);
    let session = WorkspaceSessionId([2; 16]);
    RemoteBroker::new(
        RemoteAuthorizationPolicy::new(target, session, vec!["make".into()]).with_snapshot_authority(Arc::new(TestSnapshotAuthority)),
        RemoteExecutionContext { target, workspace_session_id: session },
        backend,
    )
}

#[tokio::test]
async fn authorized_remote_request_reaches_typed_backend() {
    let backend = Arc::new(RecordingBackend {
        calls: Mutex::new(Vec::new()),
        emit: vec![
            RemoteBackendEvent::Stdout(b"out".to_vec()),
            RemoteBackendEvent::Stderr(b"err".to_vec()),
            RemoteBackendEvent::Completed { exit_code: 7 },
        ],
        result: None,
    });
    let broker = remote_broker(backend.clone());
    let (tx, mut rx) = mpsc::channel(8);

    broker.dispatch(remote_request("make"), tx).await.unwrap();

    assert_eq!(backend.calls.lock().unwrap().len(), 1);
    let request = backend.calls.lock().unwrap()[0].request().clone();
    let crate::remote::RemoteOperation::Build(build) = request.operation() else { panic!("expected build") };
    assert_eq!(build.tool().as_str(), "make");
    assert_eq!(build.cwd().as_str(), "src");
    assert_eq!(build.argv(), ["build", "--release"]);
    assert_eq!(build.env(), [("CC".into(), "cc".into())]);
    assert_eq!(rx.recv().await, Some(RemoteBackendEvent::Stdout(b"out".to_vec())));
    assert_eq!(rx.recv().await, Some(RemoteBackendEvent::Stderr(b"err".to_vec())));
    assert_eq!(rx.recv().await, Some(RemoteBackendEvent::Completed { exit_code: 7 }));
}

#[tokio::test]
async fn rejected_remote_request_never_calls_backend() {
    let backend = Arc::new(RecordingBackend { calls: Mutex::new(Vec::new()), emit: Vec::new(), result: None });
    let broker = remote_broker(backend.clone());
    let (tx, mut rx) = mpsc::channel(8);

    let error = broker.dispatch(remote_request("cargo"), tx).await.unwrap_err();

    assert!(matches!(error, RemoteDispatchError::Unauthorized(_)));
    assert!(backend.calls.lock().unwrap().is_empty());
    assert!(matches!(rx.recv().await, Some(RemoteBackendEvent::Error { message }) if message.contains("authorization rejected")));
}

#[tokio::test]
async fn rejected_snapshot_capability_never_calls_backend() {
    let backend = Arc::new(RecordingBackend { calls: Mutex::new(Vec::new()), emit: Vec::new(), result: None });
    let target = RemoteTargetId([3; 16]);
    let session = WorkspaceSessionId([2; 16]);
    let policy = RemoteAuthorizationPolicy::new(target, session, vec!["make".into()]).with_snapshot_authority(Arc::new(RejectSnapshotAuthority));
    let broker = RemoteBroker::new(policy, RemoteExecutionContext { target, workspace_session_id: session }, backend.clone());
    let (tx, mut rx) = mpsc::channel(8);

    let error = broker.dispatch(remote_request("make"), tx).await.unwrap_err();
    assert_eq!(error, RemoteDispatchError::Unauthorized(crate::remote::RemoteAuthorizationError::SnapshotNotAllowed));
    assert!(backend.calls.lock().unwrap().is_empty());
    assert!(matches!(rx.recv().await, Some(RemoteBackendEvent::Error { message }) if message.contains("SnapshotNotAllowed")));
}

#[tokio::test]
async fn backend_failure_is_reported_as_typed_error() {
    let backend =
        Arc::new(RecordingBackend { calls: Mutex::new(Vec::new()), emit: Vec::new(), result: Some(RemoteBackendError::Spawn("not found".into())) });
    let broker = remote_broker(backend);
    let (tx, mut rx) = mpsc::channel(8);

    assert!(matches!(broker.dispatch(remote_request("make"), tx).await, Err(RemoteDispatchError::Backend(RemoteBackendError::Spawn(_)))));
    assert_eq!(rx.recv().await, Some(RemoteBackendEvent::Error { message: "not found".into() }));
}

#[tokio::test]
async fn fake_backend_can_report_timeout_and_cancellation_states() {
    for (failure, expected) in [
        (RemoteBackendError::Timeout, RemoteBackendEvent::Error { message: "remote backend timed out".into() }),
        (RemoteBackendError::Cancelled, RemoteBackendEvent::Cancelled),
    ] {
        let backend = Arc::new(RecordingBackend { calls: Mutex::new(Vec::new()), emit: Vec::new(), result: Some(failure.clone()) });
        let broker = remote_broker(backend);
        let (tx, mut rx) = mpsc::channel(8);

        assert!(matches!(broker.dispatch(remote_request("make"), tx).await, Err(RemoteDispatchError::Backend(error)) if error == failure));
        assert_eq!(rx.recv().await, Some(expected));
    }
}

#[tokio::test]
async fn framed_remote_sync_runs_full_dispatch_and_event_conversion_chain() {
    let backend = Arc::new(RecordingBackend {
        calls: Mutex::new(Vec::new()),
        emit: vec![RemoteBackendEvent::SyncCompleted { snapshot_id: RemoteSnapshotId::from_bytes([9; 16]) }],
        result: None,
    });
    let broker = remote_broker(backend.clone());
    let request = WireRemoteRequest::sync(WireRequestId([6; 16]), WireWorkspaceSessionId([2; 16]));
    let (mut guest, mut host) = tokio::io::duplex(4096);

    dispatch_remote_frame(request.to_frame().unwrap(), &broker, &mut host).await.unwrap();

    let event = crate::vscomm::RemoteEvent::from_frame(Frame::read_async(&mut guest).await.unwrap()).unwrap();
    assert_eq!(event.request_id, crate::vscomm::RequestId([6; 16]));
    assert_eq!(event.kind, crate::vscomm::RemoteEventKind::SyncCompleted { snapshot_id: crate::vscomm::RemoteSnapshotId([9; 16]) });
    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(matches!(calls[0].request().operation(), crate::remote::RemoteOperation::Sync(_)));
}

#[tokio::test]
async fn framed_remote_build_preserves_typed_fields_and_output_order() {
    let backend = Arc::new(RecordingBackend {
        calls: Mutex::new(Vec::new()),
        emit: vec![
            RemoteBackendEvent::Stdout(b"out".to_vec()),
            RemoteBackendEvent::Stderr(b"err".to_vec()),
            RemoteBackendEvent::Completed { exit_code: 23 },
        ],
        result: None,
    });
    let broker = remote_broker(backend.clone());
    let request = WireRemoteRequest::build(
        WireRequestId([7; 16]),
        WireWorkspaceSessionId([2; 16]),
        WireRemoteBuild::new(
            WireWorkspaceRelativePath::new("src").unwrap(),
            WireRemoteTool::new("make").unwrap(),
            vec!["release mode".into(), "$(literal)".into()],
            vec![("CC".into(), "cc".into())],
            crate::vscomm::RemoteSnapshotId([9; 16]),
        )
        .unwrap(),
    );
    let (mut guest, mut host) = tokio::io::duplex(4096);

    dispatch_remote_frame(request.to_frame().unwrap(), &broker, &mut host).await.unwrap();

    let stdout = crate::vscomm::RemoteEvent::from_frame(Frame::read_async(&mut guest).await.unwrap()).unwrap();
    let stderr = crate::vscomm::RemoteEvent::from_frame(Frame::read_async(&mut guest).await.unwrap()).unwrap();
    let completed = crate::vscomm::RemoteEvent::from_frame(Frame::read_async(&mut guest).await.unwrap()).unwrap();
    assert_eq!(stdout.kind, crate::vscomm::RemoteEventKind::Stdout(b"out".to_vec()));
    assert_eq!(stderr.kind, crate::vscomm::RemoteEventKind::Stderr(b"err".to_vec()));
    assert_eq!(completed.kind, crate::vscomm::RemoteEventKind::Completed { exit_code: 23 });

    let calls = backend.calls.lock().unwrap();
    let crate::remote::RemoteOperation::Build(build) = calls[0].request().operation() else { panic!("expected build") };
    assert_eq!(calls[0].request_id(), crate::remote::RequestId([7; 16]));
    assert_eq!(build.cwd().as_str(), "src");
    assert_eq!(build.tool().as_str(), "make");
    assert_eq!(build.argv(), ["release mode", "$(literal)"]);
    assert_eq!(build.env(), [("CC".into(), "cc".into())]);
}

#[tokio::test]
async fn malformed_remote_frame_fails_before_backend_dispatch() {
    let backend = Arc::new(RecordingBackend { calls: Mutex::new(Vec::new()), emit: Vec::new(), result: None });
    let broker = remote_broker(backend.clone());
    let mut frame = WireRemoteRequest::sync(WireRequestId([6; 16]), WireWorkspaceSessionId([2; 16])).to_frame().unwrap();
    frame.payload[6] = 99;
    let (_, mut host) = tokio::io::duplex(128);

    assert!(dispatch_remote_frame(frame, &broker, &mut host).await.is_err());
    assert!(backend.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn remote_events_stream_past_bounded_channel_capacity_before_completion() {
    let release = Arc::new(tokio::sync::Notify::new());
    let backend = Arc::new(StreamingBackend { event_count: 65, release: release.clone() });
    let broker = remote_broker(backend);
    let request = WireRemoteRequest::sync(WireRequestId([6; 16]), WireWorkspaceSessionId([2; 16]));
    let (mut guest, mut host) = tokio::io::duplex(8192);
    let dispatch = tokio::spawn(async move { dispatch_remote_frame(request.to_frame().unwrap(), &broker, &mut host).await });

    let first = tokio::time::timeout(std::time::Duration::from_secs(1), Frame::read_async(&mut guest)).await.unwrap().unwrap();
    let first = crate::vscomm::RemoteEvent::from_frame(first).unwrap();
    assert_eq!(first.kind, crate::vscomm::RemoteEventKind::Stdout(vec![0]));
    release.notify_one();

    let result = tokio::time::timeout(std::time::Duration::from_secs(1), dispatch).await.unwrap().unwrap();
    result.unwrap();
    for index in 1..65 {
        let event = crate::vscomm::RemoteEvent::from_frame(
            tokio::time::timeout(std::time::Duration::from_secs(1), Frame::read_async(&mut guest)).await.unwrap().unwrap(),
        )
        .unwrap();
        assert_eq!(event.kind, crate::vscomm::RemoteEventKind::Stdout(vec![index as u8]));
    }
    let completed = crate::vscomm::RemoteEvent::from_frame(
        tokio::time::timeout(std::time::Duration::from_secs(1), Frame::read_async(&mut guest)).await.unwrap().unwrap(),
    )
    .unwrap();
    assert_eq!(completed.kind, crate::vscomm::RemoteEventKind::Completed { exit_code: 0 });
}

#[tokio::test]
async fn writer_failure_cancels_hanging_backend_without_waiting_forever() {
    let broker = remote_broker(Arc::new(HangingBackend));
    let request = WireRemoteRequest::sync(WireRequestId([6; 16]), WireWorkspaceSessionId([2; 16]));
    let result =
        tokio::time::timeout(std::time::Duration::from_secs(1), dispatch_remote_frame(request.to_frame().unwrap(), &broker, &mut FailingWriter))
            .await
            .unwrap();

    assert!(result.is_err());
}

#[test]
fn local_passthrough_authorization_remains_separate() {
    assert!(is_allowed(&["make *".into()], "make", &["--release".into()]));
    assert!(!is_allowed(&["make *".into()], "cargo", &["build".into()]));
}

#[test]
fn local_exec_request_still_builds_on_the_local_path() {
    let workspace = tempfile::tempdir().unwrap();
    let cwd = crate::workspace::WorkspaceCwd::resolve(workspace.path(), Path::new("/workspace")).unwrap();
    let session = super::VsockSession {
        passthrough: Arc::new(vec!["true *".into()]),
        env_mode: EnvMode::Paranoid,
        workspace: workspace.path().to_path_buf(),
        merged_profile: None,
        proxy_config: None,
        remote_broker: Arc::new(remote_broker(Arc::new(RecordingBackend { calls: Mutex::new(Vec::new()), emit: Vec::new(), result: None }))),
    };
    let request = crate::vscomm::ExecRequest { cwd: "/workspace".into(), command: "true".into(), args: Vec::new(), env: Vec::new() };

    assert!(super::build_command(&session, &request, &cwd).is_ok());
}
