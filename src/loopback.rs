use crate::remote::{
    AuthorizedRemoteRequest, RemoteBackend, RemoteBackendError, RemoteBackendEvent, RemoteFuture, RemoteOperation, RemoteTargetId, WorkspaceSessionId,
};
use crate::snapshot::{SnapshotBuilder, SnapshotHandle, SnapshotStore};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::sleep;

pub const LOOPBACK_BUILD_TIMEOUT: Duration = Duration::from_secs(30);
const LOOPBACK_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

pub struct RunRemoteSession {
    session_id: WorkspaceSessionId,
    target: RemoteTargetId,
    workspace_root: PathBuf,
    snapshot_store: SnapshotStore,
    snapshot_builder: SnapshotBuilder,
    current_snapshot: Mutex<Option<SnapshotHandle>>,
    snapshot_operation: Mutex<()>,
    jobs_root: PathBuf,
}

impl RunRemoteSession {
    pub fn new(
        session_id: WorkspaceSessionId, target: RemoteTargetId, workspace_root: PathBuf, snapshot_store: SnapshotStore,
        snapshot_builder: SnapshotBuilder, jobs_root: PathBuf,
    ) -> Result<Self, String> {
        if session_id.0 == [0; 16] {
            return Err("remote session ID must be nonzero".to_string());
        }
        if target.0 == [0; 16] {
            return Err("remote target ID must be nonzero".to_string());
        }
        let snapshot_root = snapshot_store.root_for_cleanup();
        create_private_root(&snapshot_root, "snapshot store")?;
        if let Err(error) = create_private_root(&jobs_root, "loopback jobs") {
            let _ = fs::remove_dir_all(&snapshot_root);
            return Err(error);
        }
        Ok(Self {
            session_id,
            target,
            workspace_root,
            snapshot_store,
            snapshot_builder,
            current_snapshot: Mutex::new(None),
            snapshot_operation: Mutex::new(()),
            jobs_root,
        })
    }

    pub fn session_id(&self) -> WorkspaceSessionId {
        self.session_id
    }

    pub fn target(&self) -> RemoteTargetId {
        self.target
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn snapshot_store(&self) -> SnapshotStore {
        self.snapshot_store.clone()
    }

    pub fn current_snapshot(&self) -> Result<Option<SnapshotHandle>, String> {
        self.current_snapshot.lock().map(|current| current.clone()).map_err(|_| "remote session state lock poisoned".to_string())
    }

    pub fn sync_snapshot(&self) -> Result<SnapshotHandle, String> {
        let _operation = self.snapshot_operation.lock().map_err(|_| "remote session operation lock poisoned".to_string())?;
        let snapshot = self.snapshot_builder.build_root(&self.workspace_root, self.session_id)?;
        let new_handle = snapshot.handle().clone();
        let old_handle = self.current_snapshot()?.clone();
        if let Some(old_handle) = old_handle.filter(|old| old != &new_handle) {
            self.snapshot_store.remove(&old_handle)?;
        }
        self.current_snapshot.lock().map_err(|_| "remote session state lock poisoned".to_string())?.replace(new_handle.clone());
        Ok(new_handle)
    }

    fn materialize_current_snapshot(&self, destination: &Path) -> Result<(), String> {
        let _operation = self.snapshot_operation.lock().map_err(|_| "remote session operation lock poisoned".to_string())?;
        let handle = self.current_snapshot()?.ok_or_else(|| "remote build requires a successful sync".to_string())?;
        self.snapshot_store.materialize(&handle, destination).map(|_| ())
    }

    fn new_job_path(&self) -> Result<PathBuf, String> {
        let path = self.jobs_root.join(format!("job-{}", NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed)));
        if path.exists() {
            return Err("loopback job path already exists".to_string());
        }
        Ok(path)
    }
}

impl Drop for RunRemoteSession {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.jobs_root);
        let _ = fs::remove_dir_all(self.snapshot_store_root());
    }
}

impl RunRemoteSession {
    fn snapshot_store_root(&self) -> PathBuf {
        // SnapshotStore deliberately exposes no public root path; this private
        // cleanup path is kept alongside the run-owned workspace state.
        self.snapshot_store.root_for_cleanup()
    }
}

pub struct LoopbackBackend {
    session: Arc<RunRemoteSession>,
    tools: Arc<BTreeMap<String, PathBuf>>,
    timeout: Duration,
}

impl LoopbackBackend {
    pub fn new(session: Arc<RunRemoteSession>, tools: BTreeMap<String, PathBuf>) -> Self {
        Self { session, tools: Arc::new(tools), timeout: LOOPBACK_BUILD_TIMEOUT }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl RemoteBackend for LoopbackBackend {
    fn execute<'a>(
        &'a self, request: AuthorizedRemoteRequest, events: mpsc::Sender<RemoteBackendEvent>,
    ) -> RemoteFuture<'a, Result<(), RemoteBackendError>> {
        let session = self.session.clone();
        let tools = self.tools.clone();
        let timeout = self.timeout;
        Box::pin(async move {
            match request.request().operation() {
                RemoteOperation::Sync => execute_sync(session, events).await,
                RemoteOperation::Build(build) => execute_build(session, tools, timeout, build, events).await,
            }
        })
    }
}

async fn execute_sync(session: Arc<RunRemoteSession>, events: mpsc::Sender<RemoteBackendEvent>) -> Result<(), RemoteBackendError> {
    send_event(&events, RemoteBackendEvent::SyncProgress { completed_bytes: 0, total_bytes: None }).await?;
    let sync = tokio::task::spawn_blocking(move || session.sync_snapshot())
        .await
        .map_err(|error| RemoteBackendError::Failed(format!("snapshot worker failed: {error}")))?;
    sync.map_err(RemoteBackendError::Failed)?;
    send_event(&events, RemoteBackendEvent::Completed { exit_code: 0 }).await
}

async fn execute_build(
    session: Arc<RunRemoteSession>, tools: Arc<BTreeMap<String, PathBuf>>, timeout_duration: Duration, build: &crate::remote::RemoteBuild,
    events: mpsc::Sender<RemoteBackendEvent>,
) -> Result<(), RemoteBackendError> {
    if !build.env().is_empty() {
        return Err(RemoteBackendError::Failed("remote environment is unavailable until remote environment policy is configured".to_string()));
    }
    let executable = tools
        .get(build.tool().as_str())
        .ok_or_else(|| RemoteBackendError::Spawn(format!("loopback tool is not configured: {}", build.tool().as_str())))?;
    let job_path = session.new_job_path().map_err(RemoteBackendError::Failed)?;
    let _job = JobGuard { path: job_path.clone() };
    let destination = job_path.clone();
    tokio::task::spawn_blocking({
        let session = session.clone();
        move || session.materialize_current_snapshot(&destination)
    })
    .await
    .map_err(|error| RemoteBackendError::Failed(format!("materialization worker failed: {error}")))?
    .map_err(RemoteBackendError::Failed)?;

    let cwd = job_path.join(build.cwd().as_str());
    let cwd_metadata = fs::symlink_metadata(&cwd).map_err(|error| RemoteBackendError::Failed(format!("remote cwd is unavailable: {error}")))?;
    if !cwd_metadata.file_type().is_dir() {
        return Err(RemoteBackendError::Failed("remote cwd is not a directory".to_string()));
    }

    let mut command = Command::new(executable);
    command
        .args(build.argv())
        .current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command.env_clear().env("PATH", LOOPBACK_PATH);
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| RemoteBackendError::Spawn(format!("spawn loopback tool: {error}")))?;
    let process_group = child.id().map(|pid| ProcessGroupGuard { pgid: pid as i32, active: true });
    let stdout = child.stdout.take().ok_or_else(|| RemoteBackendError::Failed("loopback child has no stdout".to_string()))?;
    let stderr = child.stderr.take().ok_or_else(|| RemoteBackendError::Failed("loopback child has no stderr".to_string()))?;
    let mut stdout_task = Box::pin(tokio::spawn(pump(stdout, RemoteStream::Stdout, events.clone())));
    let mut stderr_task = Box::pin(tokio::spawn(pump(stderr, RemoteStream::Stderr, events.clone())));
    let mut child_wait = Box::pin(child.wait());
    let mut timeout_sleep = Box::pin(sleep(timeout_duration));
    let mut child_status = None;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut failure = None;

    while child_status.is_none() || !stdout_done || !stderr_done {
        tokio::select! {
            status = &mut child_wait, if child_status.is_none() => {
                child_status = Some(status.map_err(|error| RemoteBackendError::Failed(format!("wait for loopback tool: {error}"))));
            }
            result = &mut stdout_task, if !stdout_done => {
                stdout_done = true;
                if let Err(error) = join_pump(result).await { failure.get_or_insert(error); kill_process_group(process_group.as_ref()); }
            }
            result = &mut stderr_task, if !stderr_done => {
                stderr_done = true;
                if let Err(error) = join_pump(result).await { failure.get_or_insert(error); kill_process_group(process_group.as_ref()); }
            }
            _ = &mut timeout_sleep, if child_status.is_none() => {
                failure.get_or_insert(RemoteBackendError::Timeout);
                kill_process_group(process_group.as_ref());
            }
        }
    }

    if failure.is_none() {
        kill_process_group(process_group.as_ref());
    }
    if let Some(mut process_group) = process_group {
        process_group.active = false;
    }
    if let Some(error) = failure {
        return Err(error);
    }
    let status = child_status.unwrap()?;
    send_event(&events, RemoteBackendEvent::Completed { exit_code: status.code().unwrap_or(-1) }).await
}

#[derive(Clone, Copy)]
enum RemoteStream {
    Stdout,
    Stderr,
}

async fn pump<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R, stream: RemoteStream, events: mpsc::Sender<RemoteBackendEvent>,
) -> Result<(), RemoteBackendError> {
    let mut buffer = [0u8; 8192];
    loop {
        let count = reader.read(&mut buffer).await.map_err(|error| RemoteBackendError::Failed(format!("read loopback output: {error}")))?;
        if count == 0 {
            return Ok(());
        }
        let event = match stream {
            RemoteStream::Stdout => RemoteBackendEvent::Stdout(buffer[..count].to_vec()),
            RemoteStream::Stderr => RemoteBackendEvent::Stderr(buffer[..count].to_vec()),
        };
        events.send(event).await.map_err(|_| RemoteBackendError::Cancelled)?;
    }
}

async fn join_pump(result: Result<Result<(), RemoteBackendError>, tokio::task::JoinError>) -> Result<(), RemoteBackendError> {
    result.map_err(|error| RemoteBackendError::Failed(format!("loopback output task failed: {error}")))?
}

async fn send_event(events: &mpsc::Sender<RemoteBackendEvent>, event: RemoteBackendEvent) -> Result<(), RemoteBackendError> {
    events.send(event).await.map_err(|_| RemoteBackendError::Cancelled)
}

struct JobGuard {
    path: PathBuf,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct ProcessGroupGuard {
    pgid: i32,
    active: bool,
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if self.active {
            kill_process_group(Some(self));
        }
    }
}

fn kill_process_group(group: Option<&ProcessGroupGuard>) {
    if let Some(group) = group {
        unsafe {
            libc::kill(-group.pgid, libc::SIGTERM);
            libc::kill(-group.pgid, libc::SIGKILL);
        }
    }
}

pub fn resolve_fixed_tools(names: impl IntoIterator<Item = String>) -> BTreeMap<String, PathBuf> {
    let mut tools = BTreeMap::new();
    for name in names {
        if name.is_empty() || name.contains('/') || name.as_bytes().contains(&0) {
            continue;
        }
        for directory in ["/usr/local/bin", "/usr/bin", "/bin"] {
            let path = Path::new(directory).join(&name);
            if path.is_file() {
                if let Ok(metadata) = fs::metadata(&path) {
                    use std::os::unix::fs::PermissionsExt;
                    if metadata.permissions().mode() & 0o111 != 0 {
                        tools.insert(name.clone(), path);
                        break;
                    }
                }
            }
        }
    }
    tools
}

pub fn cleanup_stale_roots(parent: &Path) -> Result<(), String> {
    let current_uid = unsafe { libc::geteuid() };
    for entry in fs::read_dir(parent).map_err(|error| format!("read temporary runtime roots: {error}"))? {
        let entry = entry.map_err(|error| format!("read temporary runtime root: {error}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("bunkerbox-loopback-") && !name.starts_with("bunkerbox-snapshots-") {
            continue;
        }
        let Some(pid) = name.split('-').nth(2).and_then(|value| value.parse::<libc::pid_t>().ok()) else {
            continue;
        };
        if process_is_alive(pid) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| format!("inspect stale runtime root: {error}"))?;
        if metadata.file_type().is_dir() && metadata.uid() == current_uid {
            fs::remove_dir_all(entry.path()).map_err(|error| format!("remove stale runtime root: {error}"))?;
        }
    }
    Ok(())
}

fn process_is_alive(pid: libc::pid_t) -> bool {
    if pid <= 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn create_private_root(path: &Path, label: &str) -> Result<(), String> {
    fs::create_dir(path).map_err(|error| format!("create {label}: {error}"))?;
    if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o700)) {
        let _ = fs::remove_dir(path);
        return Err(format!("set private {label} mode: {error}"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "loopback_ut.rs"]
mod tests;
