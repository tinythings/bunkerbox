use super::*;
use crate::artifact::{ArtifactLimits, ArtifactPolicy};
use crate::remote::{
    RemoteAuthorizationPolicy, RemoteBuild, RemoteExecutionContext, RemoteFailureClass, RemoteRequest, RemoteSnapshotAuthority, RemoteSnapshotId,
    RemoteTool, RequestId, WorkspaceRelativePath,
};
use std::env;
use std::os::unix::fs::PermissionsExt;
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

struct TestSnapshotAuthority;

impl RemoteSnapshotAuthority for TestSnapshotAuthority {
    fn snapshot_available(&self, _session: WorkspaceSessionId, _snapshot_id: RemoteSnapshotId) -> bool {
        true
    }
}

fn authorized_build(
    target: RemoteTargetId, session: WorkspaceSessionId, tool: &str, args: Vec<String>, env: Vec<(String, String)>, snapshot_id: RemoteSnapshotId,
) -> crate::remote::AuthorizedRemoteRequest {
    let build = RemoteBuild::new(WorkspaceRelativePath::new("src").unwrap(), RemoteTool::new(tool).unwrap(), args, env, snapshot_id).unwrap();
    let request = RemoteRequest::build(RequestId([3; 16]), session, build);
    let policy = RemoteAuthorizationPolicy::new(target, session, vec![tool.to_string()]).with_snapshot_authority(Arc::new(TestSnapshotAuthority));
    policy.authorize(&RemoteExecutionContext { target, workspace_session_id: session }, request).unwrap()
}

async fn collect_events(mut receiver: mpsc::Receiver<RemoteBackendEvent>) -> Vec<RemoteBackendEvent> {
    let mut events = Vec::new();
    while let Some(event) = receiver.recv().await {
        events.push(event);
    }
    events
}

async fn sync_capability(backend: &LoopbackBackend, target: RemoteTargetId, session: WorkspaceSessionId) -> RemoteSnapshotId {
    let request = RemoteRequest::sync(RequestId([4; 16]), session);
    let policy = RemoteAuthorizationPolicy::new(target, session, Vec::new());
    let authorized = policy.authorize(&RemoteExecutionContext { target, workspace_session_id: session }, request).unwrap();
    let (events, receiver) = mpsc::channel(8);
    assert_eq!(backend.execute(authorized, events).await, Ok(()));
    collect_events(receiver)
        .await
        .into_iter()
        .find_map(|event| match event {
            RemoteBackendEvent::SyncCompleted { snapshot_id } => Some(snapshot_id),
            _ => None,
        })
        .unwrap()
}

async fn diagnostic_sync(backend: &LoopbackBackend, target: RemoteTargetId, session: WorkspaceSessionId) -> Vec<RemoteBackendEvent> {
    let request = RemoteRequest::diagnostic_sync(RequestId([5; 16]), session);
    let policy = RemoteAuthorizationPolicy::new(target, session, Vec::new());
    let authorized = policy.authorize(&RemoteExecutionContext { target, workspace_session_id: session }, request).unwrap();
    let (events, receiver) = mpsc::channel(8);
    assert_eq!(backend.execute(authorized, events).await, Ok(()));
    collect_events(receiver).await
}

fn published_snapshot_count(session: &RunRemoteSession) -> usize {
    fs::read_dir(session.snapshot_store_root())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() != ".staging")
        .map(|entry| fs::read_dir(entry.path()).map(|entries| entries.filter_map(Result::ok).count()).unwrap_or_default())
        .sum()
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
    let sync_events = collect_events(sync_rx).await;
    assert!(matches!(sync_events.first(), Some(RemoteBackendEvent::SyncProgress { completed_bytes: 0, total_bytes: None })));
    let snapshot_id = sync_events
        .iter()
        .find_map(|event| match event {
            RemoteBackendEvent::SyncCompleted { snapshot_id } => Some(*snapshot_id),
            _ => None,
        })
        .unwrap();

    let (build_tx, build_rx) = mpsc::channel(8);
    let authorized = authorized_build(target, session_id, "printf", vec!["value with spaces:$(literal)".to_string()], Vec::new(), snapshot_id);
    assert_eq!(backend.execute(authorized, build_tx).await, Ok(()));
    assert_eq!(
        collect_events(build_rx).await,
        vec![RemoteBackendEvent::Stdout(b"value with spaces:$(literal)".to_vec()), RemoteBackendEvent::Completed { exit_code: 0 },]
    );
    assert!(fs::read_dir(&session_state.jobs_root).unwrap().next().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arbitrary_logical_tool_uses_its_trusted_target_mapping() {
    let (_temp, session, target, session_id) = fixture();
    let mut tools = resolve_fixed_tools(["printf".to_string()]);
    let Some(printf) = tools.remove("printf") else { return };
    tools.insert("build-my-car".into(), printf);
    let backend = LoopbackBackend::new(session, tools);
    let snapshot_id = sync_capability(&backend, target, session_id).await;
    let (events, receiver) = mpsc::channel(8);
    let request = authorized_build(target, session_id, "build-my-car", vec!["custom $(argument)".into()], Vec::new(), snapshot_id);
    assert_eq!(backend.execute(request, events).await, Ok(()));
    let events = collect_events(receiver).await;
    assert!(events.iter().any(|event| matches!(event, RemoteBackendEvent::Stdout(bytes) if bytes == b"custom $(argument)")));
    assert!(events.iter().any(|event| matches!(event, RemoteBackendEvent::Completed { exit_code: 0 })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnostic_sync_releases_capability_and_snapshot_storage() {
    let (_temp, session, target, session_id) = fixture();
    let backend = LoopbackBackend::new(session.clone(), BTreeMap::new());

    for _ in 0..(MAX_OUTSTANDING_SNAPSHOT_CAPABILITIES * 2) {
        let events = diagnostic_sync(&backend, target, session_id).await;
        assert!(events.iter().any(|event| matches!(event, RemoteBackendEvent::Completed { exit_code: 0 })));
        assert!(!events.iter().any(|event| matches!(event, RemoteBackendEvent::SyncCompleted { .. })));
        assert!(session.snapshot_capabilities.lock().unwrap().capabilities.is_empty());
        assert_eq!(published_snapshot_count(&session), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_diagnostic_sync_registers_nothing() {
    let (_temp, session, target, session_id) = fixture();
    fs::remove_dir_all(session.workspace_root()).unwrap();
    let backend = LoopbackBackend::new(session.clone(), BTreeMap::new());
    let request = RemoteRequest::diagnostic_sync(RequestId([5; 16]), session_id);
    let policy = RemoteAuthorizationPolicy::new(target, session_id, Vec::new());
    let authorized = policy.authorize(&RemoteExecutionContext { target, workspace_session_id: session_id }, request).unwrap();
    let (events, _receiver) = mpsc::channel(8);

    assert!(matches!(backend.execute(authorized, events).await, Err(RemoteBackendError::Failed(_))));
    assert!(session.snapshot_capabilities.lock().unwrap().capabilities.is_empty());
    assert_eq!(published_snapshot_count(&session), 0);
}

#[test]
fn outstanding_snapshot_capabilities_are_bounded_and_consumption_frees_a_slot() {
    let (temp, session, _target, _session_id) = fixture();
    let mut capabilities = Vec::new();
    for _ in 0..MAX_OUTSTANDING_SNAPSHOT_CAPABILITIES {
        capabilities.push(session.sync_snapshot().unwrap());
    }
    assert_eq!(session.snapshot_capabilities.lock().unwrap().capabilities.len(), MAX_OUTSTANDING_SNAPSHOT_CAPABILITIES);
    assert!(session.sync_snapshot().unwrap_err().contains("capability limit reached"));

    let claim = session.claim_snapshot(capabilities.pop().unwrap()).unwrap();
    drop(claim);
    assert_eq!(session.snapshot_capabilities.lock().unwrap().capabilities.len(), MAX_OUTSTANDING_SNAPSHOT_CAPABILITIES - 1);
    session.sync_snapshot().unwrap();

    let snapshot_root = session.snapshot_store_root();
    drop(session);
    assert!(!snapshot_root.exists());
    drop(temp);
}

#[test]
fn export_claim_uses_the_exact_capability_and_releases_its_snapshot() {
    let (_temp, session, _target, _session_id) = fixture();
    fs::write(session.workspace_root().join("src/input.txt"), b"A\n").unwrap();
    let snapshot_a = session.sync_snapshot().unwrap();
    fs::write(session.workspace_root().join("src/input.txt"), b"B\n").unwrap();
    let snapshot_b = session.sync_snapshot().unwrap();

    let claim_a = session.claim_snapshot_for_export(snapshot_a).unwrap();
    let entry_a = claim_a.entries().iter().find(|entry| entry.path().as_str() == "src/input.txt").unwrap();
    assert_eq!(claim_a.handle().session_id(), session.session_id());
    assert_eq!(claim_a.read_file(entry_a).unwrap(), b"A\n");
    assert!(claim_a.read_file_bounded(entry_a, 1).is_err());
    assert_eq!(claim_a.total_file_bytes(), 2);
    assert!(session.snapshot_capabilities.lock().unwrap().capabilities.contains_key(&snapshot_a));
    assert!(session.snapshot_capabilities.lock().unwrap().capabilities.contains_key(&snapshot_b));
    assert_eq!(published_snapshot_count(&session), 2);

    drop(claim_a);
    session.abort_snapshot_capability(snapshot_a).unwrap();
    assert_eq!(published_snapshot_count(&session), 1);

    let claim_b = session.claim_snapshot_for_export(snapshot_b).unwrap();
    let entry_b = claim_b.entries().iter().find(|entry| entry.path().as_str() == "src/input.txt").unwrap();
    assert_eq!(claim_b.read_file(entry_b).unwrap(), b"B\n");
    drop(claim_b);
    session.abort_snapshot_capability(snapshot_b).unwrap();
    assert_eq!(published_snapshot_count(&session), 0);
}

#[test]
fn export_claim_rejects_missing_replay_and_cross_session_capabilities() {
    let (_temp_a, session_a, _target_a, _session_id_a) = fixture();
    let snapshot_id = session_a.sync_snapshot().unwrap();
    let (_temp_b, session_b, _target_b, _session_id_b) = fixture();

    assert!(session_a.claim_snapshot_for_export(RemoteSnapshotId::from_bytes([9; 16])).is_err());
    assert!(session_b.claim_snapshot_for_export(snapshot_id).is_err());
    assert!(session_a.snapshot_capabilities.lock().unwrap().capabilities.contains_key(&snapshot_id));

    let claim = session_a.claim_snapshot_for_export(snapshot_id).unwrap();
    assert!(session_a.snapshot_capabilities.lock().unwrap().capabilities.contains_key(&snapshot_id));
    drop(claim);
    session_a.abort_snapshot_capability(snapshot_id).unwrap();
    assert_eq!(published_snapshot_count(&session_a), 0);
}

#[test]
fn aborting_an_unclaimed_capability_releases_snapshot_storage() {
    let (_temp, session, _target, _session_id) = fixture();
    let snapshot_id = session.sync_snapshot().unwrap();

    assert_eq!(published_snapshot_count(&session), 1);
    session.abort_snapshot_capability(snapshot_id).unwrap();
    assert_eq!(published_snapshot_count(&session), 0);
    assert!(session.claim_snapshot_for_export(snapshot_id).is_err());
    assert!(session.abort_snapshot_capability(snapshot_id).is_err());
}

#[test]
fn export_claim_keeps_session_cleanup_scoped_until_drop() {
    let (_temp, session, _target, _session_id) = fixture();
    let snapshot_id = session.sync_snapshot().unwrap();
    let snapshot_root = session.snapshot_store_root();
    let claim = session.claim_snapshot_for_export(snapshot_id).unwrap();

    drop(session);
    assert!(snapshot_root.exists());
    drop(claim);
    assert!(!snapshot_root.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interleaved_syncs_build_their_own_snapshot_capabilities() {
    let (_temp, session, target, session_id) = fixture();
    let tools = resolve_fixed_tools(["cat".to_string()]);
    if tools.is_empty() {
        return;
    }
    let backend = LoopbackBackend::new(session.clone(), tools);
    fs::write(session.workspace_root().join("src/input.txt"), b"A\n").unwrap();
    let snapshot_a = sync_capability(&backend, target, session_id).await;
    fs::write(session.workspace_root().join("src/input.txt"), b"B\n").unwrap();
    let snapshot_b = sync_capability(&backend, target, session_id).await;
    assert_ne!(snapshot_a, snapshot_b);

    let authority = session.clone();
    let authorize = |snapshot_id| {
        let build = RemoteBuild::new(
            WorkspaceRelativePath::new("src").unwrap(),
            RemoteTool::new("cat").unwrap(),
            vec!["input.txt".into()],
            Vec::new(),
            snapshot_id,
        )
        .unwrap();
        let request = RemoteRequest::build(RequestId([8; 16]), session_id, build);
        RemoteAuthorizationPolicy::new(target, session_id, vec!["cat".into()])
            .with_snapshot_authority(authority.clone())
            .authorize(&RemoteExecutionContext { target, workspace_session_id: session_id }, request)
            .unwrap()
    };
    let request_a = authorize(snapshot_a);
    let request_b = authorize(snapshot_b);
    let (events_a, receiver_a) = mpsc::channel(8);
    let (events_b, receiver_b) = mpsc::channel(8);
    let (result_a, result_b) = tokio::join!(backend.execute(request_a, events_a), backend.execute(request_b, events_b));
    assert_eq!(result_a, Ok(()));
    assert_eq!(result_b, Ok(()));
    let output_a = collect_events(receiver_a).await;
    let output_b = collect_events(receiver_b).await;
    assert!(output_a.iter().any(|event| matches!(event, RemoteBackendEvent::Stdout(bytes) if bytes == b"A\n")));
    assert!(output_b.iter().any(|event| matches!(event, RemoteBackendEvent::Stdout(bytes) if bytes == b"B\n")));
    assert!(output_a.iter().any(|event| matches!(event, RemoteBackendEvent::Completed { exit_code: 0 })));
    assert!(output_b.iter().any(|event| matches!(event, RemoteBackendEvent::Completed { exit_code: 0 })));

    let replay = RemoteBuild::new(
        WorkspaceRelativePath::new("src").unwrap(),
        RemoteTool::new("cat").unwrap(),
        vec!["input.txt".into()],
        Vec::new(),
        snapshot_a,
    )
    .unwrap();
    let replay_request = RemoteRequest::build(RequestId([9; 16]), session_id, replay);
    assert_eq!(
        RemoteAuthorizationPolicy::new(target, session_id, vec!["cat".into()])
            .with_snapshot_authority(session.clone())
            .authorize(&RemoteExecutionContext { target, workspace_session_id: session_id }, replay_request),
        Err(crate::remote::RemoteAuthorizationError::SnapshotNotAllowed)
    );

    let unknown = RemoteBuild::new(
        WorkspaceRelativePath::new("src").unwrap(),
        RemoteTool::new("cat").unwrap(),
        vec!["input.txt".into()],
        Vec::new(),
        RemoteSnapshotId::from_bytes([6; 16]),
    )
    .unwrap();
    let unknown_request = RemoteRequest::build(RequestId([10; 16]), session_id, unknown);
    assert_eq!(
        RemoteAuthorizationPolicy::new(target, session_id, vec!["cat".into()])
            .with_snapshot_authority(session.clone())
            .authorize(&RemoteExecutionContext { target, workspace_session_id: session_id }, unknown_request),
        Err(crate::remote::RemoteAuthorizationError::SnapshotNotAllowed)
    );

    let other_session = WorkspaceSessionId([7; 16]);
    let cross_session = RemoteBuild::new(
        WorkspaceRelativePath::new("src").unwrap(),
        RemoteTool::new("cat").unwrap(),
        vec!["input.txt".into()],
        Vec::new(),
        snapshot_b,
    )
    .unwrap();
    let cross_request = RemoteRequest::build(RequestId([11; 16]), other_session, cross_session);
    assert_eq!(
        RemoteAuthorizationPolicy::new(target, other_session, vec!["cat".into()])
            .with_snapshot_authority(session)
            .authorize(&RemoteExecutionContext { target, workspace_session_id: other_session }, cross_request),
        Err(crate::remote::RemoteAuthorizationError::SnapshotNotAllowed)
    );
}

#[test]
fn unapproved_remote_environment_is_rejected_before_backend_execution() {
    let (_temp, _session, target, session_id) = fixture();
    let build = RemoteBuild::new(
        WorkspaceRelativePath::new("src").unwrap(),
        RemoteTool::new("printf").unwrap(),
        Vec::new(),
        vec![("UNTRUSTED".to_string(), "1".to_string())],
        RemoteSnapshotId::from_bytes([9; 16]),
    )
    .unwrap();
    let request = RemoteRequest::build(RequestId([3; 16]), session_id, build);
    let policy =
        RemoteAuthorizationPolicy::new(target, session_id, vec!["printf".to_string()]).with_snapshot_authority(Arc::new(TestSnapshotAuthority));
    assert_eq!(
        policy.authorize(&RemoteExecutionContext { target, workspace_session_id: session_id }, request),
        Err(crate::remote::RemoteAuthorizationError::EnvironmentNotAllowed("UNTRUSTED".to_string()))
    );
}

#[test]
fn fixed_tool_resolution_never_uses_guest_wrapper_path() {
    let tools = resolve_fixed_tools(["make".to_string()]);
    if let Some(path) = tools.get("make") {
        assert!(matches!(path.to_str(), Some(value) if value == "/usr/local/bin/make" || value == "/usr/bin/make" || value == "/bin/make"));
        assert!(!path.starts_with("/usr/local/bunkerbox/bin"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_preserves_argument_bytes_and_reports_nonzero_stderr() {
    let (_temp, session, target, session_id) = fixture();
    let snapshot_id = session.sync_snapshot().unwrap();
    let tools = resolve_fixed_tools(["ls".to_string()]);
    if tools.is_empty() {
        return;
    }
    let backend = LoopbackBackend::new(session, tools);
    let (events, receiver) = mpsc::channel(8);
    let request = authorized_build(target, session_id, "ls", vec!["$(not-a-shell-argument)".to_string()], Vec::new(), snapshot_id);
    assert_eq!(backend.execute(request, events).await, Ok(()));
    let events = collect_events(receiver).await;
    assert!(events.iter().any(|event| matches!(event, RemoteBackendEvent::Stderr(bytes) if !bytes.is_empty())));
    assert!(events.iter().any(|event| matches!(event, RemoteBackendEvent::Completed { exit_code } if *exit_code != 0)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_exit_does_not_wait_for_inherited_output_pipes() {
    let (_temp, session, target, session_id) = fixture();
    fs::write(session.workspace_root().join("src/Makefile"), ".PHONY: leak\nleak:\n\t@sleep 30 & echo $$!\n\t@printf 'direct-output\\n'\n").unwrap();
    let snapshot_id = session.sync_snapshot().unwrap();
    let tools = resolve_fixed_tools(["make".to_string()]);
    if tools.is_empty() {
        return;
    }

    let backend = LoopbackBackend::new(session.clone(), tools);
    let (events, receiver) = mpsc::channel(16);
    let request = authorized_build(target, session_id, "make", vec!["leak".into()], Vec::new(), snapshot_id);
    let result = tokio::time::timeout(Duration::from_secs(2), backend.execute(request, events)).await.unwrap();
    assert_eq!(result, Ok(()));

    let events = collect_events(receiver).await;
    let stdout = events
        .iter()
        .filter_map(|event| match event {
            RemoteBackendEvent::Stdout(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    assert!(stdout.windows(b"direct-output\n".len()).any(|window| window == b"direct-output\n"));
    let pid = std::str::from_utf8(&stdout).unwrap().split_whitespace().find_map(|value| value.parse::<libc::pid_t>().ok()).unwrap();

    let terminal_events = events
        .iter()
        .filter(|event| matches!(event, RemoteBackendEvent::Error { .. } | RemoteBackendEvent::Cancelled | RemoteBackendEvent::Completed { .. }))
        .collect::<Vec<_>>();
    assert_eq!(terminal_events, vec![&RemoteBackendEvent::Completed { exit_code: 0 }]);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while super::process_is_alive(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!super::process_is_alive(pid));
    assert!(fs::read_dir(&session.jobs_root).unwrap().next().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_receives_guest_environment_but_trusted_target_wins() {
    let (_temp, session, target, session_id) = fixture();
    let snapshot_id = session.sync_snapshot().unwrap();
    let tools = resolve_fixed_tools(["printenv".to_string()]);
    if tools.is_empty() {
        return;
    }
    let backend = LoopbackBackend::new(session, tools).with_target_environment(BTreeMap::from([("CC".into(), "trusted-target".into())]));
    let (events, receiver) = mpsc::channel(8);
    let request = authorized_build(target, session_id, "printenv", vec!["CC".into()], vec![("CC".into(), "guest-value".into())], snapshot_id);
    assert_eq!(backend.execute(request, events).await, Ok(()));
    let events = collect_events(receiver).await;
    assert!(events.iter().any(|event| matches!(event, RemoteBackendEvent::Stdout(bytes) if bytes == b"trusted-target\n")));
    assert!(events.iter().any(|event| matches!(event, RemoteBackendEvent::Completed { exit_code: 0 })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_tool_fails_before_execution() {
    let (_temp, session, target, session_id) = fixture();
    let snapshot_id = session.sync_snapshot().unwrap();
    let backend = LoopbackBackend::new(session, BTreeMap::new());
    let (events, _receiver) = mpsc::channel(8);
    let request = authorized_build(target, session_id, "missing-tool", Vec::new(), Vec::new(), snapshot_id);
    assert_eq!(backend.execute(request, events).await, Err(RemoteBackendError::Spawn("loopback tool is not configured: missing-tool".to_string())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_kills_a_direct_child_process() {
    let (_temp, session, target, session_id) = fixture();
    let snapshot_id = session.sync_snapshot().unwrap();
    let tools = resolve_fixed_tools(["sleep".to_string()]);
    if tools.is_empty() {
        return;
    }
    let backend = LoopbackBackend::new(session, tools).with_timeout(Duration::from_millis(50));
    let (events, _receiver) = mpsc::channel(8);
    let request = authorized_build(target, session_id, "sleep", vec!["5".to_string()], Vec::new(), snapshot_id);
    assert_eq!(backend.execute(request, events).await, Err(RemoteBackendError::Deadline { cause: crate::remote::RemoteTimeoutCause::Build }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_limit_kills_a_flooding_direct_child() {
    let (_temp, session, target, session_id) = fixture();
    let snapshot_id = session.sync_snapshot().unwrap();
    let tools = resolve_fixed_tools(["printf".to_string()]);
    if tools.is_empty() {
        return;
    }
    let backend = LoopbackBackend::new(session, tools).with_output_limit(8);
    let (events, _receiver) = mpsc::channel(8);
    let request = authorized_build(target, session_id, "printf", vec!["0123456789".into()], Vec::new(), snapshot_id);
    assert_eq!(backend.execute(request, events).await, Err(RemoteBackendError::OutputLimit { limit: 8 }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_retrieves_only_trusted_declared_artifacts_after_successful_build() {
    let (_temp, session, target, session_id) = fixture();
    fs::write(session.workspace_root().join("src/Makefile"), ".PHONY: artifact\nartifact:\n\t@printf 'data' > result\n").unwrap();
    let snapshot_id = session.sync_snapshot().unwrap();
    let tools = resolve_fixed_tools(["make".to_string()]);
    if tools.is_empty() {
        return;
    }
    let backend = LoopbackBackend::new(session.clone(), tools)
        .with_artifacts(ArtifactPolicy::new(vec!["src/result".to_string()]).unwrap(), ArtifactLimits::default());
    let (events, receiver) = mpsc::channel(16);
    let request = authorized_build(target, session_id, "make", vec!["artifact".into()], Vec::new(), snapshot_id);
    assert_eq!(backend.execute(request, events).await, Ok(()));
    let events = collect_events(receiver).await;
    assert!(events.iter().any(|event| matches!(event, RemoteBackendEvent::Completed { exit_code: 0 })));
    assert_eq!(fs::read(session.workspace_root().join(".bunkerbox/artifacts/03030303030303030303030303030303/src/result")).unwrap(), b"data");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_missing_required_artifact_is_a_terminal_artifact_failure() {
    let (_temp, session, target, session_id) = fixture();
    fs::write(session.workspace_root().join("src/Makefile"), ".PHONY: ok\nok:\n\t@true\n").unwrap();
    let snapshot_id = session.sync_snapshot().unwrap();
    let tools = resolve_fixed_tools(["make".to_string()]);
    if tools.is_empty() {
        return;
    }
    let backend = LoopbackBackend::new(session.clone(), tools)
        .with_artifacts(ArtifactPolicy::new(vec!["src/missing".to_string()]).unwrap(), ArtifactLimits::default());
    let (events, receiver) = mpsc::channel(16);
    let request = authorized_build(target, session_id, "make", vec!["ok".into()], Vec::new(), snapshot_id);
    assert!(matches!(backend.execute(request, events).await, Err(RemoteBackendError::Transport { class: RemoteFailureClass::ArtifactManifest, .. })));
    let events = collect_events(receiver).await;
    assert!(!events.iter().any(|event| matches!(event, RemoteBackendEvent::Completed { .. })));
    assert!(fs::read_dir(&session.jobs_root).unwrap().next().is_none());
}

fn cargo_fixture_session(temp: &TempDir) -> (Arc<RunRemoteSession>, RemoteTargetId, WorkspaceSessionId) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/remote-cargo");
    let workspace = temp.path().join("cargo-workspace");
    fs::create_dir_all(workspace.join("src")).unwrap();
    for relative in ["Cargo.toml", "build.rs", "src/main.rs"] {
        fs::copy(source.join(relative), workspace.join(relative)).unwrap();
    }
    let session_id = WorkspaceSessionId([11; 16]);
    let target = RemoteTargetId([12; 16]);
    let snapshot_store = SnapshotStore::new(temp.path().join("cargo-snapshots"));
    let exclusions = crate::snapshot::SnapshotExclusionPolicy::from_patterns(Vec::<String>::new()).unwrap();
    let builder = SnapshotBuilder::new(snapshot_store.clone(), crate::snapshot::SnapshotLimits::default(), exclusions);
    let session = Arc::new(RunRemoteSession::new(session_id, target, workspace, snapshot_store, builder, temp.path().join("cargo-jobs")).unwrap());
    (session, target, session_id)
}

fn cargo_executable() -> PathBuf {
    env::var_os("PATH")
        .into_iter()
        .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join("cargo"))
        .find(|path| path.is_file() && fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0))
        .expect("Cargo must be available on PATH for the remote Cargo fixture")
}

fn cargo_target_environment() -> BTreeMap<String, String> {
    let mut environment = BTreeMap::new();
    for name in ["PATH", "HOME", "RUSTUP_HOME", "CARGO_HOME"] {
        if let Ok(value) = env::var(name) {
            environment.insert(name.to_string(), value);
        }
    }
    environment
}

fn authorized_cargo_build(
    target: RemoteTargetId, session: WorkspaceSessionId, request_id: u8, args: Vec<String>, snapshot_id: RemoteSnapshotId,
) -> crate::remote::AuthorizedRemoteRequest {
    let build = RemoteBuild::new(WorkspaceRelativePath::new("").unwrap(), RemoteTool::new("cargo").unwrap(), args, Vec::new(), snapshot_id).unwrap();
    let request = RemoteRequest::build(RequestId([request_id; 16]), session, build);
    let policy = RemoteAuthorizationPolicy::new(target, session, vec!["cargo".to_string()]).with_snapshot_authority(Arc::new(TestSnapshotAuthority));
    policy.authorize(&RemoteExecutionContext { target, workspace_session_id: session }, request).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cargo_fixture_runs_build_script_nested_cargo_and_trusted_artifact_flow() {
    let temp = tempfile::tempdir().unwrap();
    let (session, target, session_id) = cargo_fixture_session(&temp);
    let cargo = cargo_executable();
    let mut tools = BTreeMap::new();
    tools.insert("cargo".to_string(), cargo);
    let target_environment = cargo_target_environment();
    let artifact = "target/debug/bunkerbox-cargo-fixture-artifact.txt".to_string();
    let backend = LoopbackBackend::new(session.clone(), tools.clone())
        .with_target_environment(target_environment.clone())
        .with_artifacts(ArtifactPolicy::new(vec![artifact.clone()]).unwrap(), ArtifactLimits::default());
    let snapshot_id = sync_capability(&backend, target, session_id).await;
    let (events, receiver) = mpsc::channel(64);
    assert_eq!(
        backend.execute(authorized_cargo_build(target, session_id, 13, vec!["build".into(), "--offline".into()], snapshot_id), events).await,
        Ok(())
    );
    let events = collect_events(receiver).await;
    let output = events
        .iter()
        .filter_map(|event| match event {
            RemoteBackendEvent::Stdout(bytes) | RemoteBackendEvent::Stderr(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    assert!(String::from_utf8_lossy(&output).contains("bunkerbox-cargo-fixture-build-script"));
    assert!(events.iter().any(|event| matches!(event, RemoteBackendEvent::Completed { exit_code: 0 })));
    assert!(session
        .workspace_root()
        .join(".bunkerbox/artifacts/0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d/target/debug/bunkerbox-cargo-fixture-artifact.txt")
        .is_file());

    let backend = LoopbackBackend::new(session.clone(), tools)
        .with_target_environment(target_environment)
        .with_artifacts(ArtifactPolicy::new(vec!["target/debug/missing-cargo-artifact".to_string()]).unwrap(), ArtifactLimits::default());
    let snapshot_id = sync_capability(&backend, target, session_id).await;
    let (events, receiver) = mpsc::channel(64);
    assert!(matches!(
        backend.execute(authorized_cargo_build(target, session_id, 14, vec!["build".into(), "--offline".into()], snapshot_id), events).await,
        Err(RemoteBackendError::Transport { class: RemoteFailureClass::ArtifactManifest, .. })
    ));
    let events = collect_events(receiver).await;
    assert!(!events.iter().any(|event| matches!(event, RemoteBackendEvent::Completed { .. })));
    assert!(fs::read_dir(&session.jobs_root).unwrap().next().is_none());
}
