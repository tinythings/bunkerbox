use super::*;
use crate::remote::{
    RemoteAuthorizationPolicy, RemoteBuild, RemoteExecutionContext, RemoteRequest, RemoteSnapshotId, RemoteTool, RequestId, WorkspaceRelativePath,
    WorkspaceSessionId,
};
use crate::remote_target::RemoteTargetConfig;
use crate::snapshot::{SnapshotBuilder, SnapshotExclusionPolicy, SnapshotLimits, SnapshotStore};
use crate::worker_protocol::{self, WorkerErrorKind, WorkerMessage, WorkerOperation, WorkerSessionId, WorkerUploadEntry, WorkerUploadId};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::{tempdir, TempDir};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Clone, Copy)]
enum ScriptMode {
    Success,
    WrongVersion,
    ProtocolError,
}

struct ScriptedFactory {
    modes: Vec<ScriptMode>,
    next: AtomicUsize,
    specs: Arc<Mutex<Vec<SshLaunchSpec>>>,
    messages: Arc<Mutex<Vec<WorkerMessage>>>,
}

impl ScriptedFactory {
    fn new(modes: Vec<ScriptMode>) -> Arc<Self> {
        Arc::new(Self { modes, next: AtomicUsize::new(0), specs: Arc::new(Mutex::new(Vec::new())), messages: Arc::new(Mutex::new(Vec::new())) })
    }
}

impl SshProcessFactory for ScriptedFactory {
    fn spawn(&self, spec: &SshLaunchSpec) -> Result<Box<dyn SshProcess>, String> {
        let index = self.next.fetch_add(1, Ordering::Relaxed);
        let mode = *self.modes.get(index).ok_or_else(|| "unexpected scripted SSH process".to_string())?;
        self.specs.lock().unwrap().push(spec.clone());

        let (host_writer, worker_reader) = tokio::io::duplex(8192);
        let (worker_writer, host_reader) = tokio::io::duplex(8192);
        let (host_stderr, _worker_stderr) = tokio::io::duplex(256);
        let (status_tx, status_rx) = oneshot::channel();
        let messages = self.messages.clone();
        tokio::spawn(async move {
            let status = scripted_worker(worker_reader, worker_writer, mode, messages).await;
            let _ = status_tx.send(status);
        });

        Ok(Box::new(ScriptedProcess {
            stdin: Some(Box::new(host_writer)),
            stdout: Some(Box::new(host_reader)),
            stderr: Some(Box::new(host_stderr)),
            status: Some(status_rx),
            killed: Arc::new(AtomicBool::new(false)),
        }))
    }
}

struct ScriptedProcess {
    stdin: Option<WorkerWriter>,
    stdout: Option<WorkerReader>,
    stderr: Option<WorkerReader>,
    status: Option<oneshot::Receiver<i32>>,
    killed: Arc<AtomicBool>,
}

impl SshProcess for ScriptedProcess {
    fn take_stdin(&mut self) -> Option<WorkerWriter> {
        self.stdin.take()
    }

    fn take_stdout(&mut self) -> Option<WorkerReader> {
        self.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<WorkerReader> {
        self.stderr.take()
    }

    fn terminate_group(&mut self) {
        self.killed.store(true, Ordering::Relaxed);
    }

    fn kill_group(&mut self) {
        self.killed.store(true, Ordering::Relaxed);
    }

    fn wait<'a>(&'a mut self) -> crate::remote::RemoteFuture<'a, Result<i32, String>> {
        Box::pin(async move {
            let status = self.status.take().ok_or_else(|| "scripted process was already waited".to_string())?;
            let code = status.await.map_err(|_| "scripted worker stopped without an exit status".to_string())?;
            Ok(code)
        })
    }
}

async fn scripted_worker<R, W>(mut reader: R, mut writer: W, mode: ScriptMode, messages: Arc<Mutex<Vec<WorkerMessage>>>) -> i32
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let Ok(WorkerMessage::Hello { request_id, session_id, .. }) = worker_protocol::read_message(&mut reader).await else {
        return 71;
    };
    if matches!(mode, ScriptMode::WrongVersion) {
        let mut frame = WorkerMessage::hello(request_id, session_id, true).encode().unwrap();
        frame[4..6].copy_from_slice(&(worker_protocol::WORKER_PROTOCOL_VERSION + 1).to_le_bytes());
        writer.write_all(&frame).await.unwrap();
        return 0;
    }
    worker_protocol::write_message(&mut writer, &WorkerMessage::hello(request_id, session_id, true)).await.unwrap();

    let Ok(first) = worker_protocol::read_message(&mut reader).await else { return 72 };
    messages.lock().unwrap().push(first.clone());
    if matches!(mode, ScriptMode::ProtocolError) {
        worker_protocol::write_message(
            &mut writer,
            &WorkerMessage::error(request_id, session_id, WorkerOperation::Upload, WorkerErrorKind::WorkerProtocol, "scripted protocol failure"),
        )
        .await
        .unwrap();
        while worker_protocol::read_message(&mut reader).await.is_ok() {}
        return 0;
    }
    match first {
        WorkerMessage::UploadBegin { request_id, session_id, upload_id, .. } => loop {
            let Ok(message) = worker_protocol::read_message(&mut reader).await else { return 73 };
            messages.lock().unwrap().push(message.clone());
            match message {
                WorkerMessage::UploadFileChunk { .. } => {}
                WorkerMessage::UploadComplete { request_id: received_request, session_id: received_session, upload_id: received_upload } => {
                    if received_request != request_id || received_session != session_id || received_upload != upload_id {
                        return 74;
                    }
                    worker_protocol::write_message(
                        &mut writer,
                        &WorkerMessage::SyncProgress { request_id, session_id, upload_id, completed_bytes: 1, total_bytes: Some(1) },
                    )
                    .await
                    .unwrap();
                    worker_protocol::write_message(&mut writer, &WorkerMessage::UploadComplete { request_id, session_id, upload_id }).await.unwrap();
                    return 0;
                }
                _ => return 75,
            }
        },
        WorkerMessage::Build { request_id, session_id, build } => {
            messages.lock().unwrap().push(WorkerMessage::Build { request_id, session_id, build: build.clone() });
            worker_protocol::write_message(&mut writer, &WorkerMessage::stdout(request_id, session_id, b"remote stdout\n".to_vec())).await.unwrap();
            worker_protocol::write_message(&mut writer, &WorkerMessage::stderr(request_id, session_id, b"remote stderr\n".to_vec())).await.unwrap();
            worker_protocol::write_message(&mut writer, &WorkerMessage::completed(request_id, session_id, WorkerOperation::Build, 7)).await.unwrap();
            let Ok(cleanup) = worker_protocol::read_message(&mut reader).await else { return 76 };
            messages.lock().unwrap().push(cleanup.clone());
            if !matches!(cleanup, WorkerMessage::Cleanup { request_id: received_request, session_id: received_session, upload_token } if received_request == request_id && received_session == session_id && upload_token == build.upload_token())
            {
                return 77;
            }
            worker_protocol::write_message(&mut writer, &WorkerMessage::completed(request_id, session_id, WorkerOperation::Cleanup, 0))
                .await
                .unwrap();
            0
        }
        _ => 78,
    }
}

struct Fixture {
    _temp: TempDir,
    session: Arc<RunRemoteSession>,
    target: crate::remote::RemoteTargetId,
    session_id: WorkspaceSessionId,
    ssh_target: crate::remote_target::SshTarget,
}

fn fixture() -> Fixture {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir(&project).unwrap();
    fs::create_dir(project.join("src")).unwrap();
    fs::write(project.join("src/input.txt"), b"snapshot\n").unwrap();

    let key = temp.path().join("id_ed25519");
    fs::write(&key, b"test key").unwrap();
    set_mode(&key, 0o600);
    let known_hosts = temp.path().join("known_hosts");
    fs::write(&known_hosts, b"build.example.test ssh-ed25519 AAAA\n").unwrap();
    set_mode(&known_hosts, 0o644);
    let config_path = temp.path().join("remote-targets.yaml");
    let project_yaml = serde_yaml::to_string(&project).unwrap();
    let key_yaml = serde_yaml::to_string(&key).unwrap();
    let known_yaml = serde_yaml::to_string(&known_hosts).unwrap();
    let yaml = format!(
        "version: 1\ntargets:\n  test:\n    transport: ssh\n    host: build.example.test\n    port: 22\n    user: builder\n    identity-file: {}\n    known-hosts-file: {}\n    worker-path: /usr/local/libexec/bunkerbox-worker\n    workspace-root: /var/tmp/bunkerbox-workers\n    tools:\n      make: /usr/bin/make\n    environment:\n      PATH: /usr/bin\n    resources:\n      connect-timeout-seconds: 5\n      sync-timeout-seconds: 5\n      build-timeout-seconds: 5\n      max-output-bytes: 67108864\nprojects:\n  {}:\n    backend: ssh\n    target: test\n",
        key_yaml.trim(), known_yaml.trim(), project_yaml.trim()
    );
    fs::write(&config_path, yaml).unwrap();
    let config = RemoteTargetConfig::load_from(&config_path).unwrap();
    let ssh_target = config.resolve_for_project(&project).unwrap().target().unwrap().clone();

    let session_id = WorkspaceSessionId([1; 16]);
    let target = crate::remote::RemoteTargetId([2; 16]);
    let store = SnapshotStore::new(temp.path().join("snapshots"));
    let exclusions = SnapshotExclusionPolicy::from_patterns(Vec::<String>::new()).unwrap();
    let builder = SnapshotBuilder::new(store.clone(), SnapshotLimits::default(), exclusions);
    let session = Arc::new(RunRemoteSession::new(session_id, target, project, store, builder, temp.path().join("jobs")).unwrap());
    Fixture { _temp: temp, session, target, session_id, ssh_target }
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    #[cfg(not(unix))]
    let _ = (path, mode);
}

fn authorize_sync(fixture: &Fixture) -> crate::remote::AuthorizedRemoteRequest {
    let policy =
        RemoteAuthorizationPolicy::new(fixture.target, fixture.session_id, vec!["make".to_string()]).with_snapshot_authority(fixture.session.clone());
    policy
        .authorize(
            &RemoteExecutionContext { target: fixture.target, workspace_session_id: fixture.session_id },
            RemoteRequest::sync(RequestId([3; 16]), fixture.session_id),
        )
        .unwrap()
}

fn authorize_build(fixture: &Fixture, snapshot_id: RemoteSnapshotId) -> crate::remote::AuthorizedRemoteRequest {
    let build = RemoteBuild::new(
        WorkspaceRelativePath::new("src").unwrap(),
        RemoteTool::new("make").unwrap(),
        vec!["release".to_string(), "literal $(arg)".to_string()],
        vec![("CC".to_string(), "clang".to_string())],
        snapshot_id,
    )
    .unwrap();
    let policy =
        RemoteAuthorizationPolicy::new(fixture.target, fixture.session_id, vec!["make".to_string()]).with_snapshot_authority(fixture.session.clone());
    policy
        .authorize(
            &RemoteExecutionContext { target: fixture.target, workspace_session_id: fixture.session_id },
            RemoteRequest::build(RequestId([4; 16]), fixture.session_id, build),
        )
        .unwrap()
}

async fn collect(mut receiver: tokio::sync::mpsc::Receiver<RemoteBackendEvent>) -> Vec<RemoteBackendEvent> {
    let mut events = Vec::new();
    while let Some(event) = receiver.recv().await {
        events.push(event);
    }
    events
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripted_worker_proves_end_to_end_ssh_transport_and_exact_build_result() {
    let fixture = fixture();
    let factory = ScriptedFactory::new(vec![ScriptMode::Success, ScriptMode::Success]);
    let backend = SshBackend::new(fixture.session.clone(), fixture.ssh_target.clone()).unwrap().with_process_factory(factory.clone());

    let (sync_tx, sync_rx) = tokio::sync::mpsc::channel(64);
    backend.execute(authorize_sync(&fixture), sync_tx).await.unwrap();
    let sync_events = collect(sync_rx).await;
    let snapshot_id = sync_events.iter().find_map(|event| match event {
        RemoteBackendEvent::SyncCompleted { snapshot_id } => Some(*snapshot_id),
        _ => None,
    });
    let snapshot_id = snapshot_id.expect("scripted sync must retain a capability");

    let (build_tx, build_rx) = tokio::sync::mpsc::channel(64);
    backend.execute(authorize_build(&fixture, snapshot_id), build_tx).await.unwrap();
    let build_events = collect(build_rx).await;
    assert!(build_events.iter().any(|event| matches!(event, RemoteBackendEvent::Stdout(bytes) if bytes == b"remote stdout\n")));
    assert!(build_events.iter().any(|event| matches!(event, RemoteBackendEvent::Stderr(bytes) if bytes == b"remote stderr\n")));
    assert!(build_events.iter().any(|event| matches!(event, RemoteBackendEvent::Completed { exit_code: 7 })));
    assert_eq!(backend.pending_uploads(), 0);

    let messages = factory.messages.lock().unwrap();
    assert!(messages.iter().any(|message| matches!(message, WorkerMessage::UploadBegin { .. })));
    assert!(messages.iter().any(|message| matches!(message, WorkerMessage::Build { build, .. } if build.argv() == ["release", "literal $(arg)"])));
    assert!(messages.iter().any(|message| matches!(message, WorkerMessage::Cleanup { .. })));
    let specs = factory.specs.lock().unwrap();
    assert_eq!(specs.len(), 2);
    for spec in specs.iter() {
        let joined = spec.args().join(" ");
        assert!(joined.contains("StrictHostKeyChecking=yes"));
        assert!(joined.contains("IdentityAgent=none"));
        assert!(!joined.contains("literal $(arg)"));
        assert!(!joined.contains("snapshot_id"));
        assert_eq!(spec.remote_command(), "exec '/usr/local/libexec/bunkerbox-worker' --stdio --workspace-root '/var/tmp/bunkerbox-workers'");
    }
}

#[test]
fn launch_spec_contains_only_fixed_trusted_ssh_arguments() {
    let fixture = fixture();
    let spec = SshLaunchSpec::from_target(&fixture.ssh_target).unwrap();
    assert_eq!(spec.program(), Path::new("/usr/bin/ssh"));
    assert!(spec.args().windows(2).any(|pair| pair == ["-F", "/dev/null"]));
    assert!(spec.args().contains(&"BatchMode=yes".to_string()));
    assert!(spec.args().contains(&"RequestTTY=no".to_string()));
    assert!(spec.args().contains(&"ControlMaster=no".to_string()));
    assert!(spec.args().contains(&"EscapeChar=none".to_string()));
    assert!(!spec.args().iter().any(|arg| arg == "-tt" || arg == "accept-new" || arg.contains("SSH_AUTH_SOCK")));
}

#[test]
fn impossible_v1_uploads_fail_before_transport_spawn() {
    let entries = (0..worker_protocol::MAX_WORKER_UPLOAD_ENTRIES + 1)
        .map(|index| WorkerUploadEntry::directory(format!("d{index:04}"), 0o755).unwrap())
        .collect::<Vec<_>>();
    let error = preflight_upload([1; 16], WorkerSessionId([2; 16]), WorkerUploadId([3; 16]), &entries).unwrap_err();
    assert!(matches!(error, RemoteBackendError::Transport { class: RemoteFailureClass::SnapshotTransfer, .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_version_failure_is_typed_and_never_falls_back() {
    let fixture = fixture();
    let factory = ScriptedFactory::new(vec![ScriptMode::WrongVersion]);
    let backend = SshBackend::new(fixture.session.clone(), fixture.ssh_target.clone()).unwrap().with_process_factory(factory);
    let (tx, _rx) = tokio::sync::mpsc::channel(16);

    let error = backend.execute(authorize_sync(&fixture), tx).await.unwrap_err();
    assert!(matches!(error, RemoteBackendError::Transport { class: RemoteFailureClass::WorkerVersion, .. }));
    assert_eq!(backend.pending_uploads(), 0);
    assert_eq!(fixture.session.snapshot_capability_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_protocol_failure_is_terminal_without_local_fallback() {
    let fixture = fixture();
    let factory = ScriptedFactory::new(vec![ScriptMode::ProtocolError]);
    let backend = SshBackend::new(fixture.session.clone(), fixture.ssh_target.clone()).unwrap().with_process_factory(factory);
    let (tx, _rx) = tokio::sync::mpsc::channel(16);

    let error = backend.execute(authorize_sync(&fixture), tx).await.unwrap_err();
    assert!(matches!(error, RemoteBackendError::Transport { class: RemoteFailureClass::WorkerProtocol, .. }), "{error:?}");
    assert_eq!(fixture.session.snapshot_capability_count(), 0);
}
