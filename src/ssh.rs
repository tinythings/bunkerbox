use crate::artifact::{ArtifactLimits, ArtifactManifest, ArtifactPolicy, ArtifactPublication};
use crate::loopback::{RunRemoteSession, SnapshotExportClaim};
use crate::remote::{
    AuthorizedRemoteRequest, RemoteBackend, RemoteBackendError, RemoteBackendEvent, RemoteFailureClass, RemoteFuture, RemoteOperation,
    RemoteSnapshotId,
};
use crate::remote_target::{ResourceLimits, SshTarget};
use crate::snapshot::SnapshotEntryKind;
use crate::worker_protocol::{
    self, WorkerArtifactEntry, WorkerArtifactPath, WorkerArtifactSetId, WorkerBuild, WorkerErrorKind, WorkerMessage, WorkerOperation,
    WorkerRelativePath, WorkerRequestId, WorkerSessionId, WorkerUploadEntry, WorkerUploadId, MAX_WORKER_CHUNK_BYTES, MAX_WORKER_FILE_BYTES,
    WORKER_ARTIFACT_PROTOCOL_VERSION, WORKER_PROTOCOL_VERSION,
};
use rand::RngCore;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const SSH_PROGRAM: &str = "/usr/bin/ssh";
const MAX_SSH_DIAGNOSTIC_BYTES: usize = 16 * 1024;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const PROCESS_REAP_TIMEOUT: Duration = Duration::from_secs(2);

type WorkerReader = Box<dyn AsyncRead + Send + Unpin>;
type WorkerWriter = Box<dyn AsyncWrite + Send + Unpin>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshLaunchSpec {
    program: PathBuf,
    args: Vec<String>,
    remote_command: String,
}

impl SshLaunchSpec {
    pub fn from_target(target: &SshTarget) -> Result<Self, String> {
        if target.host().is_empty() || target.user().is_empty() || target.port() == 0 {
            return Err("SSH target is not fully validated".to_string());
        }

        let remote_command = format!("exec {} --stdio --workspace-root {}", shell_quote(target.worker_path()), shell_quote(target.workspace_root()));
        let connect_timeout = target.resources().connect_timeout().as_secs().max(1).to_string();
        let args = vec![
            "-F".to_string(),
            "/dev/null".to_string(),
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "StrictHostKeyChecking=yes".to_string(),
            "-o".to_string(),
            format!("UserKnownHostsFile={}", target.known_hosts_file().display()),
            "-o".to_string(),
            "GlobalKnownHostsFile=/dev/null".to_string(),
            "-o".to_string(),
            "IdentitiesOnly=yes".to_string(),
            "-o".to_string(),
            "IdentityAgent=none".to_string(),
            "-o".to_string(),
            "ForwardAgent=no".to_string(),
            "-o".to_string(),
            "ClearAllForwardings=yes".to_string(),
            "-o".to_string(),
            "RequestTTY=no".to_string(),
            "-o".to_string(),
            "PasswordAuthentication=no".to_string(),
            "-o".to_string(),
            "KbdInteractiveAuthentication=no".to_string(),
            "-o".to_string(),
            "ControlMaster=no".to_string(),
            "-o".to_string(),
            "EscapeChar=none".to_string(),
            "-o".to_string(),
            format!("ConnectTimeout={connect_timeout}"),
            "-p".to_string(),
            target.port().to_string(),
            "-i".to_string(),
            target.identity_file().display().to_string(),
            "-l".to_string(),
            target.user().to_string(),
            "--".to_string(),
            target.host().to_string(),
            remote_command.clone(),
        ];

        Ok(Self { program: PathBuf::from(SSH_PROGRAM), args, remote_command })
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn remote_command(&self) -> &str {
        &self.remote_command
    }
}

pub trait SshProcess: Send {
    fn take_stdin(&mut self) -> Option<WorkerWriter>;
    fn take_stdout(&mut self) -> Option<WorkerReader>;
    fn take_stderr(&mut self) -> Option<WorkerReader>;
    fn terminate_group(&mut self);
    fn kill_group(&mut self);
    fn wait<'a>(&'a mut self) -> RemoteFuture<'a, Result<i32, String>>;
}

pub trait SshProcessFactory: Send + Sync {
    fn spawn(&self, spec: &SshLaunchSpec) -> Result<Box<dyn SshProcess>, String>;
}

#[derive(Debug, Default)]
pub struct SystemSshProcessFactory;

impl SshProcessFactory for SystemSshProcessFactory {
    fn spawn(&self, spec: &SshLaunchSpec) -> Result<Box<dyn SshProcess>, String> {
        let mut command = Command::new(spec.program());
        command
            .args(spec.args())
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command.spawn().map_err(|error| format!("spawn SSH transport: {error}"))?;
        let pid = child.id().map(|pid| pid as i32);
        let stdin = child.stdin.take().ok_or_else(|| "SSH transport has no stdin".to_string())?;
        let stdout = child.stdout.take().ok_or_else(|| "SSH transport has no stdout".to_string())?;
        let stderr = child.stderr.take().ok_or_else(|| "SSH transport has no stderr".to_string())?;
        Ok(Box::new(SystemSshProcess {
            child,
            pid,
            stdin: Some(Box::new(stdin)),
            stdout: Some(Box::new(stdout)),
            stderr: Some(Box::new(stderr)),
            active: true,
        }))
    }
}

struct SystemSshProcess {
    child: tokio::process::Child,
    pid: Option<i32>,
    stdin: Option<WorkerWriter>,
    stdout: Option<WorkerReader>,
    stderr: Option<WorkerReader>,
    active: bool,
}

impl SshProcess for SystemSshProcess {
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
        signal_group(self.pid, libc::SIGTERM);
    }

    fn kill_group(&mut self) {
        signal_group(self.pid, libc::SIGTERM);
        signal_group(self.pid, libc::SIGKILL);
    }

    fn wait<'a>(&'a mut self) -> RemoteFuture<'a, Result<i32, String>> {
        Box::pin(async move {
            let status = self.child.wait().await.map_err(|error| format!("wait for SSH transport: {error}"))?;
            self.active = false;
            Ok(status.code().unwrap_or(-1))
        })
    }
}

impl Drop for SystemSshProcess {
    fn drop(&mut self) {
        if self.active {
            self.kill_group();
        }
    }
}

pub struct SshBackend {
    session: Arc<RunRemoteSession>,
    target: SshTarget,
    factory: Arc<dyn SshProcessFactory>,
    uploads: Arc<Mutex<BTreeMap<RemoteSnapshotId, WorkerUploadId>>>,
    artifact_policy: ArtifactPolicy,
    artifact_limits: ArtifactLimits,
}

impl SshBackend {
    pub fn new(session: Arc<RunRemoteSession>, target: SshTarget) -> Result<Self, String> {
        let _ = SshLaunchSpec::from_target(&target)?;
        Ok(Self {
            artifact_limits: target.resources().artifact_limits(),
            session,
            target,
            factory: Arc::new(SystemSshProcessFactory),
            uploads: Arc::new(Mutex::new(BTreeMap::new())),
            artifact_policy: ArtifactPolicy::default(),
        })
    }

    pub fn with_process_factory(mut self, factory: Arc<dyn SshProcessFactory>) -> Self {
        self.factory = factory;
        self
    }

    pub fn with_artifacts(mut self, policy: ArtifactPolicy, limits: ArtifactLimits) -> Self {
        self.artifact_policy = policy;
        self.artifact_limits = limits;
        self
    }

    pub fn target(&self) -> &SshTarget {
        &self.target
    }

    pub fn pending_uploads(&self) -> usize {
        self.uploads.lock().map(|uploads| uploads.len()).unwrap_or(0)
    }
}

impl RemoteBackend for SshBackend {
    fn execute<'a>(
        &'a self, request: AuthorizedRemoteRequest, events: tokio::sync::mpsc::Sender<RemoteBackendEvent>,
    ) -> RemoteFuture<'a, Result<(), RemoteBackendError>> {
        let operation = request.request().operation().clone();
        let request_id = request.request_id();
        let session = self.session.clone();
        let target = self.target.clone();
        let factory = self.factory.clone();
        let uploads = self.uploads.clone();
        let artifact_policy = self.artifact_policy.clone();
        let artifact_limits = self.artifact_limits;
        Box::pin(async move {
            let backend = SshExecution { session, target, factory, uploads, artifact_policy, artifact_limits };
            match operation {
                RemoteOperation::Sync(sync) => backend.execute_sync(request_id.0, sync.retain_capability(), events).await,
                RemoteOperation::Build(build) => backend.execute_build(request_id.0, &build, events).await,
            }
        })
    }
}

struct SshExecution {
    session: Arc<RunRemoteSession>,
    target: SshTarget,
    factory: Arc<dyn SshProcessFactory>,
    uploads: Arc<Mutex<BTreeMap<RemoteSnapshotId, WorkerUploadId>>>,
    artifact_policy: ArtifactPolicy,
    artifact_limits: ArtifactLimits,
}

impl SshExecution {
    async fn execute_sync(
        &self, request_id: [u8; 16], retain_capability: bool, events: tokio::sync::mpsc::Sender<RemoteBackendEvent>,
    ) -> Result<(), RemoteBackendError> {
        send_event(&events, RemoteBackendEvent::SyncProgress { completed_bytes: 0, total_bytes: None }).await?;
        let snapshot_id = self.session.sync_snapshot().map_err(RemoteBackendError::Failed)?;
        let export = match self.session.claim_snapshot_for_export(snapshot_id) {
            Ok(export) => export,
            Err(error) => {
                let _ = self.session.abort_snapshot_capability(snapshot_id);
                return Err(RemoteBackendError::Failed(error));
            }
        };
        let upload_id = random_upload_id();
        let entries = match export.entries().iter().map(worker_entry).collect::<Result<Vec<_>, _>>() {
            Ok(entries) => entries,
            Err(error) => {
                drop(export);
                let _ = self.session.abort_snapshot_capability(snapshot_id);
                return Err(error);
            }
        };
        if let Err(error) = preflight_upload(request_id, session_id_for(&self.session), upload_id, &entries) {
            drop(export);
            let _ = self.session.abort_snapshot_capability(snapshot_id);
            return Err(error);
        }
        let mut connection = match WorkerConnection::spawn(&self.factory, &self.target) {
            Ok(connection) => connection,
            Err(error) => {
                drop(export);
                let _ = self.session.abort_snapshot_capability(snapshot_id);
                return Err(error);
            }
        };
        let session_id = session_id_for(&self.session);
        let operation = upload_and_finish(
            &mut connection,
            UploadPlan {
                request_id: WorkerRequestId(request_id),
                session_id,
                upload_id,
                entries: &entries,
                export: &export,
                events: &events,
                cleanup: !retain_capability,
            },
        );
        let result = match timeout(self.target.resources().sync_timeout(), operation).await {
            Ok(result) => result,
            Err(_) => {
                connection.kill_and_reap().await;
                Err(RemoteBackendError::Timeout)
            }
        };
        drop(export);

        if let Err(error) = result {
            connection.kill_and_reap().await;
            let _ = self.session.abort_snapshot_capability(snapshot_id);
            return Err(error);
        }

        if retain_capability {
            self.uploads
                .lock()
                .map_err(|_| RemoteBackendError::Failed("SSH upload registry lock poisoned".to_string()))?
                .insert(snapshot_id, upload_id);
            if let Err(error) = send_event(&events, RemoteBackendEvent::SyncCompleted { snapshot_id }).await {
                self.remove_upload(snapshot_id);
                let _ = self.session.abort_snapshot_capability(snapshot_id);
                return Err(error);
            }
        } else {
            self.session.abort_snapshot_capability(snapshot_id).map_err(RemoteBackendError::Failed)?;
            send_event(&events, RemoteBackendEvent::Completed { exit_code: 0 }).await?;
        }
        Ok(())
    }

    async fn execute_build(
        &self, request_id: [u8; 16], build: &crate::remote::RemoteBuild, events: tokio::sync::mpsc::Sender<RemoteBackendEvent>,
    ) -> Result<(), RemoteBackendError> {
        let snapshot_id = build.snapshot_id();
        let upload_id = self
            .uploads
            .lock()
            .map_err(|_| RemoteBackendError::Failed("SSH upload registry lock poisoned".to_string()))?
            .remove(&snapshot_id)
            .ok_or_else(|| RemoteBackendError::Transport {
                class: RemoteFailureClass::SnapshotTransfer,
                message: "remote snapshot upload is unavailable".to_string(),
            })?;
        let claim = self.session.claim_snapshot(snapshot_id).map_err(RemoteBackendError::Failed)?;
        let executable = match self.target.tools().get(build.tool().as_str()) {
            Some(executable) => executable.clone(),
            None => {
                drop(claim);
                return Err(RemoteBackendError::Transport {
                    class: RemoteFailureClass::WorkerUnavailable,
                    message: format!("remote tool is not configured: {}", build.tool().as_str()),
                });
            }
        };
        let guest_env = build.env().to_vec();
        let target_env = self.target.environment().iter().map(|(key, value)| (key.clone(), value.clone())).collect::<Vec<_>>();
        let worker_build =
            WorkerBuild::new(build.tool().as_str(), executable, build.argv().to_vec(), build.cwd().as_str(), guest_env, target_env, upload_id)
                .map_err(|error| RemoteBackendError::Transport { class: RemoteFailureClass::WorkerProtocol, message: error.to_string() })?;
        let worker_build = if self.artifact_policy.is_enabled() {
            let paths = self
                .artifact_policy
                .paths()
                .iter()
                .map(|path| WorkerArtifactPath::new(path.clone()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| RemoteBackendError::Transport { class: RemoteFailureClass::ArtifactManifest, message: error.to_string() })?;
            worker_build
                .with_artifacts(paths, self.artifact_limits.max_file_bytes, self.artifact_limits.max_total_bytes)
                .map_err(|error| RemoteBackendError::Transport { class: RemoteFailureClass::ArtifactManifest, message: error.to_string() })?
        } else {
            worker_build
        };

        let mut connection = WorkerConnection::spawn(&self.factory, &self.target)?;
        let session_id = WorkerSessionId(self.session.session_id().0);
        let operation = build_and_finish(
            &mut connection,
            BuildPlan {
                request_id: WorkerRequestId(request_id),
                session_id,
                build: worker_build,
                events: &events,
                upload_id,
                resources: self.target.resources(),
                artifact_policy: self.artifact_policy.clone(),
                artifact_limits: self.artifact_limits,
                workspace_root: self.session.workspace_root().to_path_buf(),
                protocol_version: if self.artifact_policy.is_enabled() { WORKER_ARTIFACT_PROTOCOL_VERSION } else { WORKER_PROTOCOL_VERSION },
            },
        );
        let result = match timeout(self.target.resources().build_timeout(), operation).await {
            Ok(result) => result,
            Err(_) => {
                connection.kill_and_reap().await;
                Err(RemoteBackendError::Timeout)
            }
        };
        if let Err(error) = result {
            let _ = timeout(CLEANUP_TIMEOUT, connection.cleanup(WorkerRequestId(request_id), session_id, upload_id)).await;
            connection.kill_and_reap().await;
            return Err(error);
        }
        drop(claim);
        Ok(())
    }

    fn remove_upload(&self, snapshot_id: RemoteSnapshotId) {
        if let Ok(mut uploads) = self.uploads.lock() {
            uploads.remove(&snapshot_id);
        }
    }
}

struct WorkerConnection {
    process: Box<dyn SshProcess>,
    writer: Option<WorkerWriter>,
    reader: WorkerReader,
    stderr_task: Option<JoinHandle<Vec<u8>>>,
    version: u16,
}

impl WorkerConnection {
    fn spawn(factory: &Arc<dyn SshProcessFactory>, target: &SshTarget) -> Result<Self, RemoteBackendError> {
        let spec = SshLaunchSpec::from_target(target)
            .map_err(|error| RemoteBackendError::Transport { class: RemoteFailureClass::WorkerUnavailable, message: error })?;
        let mut process = factory.spawn(&spec).map_err(classify_spawn_error)?;
        let writer = process.take_stdin().ok_or_else(|| unavailable("SSH transport has no stdin"))?;
        let reader = process.take_stdout().ok_or_else(|| unavailable("SSH transport has no stdout"))?;
        let stderr = process.take_stderr().ok_or_else(|| unavailable("SSH transport has no stderr"))?;
        let stderr_task = tokio::spawn(read_diagnostic(stderr));
        Ok(Self { process, writer: Some(writer), reader, stderr_task: Some(stderr_task), version: WORKER_PROTOCOL_VERSION })
    }

    async fn handshake(&mut self, request_id: WorkerRequestId, session_id: WorkerSessionId, version: u16) -> Result<(), RemoteBackendError> {
        self.version = version;
        self.write(&WorkerMessage::hello_for_version(request_id, session_id, false, version)).await?;
        let (received_version, message) = self.read_versioned().await?;
        if received_version != version {
            return Err(RemoteBackendError::Transport {
                class: RemoteFailureClass::WorkerVersion,
                message: format!("worker selected protocol version {received_version}, requested {version}"),
            });
        }
        match message {
            WorkerMessage::Hello { request_id: received_request, session_id: received_session, version, response } => {
                if received_request != request_id || received_session != session_id {
                    return Err(worker_protocol("worker hello correlation mismatch"));
                }
                if version != self.version {
                    return Err(RemoteBackendError::Transport {
                        class: RemoteFailureClass::WorkerVersion,
                        message: format!("unsupported worker version: {version}"),
                    });
                }
                if !response {
                    return Err(worker_protocol("worker hello was not a response"));
                }
                Ok(())
            }
            WorkerMessage::Error { kind, message, .. } => Err(worker_error(WorkerOperation::Protocol, kind, message)),
            _ => Err(worker_protocol("worker did not respond with Hello")),
        }
    }

    async fn write(&mut self, message: &WorkerMessage) -> Result<(), RemoteBackendError> {
        let writer = self.writer.as_mut().ok_or_else(|| disconnected("SSH worker stdin is closed"))?;
        worker_protocol::write_message_versioned(writer, message, self.version).await.map_err(worker_io_error)
    }

    async fn read(&mut self) -> Result<WorkerMessage, RemoteBackendError> {
        let (version, message) = self.read_versioned().await?;
        if version != self.version {
            return Err(RemoteBackendError::Transport {
                class: RemoteFailureClass::WorkerVersion,
                message: format!("worker frame version changed from {} to {version}", self.version),
            });
        }
        Ok(message)
    }

    async fn read_versioned(&mut self) -> Result<(u16, WorkerMessage), RemoteBackendError> {
        worker_protocol::read_message_versioned(&mut self.reader).await.map_err(worker_io_error)
    }

    async fn cleanup(
        &mut self, request_id: WorkerRequestId, session_id: WorkerSessionId, upload_id: WorkerUploadId,
    ) -> Result<(), RemoteBackendError> {
        self.write(&WorkerMessage::Cleanup { request_id, session_id, upload_token: upload_id }).await?;
        match self.read().await? {
            WorkerMessage::Completed {
                request_id: received_request,
                session_id: received_session,
                operation: WorkerOperation::Cleanup,
                exit_code,
            } => {
                if received_request != request_id || received_session != session_id {
                    return Err(worker_protocol("worker cleanup correlation mismatch"));
                }
                if exit_code != 0 {
                    return Err(RemoteBackendError::Transport {
                        class: RemoteFailureClass::Cleanup,
                        message: format!("worker cleanup exited with status {exit_code}"),
                    });
                }
                Ok(())
            }
            WorkerMessage::Error { kind, message, .. } => Err(worker_error(WorkerOperation::Cleanup, kind, message)),
            _ => Err(worker_protocol("unexpected worker cleanup response")),
        }
    }

    async fn finish(&mut self) -> Result<(), RemoteBackendError> {
        self.writer.take();
        let status = self.process.wait().await.map_err(disconnected)?;
        let diagnostic = self.stderr_task.take().map(|task| async move { task.await.unwrap_or_default() });
        let diagnostic = match diagnostic {
            Some(future) => future.await,
            None => Vec::new(),
        };
        if status != 0 {
            return Err(classify_exit(status, &diagnostic));
        }
        Ok(())
    }

    async fn kill_and_reap(&mut self) {
        self.process.kill_group();
        let _ = timeout(PROCESS_REAP_TIMEOUT, self.process.wait()).await;
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

impl Drop for WorkerConnection {
    fn drop(&mut self) {
        self.process.kill_group();
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

struct UploadPlan<'a> {
    request_id: WorkerRequestId,
    session_id: WorkerSessionId,
    upload_id: WorkerUploadId,
    entries: &'a [WorkerUploadEntry],
    export: &'a SnapshotExportClaim,
    events: &'a tokio::sync::mpsc::Sender<RemoteBackendEvent>,
    cleanup: bool,
}

struct BuildPlan<'a> {
    request_id: WorkerRequestId,
    session_id: WorkerSessionId,
    build: WorkerBuild,
    events: &'a tokio::sync::mpsc::Sender<RemoteBackendEvent>,
    upload_id: WorkerUploadId,
    resources: ResourceLimits,
    artifact_policy: ArtifactPolicy,
    artifact_limits: ArtifactLimits,
    workspace_root: PathBuf,
    protocol_version: u16,
}

async fn upload_and_finish(connection: &mut WorkerConnection, plan: UploadPlan<'_>) -> Result<(), RemoteBackendError> {
    let UploadPlan { request_id, session_id, upload_id, entries, export, events, cleanup } = plan;
    connection.handshake(request_id, session_id, WORKER_PROTOCOL_VERSION).await?;
    let total_bytes = export.total_file_bytes();
    connection.write(&WorkerMessage::UploadBegin { request_id, session_id, upload_id, entries: entries.to_vec() }).await?;
    let mut completed_bytes = 0u64;
    for entry in export.entries() {
        if entry.kind() != SnapshotEntryKind::RegularFile {
            continue;
        }
        let contents = export
            .read_file_bounded(entry, MAX_WORKER_FILE_BYTES)
            .map_err(|error| RemoteBackendError::Transport { class: RemoteFailureClass::SnapshotTransfer, message: error })?;
        for (index, chunk) in contents.chunks(MAX_WORKER_CHUNK_BYTES).enumerate() {
            let offset = u64::try_from(index).unwrap_or(u64::MAX).saturating_mul(MAX_WORKER_CHUNK_BYTES as u64);
            connection
                .write(&WorkerMessage::UploadFileChunk {
                    request_id,
                    session_id,
                    upload_id,
                    path: WorkerRelativePath::new(entry.path().as_str()).map_err(worker_io_error)?,
                    offset,
                    data: chunk.to_vec(),
                })
                .await?;
            completed_bytes = completed_bytes.saturating_add(chunk.len() as u64);
            send_event(events, RemoteBackendEvent::SyncProgress { completed_bytes, total_bytes: Some(total_bytes) }).await?;
        }
    }
    connection.write(&WorkerMessage::UploadComplete { request_id, session_id, upload_id }).await?;
    loop {
        match connection.read().await? {
            WorkerMessage::SyncProgress {
                request_id: received_request,
                session_id: received_session,
                upload_id: received_upload,
                completed_bytes,
                total_bytes,
            } => {
                if received_request != request_id || received_session != session_id || received_upload != upload_id {
                    return Err(worker_protocol("worker sync progress correlation mismatch"));
                }
                send_event(events, RemoteBackendEvent::SyncProgress { completed_bytes, total_bytes }).await?;
            }
            WorkerMessage::UploadComplete { request_id: received_request, session_id: received_session, upload_id: received_upload } => {
                if received_request != request_id || received_session != session_id || received_upload != upload_id {
                    return Err(worker_protocol("worker upload completion correlation mismatch"));
                }
                break;
            }
            WorkerMessage::Error { kind, message, .. } => return Err(worker_error(WorkerOperation::Upload, kind, message)),
            _ => return Err(worker_protocol("unexpected worker upload response")),
        }
    }
    if cleanup {
        connection.cleanup(request_id, session_id, upload_id).await?;
    }
    connection.finish().await
}

async fn build_and_finish(connection: &mut WorkerConnection, plan: BuildPlan<'_>) -> Result<(), RemoteBackendError> {
    let BuildPlan { request_id, session_id, build, events, upload_id, resources, artifact_policy, artifact_limits, workspace_root, protocol_version } =
        plan;
    connection.handshake(request_id, session_id, protocol_version).await?;
    connection.write(&WorkerMessage::build(request_id, session_id, build)).await?;
    let mut output_bytes = 0u64;
    let exit_code = loop {
        match connection.read().await? {
            WorkerMessage::Stdout { request_id: received_request, session_id: received_session, data } => {
                check_correlation(received_request, received_session, request_id, session_id, "worker stdout")?;
                output_bytes = output_bytes.saturating_add(data.len() as u64);
                if output_bytes > resources.max_output_bytes() {
                    return Err(RemoteBackendError::OutputLimit { limit: resources.max_output_bytes() });
                }
                send_event(events, RemoteBackendEvent::Stdout(data)).await?;
            }
            WorkerMessage::Stderr { request_id: received_request, session_id: received_session, data } => {
                check_correlation(received_request, received_session, request_id, session_id, "worker stderr")?;
                output_bytes = output_bytes.saturating_add(data.len() as u64);
                if output_bytes > resources.max_output_bytes() {
                    return Err(RemoteBackendError::OutputLimit { limit: resources.max_output_bytes() });
                }
                send_event(events, RemoteBackendEvent::Stderr(data)).await?;
            }
            WorkerMessage::Completed { request_id: received_request, session_id: received_session, operation: WorkerOperation::Build, exit_code } => {
                check_correlation(received_request, received_session, request_id, session_id, "worker completion")?;
                break exit_code;
            }
            WorkerMessage::Error { kind, message, .. } => return Err(worker_error(WorkerOperation::Build, kind, message)),
            _ => return Err(worker_protocol("unexpected worker build response")),
        }
    };
    if exit_code == 0 && artifact_policy.is_enabled() {
        let (artifact_set_id, manifest) = match connection.read().await? {
            WorkerMessage::ArtifactManifest { request_id: received_request, session_id: received_session, artifact_set_id, entries, total_bytes } => {
                check_correlation(received_request, received_session, request_id, session_id, "worker artifact manifest")?;
                (artifact_set_id, artifact_manifest_from_worker(entries, total_bytes, &artifact_policy, artifact_limits)?)
            }
            WorkerMessage::Error { kind, message, .. } => return Err(worker_artifact_manifest_error(kind, message)),
            _ => return Err(worker_protocol("unexpected worker artifact manifest response")),
        };
        let retrieval = timeout(
            artifact_limits.timeout,
            fetch_and_publish_artifacts(connection, request_id, session_id, artifact_set_id, &manifest, workspace_root),
        )
        .await;
        match retrieval {
            Ok(result) => result?,
            Err(_) => {
                return Err(RemoteBackendError::Transport {
                    class: RemoteFailureClass::ArtifactTransfer,
                    message: "artifact retrieval timed out".to_string(),
                })
            }
        }
    }
    connection.cleanup(request_id, session_id, upload_id).await?;
    connection.finish().await?;
    send_event(events, RemoteBackendEvent::Completed { exit_code }).await
}

async fn fetch_and_publish_artifacts(
    connection: &mut WorkerConnection, request_id: WorkerRequestId, session_id: WorkerSessionId, artifact_set_id: WorkerArtifactSetId,
    manifest: &ArtifactManifest, workspace_root: PathBuf,
) -> Result<(), RemoteBackendError> {
    let mut publication = ArtifactPublication::new(&workspace_root, request_id.0, manifest.clone())
        .map_err(|error| RemoteBackendError::Transport { class: RemoteFailureClass::ArtifactTransfer, message: error })?;
    for index in 0..manifest.entries().len() {
        connection
            .write(&WorkerMessage::FetchArtifact {
                request_id,
                session_id,
                artifact_set_id,
                entry_index: u32::try_from(index).map_err(|_| RemoteBackendError::Transport {
                    class: RemoteFailureClass::ArtifactTransfer,
                    message: "artifact index does not fit worker protocol".to_string(),
                })?,
            })
            .await
            .map_err(artifact_transfer_error)?;
        let mut writer = publication
            .begin(index)
            .map_err(|error| RemoteBackendError::Transport { class: RemoteFailureClass::ArtifactTransfer, message: error })?;
        loop {
            match connection.read().await.map_err(artifact_transfer_error)? {
                WorkerMessage::ArtifactChunk {
                    request_id: received_request,
                    session_id: received_session,
                    artifact_set_id: received_set,
                    entry_index,
                    offset,
                    data,
                } => {
                    check_correlation(received_request, received_session, request_id, session_id, "worker artifact chunk")
                        .map_err(artifact_transfer_error)?;
                    if received_set != artifact_set_id || entry_index != index as u32 {
                        return Err(RemoteBackendError::Transport {
                            class: RemoteFailureClass::ArtifactTransfer,
                            message: "worker artifact chunk identity mismatch".to_string(),
                        });
                    }
                    let expected = writer
                        .expected_offset()
                        .map_err(|error| RemoteBackendError::Transport { class: RemoteFailureClass::ArtifactTransfer, message: error })?;
                    if offset != expected {
                        return Err(RemoteBackendError::Transport {
                            class: RemoteFailureClass::ArtifactTransfer,
                            message: "worker artifact chunk offset is out of order".to_string(),
                        });
                    }
                    writer
                        .write_chunk(&data)
                        .map_err(|error| RemoteBackendError::Transport { class: RemoteFailureClass::ArtifactTransfer, message: error })?;
                }
                WorkerMessage::ArtifactComplete {
                    request_id: received_request,
                    session_id: received_session,
                    artifact_set_id: received_set,
                    entry_index,
                } => {
                    check_correlation(received_request, received_session, request_id, session_id, "worker artifact completion")
                        .map_err(artifact_transfer_error)?;
                    if received_set != artifact_set_id || entry_index != index as u32 {
                        return Err(RemoteBackendError::Transport {
                            class: RemoteFailureClass::ArtifactTransfer,
                            message: "worker artifact completion identity mismatch".to_string(),
                        });
                    }
                    publication
                        .complete(writer)
                        .map_err(|error| RemoteBackendError::Transport { class: RemoteFailureClass::ArtifactTransfer, message: error })?;
                    break;
                }
                WorkerMessage::Error { kind, message, .. } => {
                    return Err(artifact_transfer_error(worker_error(WorkerOperation::Artifact, kind, message)));
                }
                _ => return Err(artifact_transfer_error(worker_protocol("unexpected worker artifact transfer response"))),
            }
        }
    }
    publication.publish().map_err(|error| RemoteBackendError::Transport { class: RemoteFailureClass::ArtifactTransfer, message: error })
}

fn artifact_manifest_from_worker(
    entries: Vec<WorkerArtifactEntry>, total_bytes: u64, policy: &ArtifactPolicy, limits: ArtifactLimits,
) -> Result<ArtifactManifest, RemoteBackendError> {
    let entries = entries
        .into_iter()
        .map(|entry| crate::artifact::ArtifactEntry::new(entry.path().as_str().to_string(), entry.mode(), entry.size(), *entry.digest()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| RemoteBackendError::Transport { class: RemoteFailureClass::ArtifactManifest, message: error })?;
    ArtifactManifest::new(entries, total_bytes, policy, limits)
        .map_err(|error| RemoteBackendError::Transport { class: RemoteFailureClass::ArtifactManifest, message: error })
}

fn worker_entry(entry: &crate::snapshot::SnapshotEntry) -> Result<WorkerUploadEntry, RemoteBackendError> {
    match entry.kind() {
        SnapshotEntryKind::Directory => WorkerUploadEntry::directory(entry.path().as_str(), entry.mode() as u32).map_err(worker_io_error),
        SnapshotEntryKind::RegularFile => WorkerUploadEntry::file(
            entry.path().as_str(),
            entry.mode() as u32,
            entry.size(),
            *entry.content_digest().ok_or_else(|| worker_protocol("regular snapshot entry has no digest"))?,
        )
        .map_err(worker_io_error),
    }
}

fn preflight_upload(
    request_id: [u8; 16], session_id: WorkerSessionId, upload_id: WorkerUploadId, entries: &[WorkerUploadEntry],
) -> Result<(), RemoteBackendError> {
    worker_protocol::validate_upload_manifest(entries).map_err(|error| upload_preflight_error(error.to_string()))?;
    WorkerMessage::UploadBegin { request_id: WorkerRequestId(request_id), session_id, upload_id, entries: entries.to_vec() }
        .encode()
        .map_err(|error| upload_preflight_error(error.to_string()))?;
    Ok(())
}

fn upload_preflight_error(message: String) -> RemoteBackendError {
    RemoteBackendError::Transport {
        class: RemoteFailureClass::SnapshotTransfer,
        message: format!("snapshot cannot fit worker V1 upload protocol: {message}"),
    }
}

fn session_id_for(session: &RunRemoteSession) -> WorkerSessionId {
    WorkerSessionId(session.session_id().0)
}

fn check_correlation(
    received_request: WorkerRequestId, received_session: WorkerSessionId, request_id: WorkerRequestId, session_id: WorkerSessionId, label: &str,
) -> Result<(), RemoteBackendError> {
    if received_request != request_id || received_session != session_id {
        return Err(worker_protocol(format!("{label} correlation mismatch")));
    }
    Ok(())
}

fn send_event<'a>(
    events: &'a tokio::sync::mpsc::Sender<RemoteBackendEvent>, event: RemoteBackendEvent,
) -> RemoteFuture<'a, Result<(), RemoteBackendError>> {
    Box::pin(async move { events.send(event).await.map_err(|_| RemoteBackendError::Cancelled) })
}

fn classify_spawn_error(message: String) -> RemoteBackendError {
    RemoteBackendError::Transport { class: RemoteFailureClass::Connect, message }
}

fn classify_exit(status: i32, diagnostic: &[u8]) -> RemoteBackendError {
    let text = String::from_utf8_lossy(diagnostic).to_ascii_lowercase();
    let class = if text.contains("permission denied") || text.contains("authentication failed") {
        RemoteFailureClass::Authentication
    } else if text.contains("host key") || text.contains("offending") || text.contains("known_hosts") {
        RemoteFailureClass::HostIdentity
    } else if text.contains("could not resolve") || text.contains("name or service not known") {
        RemoteFailureClass::Dns
    } else if text.contains("connection refused") || text.contains("connection timed out") {
        RemoteFailureClass::Connect
    } else if text.contains("worker") || text.contains("no such file") {
        RemoteFailureClass::WorkerUnavailable
    } else {
        RemoteFailureClass::Disconnect
    };
    RemoteBackendError::Transport { class, message: format!("SSH transport exited with status {status}") }
}

fn worker_io_error(error: worker_protocol::WorkerProtocolError) -> RemoteBackendError {
    if error.is_invalid() {
        let class = if error.contains("version") { RemoteFailureClass::WorkerVersion } else { RemoteFailureClass::WorkerProtocol };
        RemoteBackendError::Transport { class, message: error.to_string() }
    } else {
        disconnected(error.to_string())
    }
}

fn worker_error(operation: WorkerOperation, kind: WorkerErrorKind, message: String) -> RemoteBackendError {
    let class = match kind {
        WorkerErrorKind::WorkerProtocol => RemoteFailureClass::WorkerProtocol,
        WorkerErrorKind::Upload | WorkerErrorKind::Sync => RemoteFailureClass::SnapshotTransfer,
        WorkerErrorKind::Artifact => RemoteFailureClass::ArtifactTransfer,
        WorkerErrorKind::Cleanup => RemoteFailureClass::Cleanup,
        WorkerErrorKind::Build => return RemoteBackendError::Failed(format!("remote worker build failure ({operation:?}): {message}")),
    };
    RemoteBackendError::Transport { class, message }
}

fn worker_artifact_manifest_error(kind: WorkerErrorKind, message: String) -> RemoteBackendError {
    if matches!(kind, WorkerErrorKind::Artifact) {
        RemoteBackendError::Transport { class: RemoteFailureClass::ArtifactManifest, message }
    } else {
        worker_error(WorkerOperation::Artifact, kind, message)
    }
}

fn artifact_transfer_error(error: RemoteBackendError) -> RemoteBackendError {
    match error {
        RemoteBackendError::Transport { message, .. } => RemoteBackendError::Transport { class: RemoteFailureClass::ArtifactTransfer, message },
        other => other,
    }
}

fn worker_protocol(message: impl Into<String>) -> RemoteBackendError {
    RemoteBackendError::Transport { class: RemoteFailureClass::WorkerProtocol, message: message.into() }
}

fn unavailable(message: impl Into<String>) -> RemoteBackendError {
    RemoteBackendError::Transport { class: RemoteFailureClass::WorkerUnavailable, message: message.into() }
}

fn disconnected(message: impl Into<String>) -> RemoteBackendError {
    RemoteBackendError::Transport { class: RemoteFailureClass::Disconnect, message: message.into() }
}

async fn read_diagnostic(mut reader: WorkerReader) -> Vec<u8> {
    let mut result = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                if result.len() < MAX_SSH_DIAGNOSTIC_BYTES {
                    let remaining = MAX_SSH_DIAGNOSTIC_BYTES - result.len();
                    result.extend_from_slice(&buffer[..count.min(remaining)]);
                }
            }
        }
    }
    result
}

fn random_upload_id() -> WorkerUploadId {
    loop {
        let mut bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut bytes);
        if bytes != [0; 16] {
            return WorkerUploadId(bytes);
        }
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn signal_group(pid: Option<i32>, signal: libc::c_int) {
    if let Some(pid) = pid.filter(|pid| *pid > 0) {
        unsafe {
            libc::kill(-pid, signal);
        }
    }
}

#[cfg(test)]
#[path = "ssh_ut.rs"]
mod tests;
