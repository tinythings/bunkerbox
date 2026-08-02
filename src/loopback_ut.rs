use super::*;
use crate::remote::{RemoteAuthorizationPolicy, RemoteBuild, RemoteExecutionContext, RemoteRequest, RemoteTool, RequestId, WorkspaceRelativePath};
use tempfile::TempDir;

fn fixture() -> (TempDir, Arc<RunRemoteSession>, RemoteTargetId, WorkspaceSessionId) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(workspace.join("src")).unwrap();
    fs::write(workspace.join("src/input.txt"), b"snapshot input\n").unwrap();
    let session_id = WorkspaceSessionId([1; 16]);
    let target = RemoteTargetId([2; 16]);
    let snapshot_store = SnapshotStore::new(temp.path().join("snapshots"));
    let exclusions = crate::snapshot::SnapshotExclusionPolicy::from_patterns(Vec::<String>::new()).unwrap();
    let builder = SnapshotBuilder::new(snapshot_store.clone(), crate::snapshot::SnapshotLimits::default(), exclusions);
    let session = Arc::new(RunRemoteSession::new(session_id, target, workspace, snapshot_store, builder, temp.path().join("jobs")).unwrap());
    (temp, session, target, session_id)
}

fn authorized_build(
    target: RemoteTargetId, session: WorkspaceSessionId, tool: &str, args: Vec<String>, env: Vec<(String, String)>,
) -> crate::remote::AuthorizedRemoteRequest {
    let build = RemoteBuild::new(WorkspaceRelativePath::new("src").unwrap(), RemoteTool::new(tool).unwrap(), args, env).unwrap();
    let request = RemoteRequest::build(RequestId([3; 16]), session, build);
    let policy = RemoteAuthorizationPolicy::new(target, session, vec![tool.to_string()]);
    policy.authorize(&RemoteExecutionContext { target, workspace_session_id: session }, request).unwrap()
}

async fn collect_events(mut receiver: mpsc::Receiver<RemoteBackendEvent>) -> Vec<RemoteBackendEvent> {
    let mut events = Vec::new();
    while let Some(event) = receiver.recv().await {
        events.push(event);
    }
    events
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_and_build_materialize_a_bound_snapshot() {
    let (_temp, session, target, session_id) = fixture();
    let session_state = session.clone();
    let mut tools = BTreeMap::new();
    let printf = ["/usr/local/bin/printf", "/usr/bin/printf", "/bin/printf"].into_iter().map(PathBuf::from).find(|path| path.is_file()).unwrap();
    tools.insert("printf".to_string(), printf);
    let backend = LoopbackBackend::new(session, tools);

    let (sync_tx, sync_rx) = mpsc::channel(8);
    let sync = RemoteRequest::sync(RequestId([4; 16]), session_id);
    let policy = RemoteAuthorizationPolicy::new(target, session_id, Vec::new());
    let authorized = policy.authorize(&RemoteExecutionContext { target, workspace_session_id: session_id }, sync).unwrap();
    assert_eq!(backend.execute(authorized, sync_tx).await, Ok(()));
    assert_eq!(
        collect_events(sync_rx).await,
        vec![RemoteBackendEvent::SyncProgress { completed_bytes: 0, total_bytes: None }, RemoteBackendEvent::Completed { exit_code: 0 },]
    );

    let (build_tx, build_rx) = mpsc::channel(8);
    let authorized = authorized_build(target, session_id, "printf", vec!["value with spaces:$(literal)".to_string()], Vec::new());
    assert_eq!(backend.execute(authorized, build_tx).await, Ok(()));
    assert_eq!(
        collect_events(build_rx).await,
        vec![RemoteBackendEvent::Stdout(b"value with spaces:$(literal)".to_vec()), RemoteBackendEvent::Completed { exit_code: 0 },]
    );
    assert!(fs::read_dir(&session_state.jobs_root).unwrap().next().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonempty_remote_environment_is_rejected() {
    let (_temp, session, target, session_id) = fixture();
    let backend = LoopbackBackend::new(session, BTreeMap::new());
    let (events, _receiver) = mpsc::channel(8);
    let request = authorized_build(target, session_id, "printf", Vec::new(), vec![("UNTRUSTED".to_string(), "1".to_string())]);
    assert_eq!(
        backend.execute(request, events).await,
        Err(RemoteBackendError::Failed("remote environment is unavailable until remote environment policy is configured".to_string(),))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_preserves_argument_bytes_and_reports_nonzero_stderr() {
    let (_temp, session, target, session_id) = fixture();
    session.sync_snapshot().unwrap();
    let tools = resolve_fixed_tools(["ls".to_string()]);
    if tools.is_empty() {
        return;
    }
    let backend = LoopbackBackend::new(session, tools);
    let (events, receiver) = mpsc::channel(8);
    let request = authorized_build(target, session_id, "ls", vec!["$(not-a-shell-argument)".to_string()], Vec::new());
    assert_eq!(backend.execute(request, events).await, Ok(()));
    let events = collect_events(receiver).await;
    assert!(events.iter().any(|event| matches!(event, RemoteBackendEvent::Stderr(bytes) if !bytes.is_empty())));
    assert!(events.iter().any(|event| matches!(event, RemoteBackendEvent::Completed { exit_code } if *exit_code != 0)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_tool_fails_before_execution() {
    let (_temp, session, target, session_id) = fixture();
    let backend = LoopbackBackend::new(session, BTreeMap::new());
    let (events, _receiver) = mpsc::channel(8);
    let request = authorized_build(target, session_id, "missing-tool", Vec::new(), Vec::new());
    assert_eq!(backend.execute(request, events).await, Err(RemoteBackendError::Spawn("loopback tool is not configured: missing-tool".to_string())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_kills_a_direct_child_process() {
    let (_temp, session, target, session_id) = fixture();
    session.sync_snapshot().unwrap();
    let tools = resolve_fixed_tools(["sleep".to_string()]);
    if tools.is_empty() {
        return;
    }
    let backend = LoopbackBackend::new(session, tools).with_timeout(Duration::from_millis(50));
    let (events, _receiver) = mpsc::channel(8);
    let request = authorized_build(target, session_id, "sleep", vec!["5".to_string()], Vec::new());
    assert_eq!(backend.execute(request, events).await, Err(RemoteBackendError::Timeout));
}
