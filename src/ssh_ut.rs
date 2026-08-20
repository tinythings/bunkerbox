use super::*;
use crate::artifact::{ArtifactLimits, ArtifactPolicy};
use crate::cfg::ProjectConfig;
use crate::remote::{
    RemoteAuthorizationPolicy, RemoteBuild, RemoteExecutionContext, RemoteRequest, RemoteSnapshotId, RemoteTool, RequestId, WorkspaceRelativePath,
    WorkspaceSessionId,
};
use crate::remote_target::{BuildTargetCatalog, RemoteTargetConfig};
use crate::snapshot::{SnapshotBuilder, SnapshotExclusionPolicy, SnapshotLimits, SnapshotStore};
use crate::worker_protocol::{
    self, WorkerArtifactEntry, WorkerArtifactSetId, WorkerErrorKind, WorkerMessage, WorkerOperation, WorkerSessionId, WorkerUploadEntry,
    WorkerUploadId,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::{tempdir, TempDir};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[test]
fn compact_target_launch_uses_host_ssh_defaults_and_fixed_worker() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir(&project).unwrap();
    let bunkerbox = project.join(".bunkerbox");
    fs::create_dir(&bunkerbox).unwrap();
    fs::write(
        bunkerbox.join(crate::remote_target::REMOTE_PROJECT_CONFIG_FILE_NAME),
        "targets:\n  build:\n    ssh: builder@build.example.test:2200\n    workspace: /var/tmp/bunkerbox\n",
    )
    .unwrap();
    let catalog = BuildTargetCatalog::load_optional(&project, &ProjectConfig::default()).unwrap().unwrap();
    let target = catalog.remote("build").unwrap().target();
    let spec = SshLaunchSpec::from_target(target).unwrap();
    assert_eq!(spec.program(), Path::new("/usr/bin/ssh"));
    assert!(spec.args().windows(2).all(|pair| pair != ["-F", "/dev/null"]));
    assert!(!spec.args().iter().any(|arg| arg == "-i" || arg == "UserKnownHostsFile=/dev/null"));
    assert!(spec.args().windows(2).any(|pair| pair == ["-p", "2200"]));
    assert!(spec.remote_command().contains("/usr/local/libexec/bunkerbox-worker"));
    assert!(spec.remote_command().contains("--stdio"));
}

#[derive(Clone, Copy)]
enum ScriptMode {
    Success,
    WrongVersion,
    ProtocolError,
    ExitBeforeHello { diagnostic_bytes: usize },
    ExitDuringUpload { diagnostic_bytes: usize },
    ArtifactSuccess,
    ArtifactManifestError,
    ArtifactTransferError,
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
        let (host_stderr, worker_stderr) = tokio::io::duplex(MAX_SSH_DIAGNOSTIC_BYTES + 1024);
        let (status_tx, status_rx) = oneshot::channel();
        let messages = self.messages.clone();
        tokio::spawn(async move {
            let status = scripted_worker(worker_reader, worker_writer, worker_stderr, mode, messages).await;
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

async fn scripted_worker<R, W, E>(mut reader: R, mut writer: W, mut stderr: E, mode: ScriptMode, messages: Arc<Mutex<Vec<WorkerMessage>>>) -> i32
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    if let ScriptMode::ExitBeforeHello { diagnostic_bytes } = mode {
        let _ = worker_protocol::read_message_versioned(&mut reader).await;
        write_scripted_diagnostic(&mut stderr, diagnostic_bytes).await;
        return 79;
    }
    let Ok((hello_version, WorkerMessage::Hello { request_id, session_id, .. })) = worker_protocol::read_message_versioned(&mut reader).await else {
        return 71;
    };
    if matches!(mode, ScriptMode::WrongVersion) {
        let mut frame = WorkerMessage::hello(request_id, session_id, true).encode().unwrap();
        frame[4..6].copy_from_slice(&(worker_protocol::WORKER_PROTOCOL_VERSION + 1).to_le_bytes());
        writer.write_all(&frame).await.unwrap();
        return 0;
    }
    worker_protocol::write_message_versioned(
        &mut writer,
        &WorkerMessage::hello_for_version(request_id, session_id, true, hello_version),
        hello_version,
    )
    .await
    .unwrap();

    let Ok((_, first)) = worker_protocol::read_message_versioned(&mut reader).await else { return 72 };
    messages.lock().unwrap().push(first.clone());
    if let ScriptMode::ExitDuringUpload { diagnostic_bytes } = mode {
        write_scripted_diagnostic(&mut stderr, diagnostic_bytes).await;
        return 80;
    }
    if matches!(mode, ScriptMode::ProtocolError) {
        worker_protocol::write_message_versioned(
            &mut writer,
            &WorkerMessage::error(request_id, session_id, WorkerOperation::Upload, WorkerErrorKind::WorkerProtocol, "scripted protocol failure"),
            hello_version,
        )
        .await
        .unwrap();
        while worker_protocol::read_message_versioned(&mut reader).await.is_ok() {}
        return 0;
    }
    match first {
        WorkerMessage::UploadBegin { request_id, session_id, upload_id, .. } => loop {
            let Ok((_, message)) = worker_protocol::read_message_versioned(&mut reader).await else { return 73 };
            messages.lock().unwrap().push(message.clone());
            match message {
                WorkerMessage::UploadFileChunk { .. } => {}
                WorkerMessage::UploadComplete { request_id: received_request, session_id: received_session, upload_id: received_upload } => {
                    if received_request != request_id || received_session != session_id || received_upload != upload_id {
                        return 74;
                    }
                    worker_protocol::write_message_versioned(
                        &mut writer,
                        &WorkerMessage::SyncProgress { request_id, session_id, upload_id, completed_bytes: 1, total_bytes: Some(1) },
                        hello_version,
                    )
                    .await
                    .unwrap();
                    worker_protocol::write_message_versioned(
                        &mut writer,
                        &WorkerMessage::UploadComplete { request_id, session_id, upload_id },
                        hello_version,
                    )
                    .await
                    .unwrap();
                    return 0;
                }
                _ => return 75,
            }
        },
        WorkerMessage::Build { request_id, session_id, build } => {
            messages.lock().unwrap().push(WorkerMessage::Build { request_id, session_id, build: build.clone() });
            worker_protocol::write_message_versioned(
                &mut writer,
                &WorkerMessage::stdout(request_id, session_id, b"remote stdout\n".to_vec()),
                hello_version,
            )
            .await
            .unwrap();
            worker_protocol::write_message_versioned(
                &mut writer,
                &WorkerMessage::stderr(request_id, session_id, b"remote stderr\n".to_vec()),
                hello_version,
            )
            .await
            .unwrap();
            let exit_code = if matches!(mode, ScriptMode::ArtifactSuccess | ScriptMode::ArtifactManifestError | ScriptMode::ArtifactTransferError) {
                0
            } else {
                7
            };
            worker_protocol::write_message_versioned(
                &mut writer,
                &WorkerMessage::completed(request_id, session_id, WorkerOperation::Build, exit_code),
                hello_version,
            )
            .await
            .unwrap();
            if matches!(mode, ScriptMode::ArtifactManifestError) {
                worker_protocol::write_message_versioned(
                    &mut writer,
                    &WorkerMessage::error(
                        request_id,
                        session_id,
                        WorkerOperation::Artifact,
                        WorkerErrorKind::Artifact,
                        "configured artifact is missing",
                    ),
                    hello_version,
                )
                .await
                .unwrap();
            } else if matches!(mode, ScriptMode::ArtifactSuccess | ScriptMode::ArtifactTransferError) {
                let digest: [u8; 32] = Sha256::digest(b"data").into();
                worker_protocol::write_message_versioned(
                    &mut writer,
                    &WorkerMessage::ArtifactManifest {
                        request_id,
                        session_id,
                        artifact_set_id: WorkerArtifactSetId([9; 16]),
                        entries: vec![WorkerArtifactEntry::new("result", 0o644, 4, digest).unwrap()],
                        total_bytes: 4,
                    },
                    hello_version,
                )
                .await
                .unwrap();
                let Ok((_, fetch)) = worker_protocol::read_message_versioned(&mut reader).await else { return 76 };
                messages.lock().unwrap().push(fetch.clone());
                if !matches!(fetch, WorkerMessage::FetchArtifact { artifact_set_id, entry_index: 0, .. } if artifact_set_id == WorkerArtifactSetId([9; 16]))
                {
                    return 77;
                }
                let data = if matches!(mode, ScriptMode::ArtifactTransferError) { b"da".to_vec() } else { b"data".to_vec() };
                worker_protocol::write_message_versioned(
                    &mut writer,
                    &WorkerMessage::ArtifactChunk {
                        request_id,
                        session_id,
                        artifact_set_id: WorkerArtifactSetId([9; 16]),
                        entry_index: 0,
                        offset: 0,
                        data,
                    },
                    hello_version,
                )
                .await
                .unwrap();
                worker_protocol::write_message_versioned(
                    &mut writer,
                    &WorkerMessage::ArtifactComplete { request_id, session_id, artifact_set_id: WorkerArtifactSetId([9; 16]), entry_index: 0 },
                    hello_version,
                )
                .await
                .unwrap();
            }
            let Ok((_, cleanup)) = worker_protocol::read_message_versioned(&mut reader).await else { return 76 };
            messages.lock().unwrap().push(cleanup.clone());
            if !matches!(cleanup, WorkerMessage::Cleanup { request_id: received_request, session_id: received_session, upload_token } if received_request == request_id && received_session == session_id && upload_token == build.upload_token())
            {
                return 77;
            }
            worker_protocol::write_message_versioned(
                &mut writer,
                &WorkerMessage::completed(request_id, session_id, WorkerOperation::Cleanup, 0),
                hello_version,
            )
            .await
            .unwrap();
            0
        }
        _ => 78,
    }
}

async fn write_scripted_diagnostic<W: AsyncWrite + Unpin>(stderr: &mut W, bytes: usize) {
    let mut diagnostic = Vec::new();
    while diagnostic.len() < bytes {
        diagnostic.extend_from_slice(b"scripted worker diagnostic: missing worker dependency\n");
    }
    diagnostic.truncate(bytes);
    let _ = stderr.write_all(&diagnostic).await;
    let _ = stderr.flush().await;
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
        assert!(spec
            .remote_command()
            .starts_with("exec '/usr/local/libexec/bunkerbox-worker' --stdio --workspace-root '/var/tmp/bunkerbox-workers'"));
        assert!(spec.remote_command().contains("--build-timeout-ms 5000"));
        assert!(spec.remote_command().contains("--max-worker-uploads 2"));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_handshake_failure_includes_worker_stderr() {
    let fixture = fixture();
    let factory = ScriptedFactory::new(vec![ScriptMode::ExitBeforeHello { diagnostic_bytes: 96 }]);
    let backend = SshBackend::new(fixture.session.clone(), fixture.ssh_target.clone()).unwrap().with_process_factory(factory);
    let (tx, _rx) = tokio::sync::mpsc::channel(16);

    let error = backend.execute(authorize_sync(&fixture), tx).await.unwrap_err();
    let RemoteBackendError::Transport { class, message } = error else { panic!("expected transport failure") };
    assert_eq!(class, RemoteFailureClass::WorkerProtocol);
    assert!(message.contains("invalid worker protocol: truncated worker frame header"));
    assert!(message.contains("\nworker stderr: scripted worker diagnostic: missing worker dependency"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_upload_failure_includes_worker_stderr() {
    let fixture = fixture();
    let factory = ScriptedFactory::new(vec![ScriptMode::ExitDuringUpload { diagnostic_bytes: 96 }]);
    let backend = SshBackend::new(fixture.session.clone(), fixture.ssh_target.clone()).unwrap().with_process_factory(factory);
    let (tx, _rx) = tokio::sync::mpsc::channel(16);

    let error = backend.execute(authorize_sync(&fixture), tx).await.unwrap_err();
    let RemoteBackendError::Transport { message, .. } = error else { panic!("expected transport failure") };
    assert!(message.contains("worker I/O error") || message.contains("invalid worker protocol"));
    assert!(message.contains("\nworker stderr: scripted worker diagnostic: missing worker dependency"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_failure_without_worker_stderr_preserves_the_original_error() {
    let fixture = fixture();
    let factory = ScriptedFactory::new(vec![ScriptMode::ExitBeforeHello { diagnostic_bytes: 0 }]);
    let backend = SshBackend::new(fixture.session.clone(), fixture.ssh_target.clone()).unwrap().with_process_factory(factory);
    let (tx, _rx) = tokio::sync::mpsc::channel(16);

    let error = backend.execute(authorize_sync(&fixture), tx).await.unwrap_err();
    let RemoteBackendError::Transport { class, message } = error else { panic!("expected transport failure") };
    assert_eq!(class, RemoteFailureClass::WorkerProtocol);
    assert_eq!(message, "invalid worker protocol: truncated worker frame header");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_worker_stderr_diagnostic_is_bounded() {
    let fixture = fixture();
    let factory = ScriptedFactory::new(vec![ScriptMode::ExitBeforeHello { diagnostic_bytes: MAX_SSH_DIAGNOSTIC_BYTES + 1024 }]);
    let backend = SshBackend::new(fixture.session.clone(), fixture.ssh_target.clone()).unwrap().with_process_factory(factory);
    let (tx, _rx) = tokio::sync::mpsc::channel(16);

    let error = backend.execute(authorize_sync(&fixture), tx).await.unwrap_err();
    let RemoteBackendError::Transport { message, .. } = error else { panic!("expected transport failure") };
    let diagnostic = message.split_once("\nworker stderr: ").map(|(_, diagnostic)| diagnostic).expect("worker diagnostic is present");
    assert_eq!(diagnostic.len(), MAX_SSH_DIAGNOSTIC_BYTES);
    assert!(diagnostic.starts_with("scripted worker diagnostic: missing worker dependency"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn artifact_capable_worker_manifest_is_fetched_verified_published_and_cleaned() {
    let fixture = fixture();
    let factory = ScriptedFactory::new(vec![ScriptMode::Success, ScriptMode::ArtifactSuccess]);
    let backend = SshBackend::new(fixture.session.clone(), fixture.ssh_target.clone())
        .unwrap()
        .with_process_factory(factory.clone())
        .with_artifacts(ArtifactPolicy::new(vec!["result".to_string()]).unwrap(), ArtifactLimits::default());

    let (sync_tx, sync_rx) = tokio::sync::mpsc::channel(64);
    backend.execute(authorize_sync(&fixture), sync_tx).await.unwrap();
    let snapshot_id = collect(sync_rx)
        .await
        .into_iter()
        .find_map(|event| match event {
            RemoteBackendEvent::SyncCompleted { snapshot_id } => Some(snapshot_id),
            _ => None,
        })
        .unwrap();

    let (build_tx, build_rx) = tokio::sync::mpsc::channel(64);
    backend.execute(authorize_build(&fixture, snapshot_id), build_tx).await.unwrap();
    let events = collect(build_rx).await;
    assert!(events.iter().any(|event| matches!(event, RemoteBackendEvent::Completed { exit_code: 0 })));
    let published = fixture.session.workspace_root().join(".bunkerbox/artifacts/04040404040404040404040404040404/result");
    assert_eq!(fs::read(published).unwrap(), b"data");

    let messages = factory.messages.lock().unwrap();
    assert!(messages
        .iter()
        .any(|message| matches!(message, WorkerMessage::Build { build, .. } if build.artifact_paths().iter().any(|path| path.as_str() == "result"))));
    assert!(messages.iter().any(|message| matches!(message, WorkerMessage::FetchArtifact { entry_index: 0, .. })));
    assert!(messages.iter().any(|message| matches!(message, WorkerMessage::Cleanup { .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_artifact_capture_failure_is_typed_as_manifest_failure() {
    let fixture = fixture();
    let factory = ScriptedFactory::new(vec![ScriptMode::Success, ScriptMode::ArtifactManifestError]);
    let backend = SshBackend::new(fixture.session.clone(), fixture.ssh_target.clone())
        .unwrap()
        .with_process_factory(factory)
        .with_artifacts(ArtifactPolicy::new(vec!["result".to_string()]).unwrap(), ArtifactLimits::default());

    let (sync_tx, sync_rx) = tokio::sync::mpsc::channel(64);
    backend.execute(authorize_sync(&fixture), sync_tx).await.unwrap();
    let snapshot_id = collect(sync_rx)
        .await
        .into_iter()
        .find_map(|event| match event {
            RemoteBackendEvent::SyncCompleted { snapshot_id } => Some(snapshot_id),
            _ => None,
        })
        .unwrap();

    let (build_tx, _build_rx) = tokio::sync::mpsc::channel(64);
    let error = backend.execute(authorize_build(&fixture, snapshot_id), build_tx).await.unwrap_err();
    assert!(matches!(error, RemoteBackendError::Transport { class: RemoteFailureClass::ArtifactManifest, .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_artifact_fetch_failure_is_typed_as_transfer_failure() {
    let fixture = fixture();
    let factory = ScriptedFactory::new(vec![ScriptMode::Success, ScriptMode::ArtifactTransferError]);
    let backend = SshBackend::new(fixture.session.clone(), fixture.ssh_target.clone())
        .unwrap()
        .with_process_factory(factory)
        .with_artifacts(ArtifactPolicy::new(vec!["result".to_string()]).unwrap(), ArtifactLimits::default());

    let (sync_tx, sync_rx) = tokio::sync::mpsc::channel(64);
    backend.execute(authorize_sync(&fixture), sync_tx).await.unwrap();
    let snapshot_id = collect(sync_rx)
        .await
        .into_iter()
        .find_map(|event| match event {
            RemoteBackendEvent::SyncCompleted { snapshot_id } => Some(snapshot_id),
            _ => None,
        })
        .unwrap();

    let (build_tx, _build_rx) = tokio::sync::mpsc::channel(64);
    let error = backend.execute(authorize_build(&fixture, snapshot_id), build_tx).await.unwrap_err();
    assert!(matches!(error, RemoteBackendError::Transport { class: RemoteFailureClass::ArtifactTransfer, .. }));
}
