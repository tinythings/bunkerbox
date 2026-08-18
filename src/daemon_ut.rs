use super::{
    dispatch_exec_request_for_session, dispatch_remote_frame, dispatch_remote_frame_for_session, is_allowed, RemoteBroker, RemoteDispatchError,
    RemoteRouter,
};
use super::{monitor_bwrap_status, ChildEvent};
use crate::cfg::{EnvMode, ProjectConfig};
use crate::remote::{
    AuthorizedRemoteRequest, RemoteAdmissionLimits, RemoteAuthorizationPolicy, RemoteBackend, RemoteBackendError, RemoteBackendEvent,
    RemoteEnvironmentPolicy, RemoteExecutionContext, RemoteExecutionControl, RemoteFuture, RemoteRequest, RemoteSnapshotAuthority, RemoteSnapshotId,
    RemoteTargetId, RequestId, WorkspaceRelativePath, WorkspaceSessionId,
};
use crate::remote_target::{ActiveBuildTarget, BuildTargetCatalog};
use crate::vscomm::{
    Frame, FrameType, RemoteBuild as WireRemoteBuild, RemoteRequest as WireRemoteRequest, RemoteTool as WireRemoteTool, RequestId as WireRequestId,
    WorkspaceRelativePath as WireWorkspaceRelativePath, WorkspaceSessionId as WireWorkspaceSessionId,
};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;
use tokio::sync::Notify;

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

struct TargetRecordingBackend {
    calls: Mutex<Vec<AuthorizedRemoteRequest>>,
}

impl RemoteBackend for TargetRecordingBackend {
    fn execute<'a>(
        &'a self, request: AuthorizedRemoteRequest, _control: RemoteExecutionControl, events: mpsc::Sender<RemoteBackendEvent>,
    ) -> RemoteFuture<'a, Result<(), RemoteBackendError>> {
        let sync = matches!(request.request().operation(), crate::remote::RemoteOperation::Sync(_));
        self.calls.lock().unwrap().push(request);
        Box::pin(async move {
            let event = if sync {
                RemoteBackendEvent::SyncCompleted { snapshot_id: RemoteSnapshotId::from_bytes([9; 16]) }
            } else {
                RemoteBackendEvent::Completed { exit_code: 0 }
            };
            events.send(event).await.map_err(|_| RemoteBackendError::Cancelled)
        })
    }
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
        &'a self, request: AuthorizedRemoteRequest, _control: RemoteExecutionControl, events: mpsc::Sender<RemoteBackendEvent>,
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
        &'a self, _request: AuthorizedRemoteRequest, _control: RemoteExecutionControl, events: mpsc::Sender<RemoteBackendEvent>,
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
        &'a self, _request: AuthorizedRemoteRequest, _control: RemoteExecutionControl, events: mpsc::Sender<RemoteBackendEvent>,
    ) -> RemoteFuture<'a, Result<(), RemoteBackendError>> {
        Box::pin(async move {
            events.send(RemoteBackendEvent::Stdout(b"first".to_vec())).await.map_err(|_| RemoteBackendError::Cancelled)?;
            std::future::pending::<Result<(), RemoteBackendError>>().await
        })
    }
}

struct HoldingBackend {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl RemoteBackend for HoldingBackend {
    fn execute<'a>(
        &'a self, _request: AuthorizedRemoteRequest, control: RemoteExecutionControl, events: mpsc::Sender<RemoteBackendEvent>,
    ) -> RemoteFuture<'a, Result<(), RemoteBackendError>> {
        let started = self.started.clone();
        let release = self.release.clone();
        Box::pin(async move {
            events.send(RemoteBackendEvent::Stdout(b"active".to_vec())).await.map_err(|_| RemoteBackendError::Cancelled)?;
            started.notify_one();
            tokio::select! {
                _ = release.notified() => events.send(RemoteBackendEvent::Completed { exit_code: 0 }).await.map_err(|_| RemoteBackendError::Cancelled),
                _ = control.cancelled() => Err(RemoteBackendError::Cancelled),
            }
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
    remote_request_with_id(tool, RequestId([1; 16]))
}

fn remote_request_with_id(tool: &str, request_id: RequestId) -> RemoteRequest {
    RemoteRequest::build(
        request_id,
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

fn target_catalog_fixture() -> (tempfile::TempDir, BuildTargetCatalog) {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(".bunkerbox")).unwrap();
    fs::write(
        root.path().join(".bunkerbox").join(crate::remote_target::REMOTE_PROJECT_CONFIG_FILE_NAME),
        "targets:\n  bsdbox:\n    ssh: builder@bsdbox\n    workspace: /var/tmp/bunkerbox\n    project:\n      remote:\n        tools:\n          - name: cargo\n            allow-args: true\n",
    )
    .unwrap();
    let catalog = BuildTargetCatalog::load_optional(root.path(), &ProjectConfig::default()).unwrap().unwrap();
    (root, catalog)
}

fn target_session(
    workspace: std::path::PathBuf, active: ActiveBuildTarget, backend: Arc<TargetRecordingBackend>, target: RemoteTargetId,
    session_id: WorkspaceSessionId,
) -> super::VsockSession {
    let policy = RemoteAuthorizationPolicy::from_policies(
        target,
        session_id,
        vec![("cargo".to_string(), crate::remote::RemoteToolPolicy::new(true).with_command("cargo"))],
        RemoteEnvironmentPolicy::default(),
    )
    .unwrap()
    .with_snapshot_authority(Arc::new(TestSnapshotAuthority));
    let context = RemoteExecutionContext { target, workspace_session_id: session_id };
    let broker = Arc::new(RemoteBroker::new(policy, context, backend));
    super::VsockSession {
        passthrough: Arc::new(vec!["cargo *".to_string()]),
        env_mode: EnvMode::Relaxed,
        workspace,
        merged_profile: None,
        proxy_config: None,
        remote_router: Arc::new(RemoteRouter::new(active, BTreeMap::from([("bsdbox".to_string(), broker)]))),
        local_session_id: session_id,
        local_capabilities: Mutex::new(BTreeMap::new()),
    }
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

#[tokio::test]
async fn admission_rejects_active_excess_without_calling_backend() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let broker = Arc::new(
        remote_broker(Arc::new(HoldingBackend { started: started.clone(), release: release.clone() }))
            .with_admission_limits(RemoteAdmissionLimits::new(1, 1).unwrap()),
    );
    let (first_tx, mut first_rx) = mpsc::channel(8);
    let first = tokio::spawn({
        let broker = broker.clone();
        async move { broker.dispatch(remote_request_with_id("make", RequestId([1; 16])), first_tx).await }
    });
    started.notified().await;

    let (second_tx, mut second_rx) = mpsc::channel(8);
    let error = broker.dispatch(remote_request_with_id("make", RequestId([2; 16])), second_tx).await.unwrap_err();
    assert!(matches!(error, RemoteDispatchError::Backend(RemoteBackendError::Transport { class: crate::remote::RemoteFailureClass::Busy, .. })));
    assert!(matches!(second_rx.recv().await, Some(RemoteBackendEvent::Error { message }) if message.contains("busy")));

    release.notify_one();
    assert!(first.await.unwrap().is_ok());
    assert_eq!(first_rx.recv().await, Some(RemoteBackendEvent::Stdout(b"active".to_vec())));
    assert_eq!(first_rx.recv().await, Some(RemoteBackendEvent::Completed { exit_code: 0 }));
}

#[tokio::test]
async fn cancel_acknowledges_separately_and_emits_one_target_terminal() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let broker = Arc::new(
        remote_broker(Arc::new(HoldingBackend { started: started.clone(), release }))
            .with_admission_limits(RemoteAdmissionLimits::new(2, 1).unwrap()),
    );
    let (target_tx, mut target_rx) = mpsc::channel(8);
    let target = tokio::spawn({
        let broker = broker.clone();
        async move { broker.dispatch(remote_request_with_id("make", RequestId([1; 16])), target_tx).await }
    });
    started.notified().await;
    assert_eq!(target_rx.recv().await, Some(RemoteBackendEvent::Stdout(b"active".to_vec())));

    let (cancel_tx, mut cancel_rx) = mpsc::channel(8);
    broker.dispatch(RemoteRequest::cancel(RequestId([8; 16]), WorkspaceSessionId([2; 16]), RequestId([1; 16])), cancel_tx).await.unwrap();
    assert_eq!(cancel_rx.recv().await, Some(RemoteBackendEvent::Completed { exit_code: 0 }));
    assert_eq!(target_rx.recv().await, Some(RemoteBackendEvent::Cancelled));
    assert!(target_rx.try_recv().is_err());
    assert!(matches!(target.await.unwrap(), Err(RemoteDispatchError::Backend(RemoteBackendError::Cancelled))));
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
        remote_router: Arc::new(RemoteRouter::new(
            ActiveBuildTarget::new(),
            BTreeMap::from([(
                "localhost".to_string(),
                Arc::new(remote_broker(Arc::new(RecordingBackend { calls: Mutex::new(Vec::new()), emit: Vec::new(), result: None }))),
            )]),
        )),
        local_session_id: WorkspaceSessionId([2; 16]),
        local_capabilities: Mutex::new(BTreeMap::new()),
    };
    let request = crate::vscomm::ExecRequest { cwd: "/workspace".into(), command: "true".into(), args: Vec::new(), env: Vec::new() };

    assert!(super::build_command(&session, &request, &cwd).is_ok());
}

#[tokio::test]
async fn transparent_exec_request_routes_fresh_managed_cargo_to_selected_remote_target() {
    let (root, catalog) = target_catalog_fixture();
    let active = ActiveBuildTarget::new();
    active.select(&catalog, "bsdbox").unwrap();
    let backend = Arc::new(TargetRecordingBackend { calls: Mutex::new(Vec::new()) });
    let session_id = WorkspaceSessionId([2; 16]);
    let target = RemoteTargetId([3; 16]);
    let session = target_session(root.path().to_path_buf(), active, backend.clone(), target, session_id);
    let request =
        crate::vscomm::ExecRequest { cwd: "/workspace".to_string(), command: "cargo".to_string(), args: vec!["build".to_string()], env: Vec::new() };
    let (mut guest, mut host) = tokio::io::duplex(4096);

    dispatch_exec_request_for_session(&request, &session, &mut host).await.unwrap();
    let exit = Frame::read_async(&mut guest).await.unwrap();
    assert!(matches!(exit.frame_type, FrameType::Exit));
    assert_eq!(i32::from_le_bytes(exit.payload.try_into().unwrap()), 0);

    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 2, "fresh remote transaction must sync before building");
    assert!(matches!(calls[0].request().operation(), crate::remote::RemoteOperation::Sync(_)));
    assert!(matches!(calls[1].request().operation(), crate::remote::RemoteOperation::Build(_)));
    assert_eq!(calls[0].target(), target);
    assert_eq!(calls[1].target(), target);
}

#[tokio::test]
async fn transparent_exec_request_keeps_selected_localhost_on_secured_local_executor() {
    let workspace = tempfile::tempdir().unwrap();
    let active = ActiveBuildTarget::new();
    let session = super::VsockSession {
        passthrough: Arc::new(vec!["/bin/touch *".to_string()]),
        env_mode: EnvMode::Relaxed,
        workspace: workspace.path().to_path_buf(),
        merged_profile: None,
        proxy_config: None,
        remote_router: Arc::new(RemoteRouter::new(active, BTreeMap::new())),
        local_session_id: WorkspaceSessionId([2; 16]),
        local_capabilities: Mutex::new(BTreeMap::new()),
    };
    let marker = workspace.path().join("local-executor-called");
    let request = crate::vscomm::ExecRequest {
        cwd: "/workspace".to_string(),
        command: "/bin/touch".to_string(),
        args: vec![marker.to_string_lossy().into_owned()],
        env: Vec::new(),
    };
    let (mut guest, mut host) = tokio::io::duplex(4096);

    dispatch_exec_request_for_session(&request, &session, &mut host).await.unwrap();
    let exit = Frame::read_async(&mut guest).await.unwrap();
    assert!(matches!(exit.frame_type, FrameType::Exit));
    assert_eq!(i32::from_le_bytes(exit.payload.try_into().unwrap()), 0);
    assert!(marker.is_file());
}

#[tokio::test]
async fn retained_remote_capability_remains_bound_after_switching_to_localhost() {
    let (root, catalog) = target_catalog_fixture();
    let active = ActiveBuildTarget::new();
    active.select(&catalog, "bsdbox").unwrap();
    let backend = Arc::new(TargetRecordingBackend { calls: Mutex::new(Vec::new()) });
    let session_id = WorkspaceSessionId([2; 16]);
    let target = RemoteTargetId([3; 16]);
    let session = target_session(root.path().to_path_buf(), active.clone(), backend.clone(), target, session_id);
    let (mut guest, mut host) = tokio::io::duplex(4096);
    let sync = WireRemoteRequest::sync(WireRequestId([6; 16]), WireWorkspaceSessionId([2; 16]));

    dispatch_remote_frame_for_session(sync.to_frame().unwrap(), &session, &mut host).await.unwrap();
    let sync_event = crate::vscomm::RemoteEvent::from_frame(Frame::read_async(&mut guest).await.unwrap()).unwrap();
    let crate::vscomm::RemoteEventKind::SyncCompleted { snapshot_id } = sync_event.kind else { panic!("expected sync completion") };

    active.select(&catalog, "localhost").unwrap();
    let build = WireRemoteRequest::build(
        WireRequestId([7; 16]),
        WireWorkspaceSessionId([2; 16]),
        WireRemoteBuild::new(
            WireWorkspaceRelativePath::new("src").unwrap(),
            WireRemoteTool::new("cargo").unwrap(),
            vec!["build".to_string()],
            Vec::new(),
            snapshot_id,
        )
        .unwrap(),
    );
    dispatch_remote_frame_for_session(build.to_frame().unwrap(), &session, &mut host).await.unwrap();
    let build_event = crate::vscomm::RemoteEvent::from_frame(Frame::read_async(&mut guest).await.unwrap()).unwrap();
    assert_eq!(build_event.kind, crate::vscomm::RemoteEventKind::Completed { exit_code: 0 });
    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].target(), target);
    assert_eq!(calls[1].target(), target);
}

#[tokio::test]
async fn local_capability_cannot_bypass_a_new_remote_target_selection() {
    let (root, catalog) = target_catalog_fixture();
    let active = ActiveBuildTarget::new();
    let backend = Arc::new(TargetRecordingBackend { calls: Mutex::new(Vec::new()) });
    let session_id = WorkspaceSessionId([2; 16]);
    let target = RemoteTargetId([3; 16]);
    let session = target_session(root.path().to_path_buf(), active.clone(), backend.clone(), target, session_id);
    let local_snapshot = RemoteSnapshotId::from_bytes([8; 16]);
    session.local_capabilities.lock().unwrap().insert(local_snapshot, ());
    active.select(&catalog, "bsdbox").unwrap();
    let build = WireRemoteRequest::build(
        WireRequestId([8; 16]),
        WireWorkspaceSessionId([2; 16]),
        WireRemoteBuild::new(
            WireWorkspaceRelativePath::new("src").unwrap(),
            WireRemoteTool::new("cargo").unwrap(),
            vec!["build".to_string()],
            Vec::new(),
            crate::vscomm::RemoteSnapshotId(*local_snapshot.as_bytes()),
        )
        .unwrap(),
    );
    let (_, mut host) = tokio::io::duplex(4096);

    assert!(dispatch_remote_frame_for_session(build.to_frame().unwrap(), &session, &mut host).await.is_err());
    assert!(backend.calls.lock().unwrap().is_empty());
    assert!(session.local_capabilities.lock().unwrap().contains_key(&local_snapshot));
}
