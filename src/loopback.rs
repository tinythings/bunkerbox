use crate::remote::{
    AuthorizedRemoteRequest, RemoteBackend, RemoteBackendError, RemoteBackendEvent, RemoteFuture, RemoteOperation, RemoteResourcePolicy,
    RemoteSnapshotAuthority, RemoteSnapshotId, RemoteTargetId, WorkspaceSessionId,
};
use crate::snapshot::{SnapshotBuilder, SnapshotHandle, SnapshotStore};
use rand::RngCore;
use std::collections::{BTreeMap, HashMap};
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

pub const LOOPBACK_BUILD_TIMEOUT: Duration = crate::remote::DEFAULT_REMOTE_BUILD_TIMEOUT;
const LOOPBACK_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
const POST_CHILD_EXIT_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
const POST_CHILD_EXIT_FINAL_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

pub struct RunRemoteSession {
    session_id: WorkspaceSessionId,
    target: RemoteTargetId,
    workspace_root: PathBuf,
    snapshot_store: SnapshotStore,
    snapshot_builder: SnapshotBuilder,
    snapshot_capabilities: Mutex<SnapshotCapabilityRegistry>,
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
            snapshot_capabilities: Mutex::new(SnapshotCapabilityRegistry::default()),
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

    pub fn sync_snapshot(&self) -> Result<RemoteSnapshotId, String> {
        let _operation = self.snapshot_operation.lock().map_err(|_| "remote session operation lock poisoned".to_string())?;
        let snapshot = self.snapshot_builder.build_root(&self.workspace_root, self.session_id)?;
        self.register_snapshot(snapshot.handle().clone())
    }

    fn register_snapshot(&self, handle: SnapshotHandle) -> Result<RemoteSnapshotId, String> {
        let mut registry = self.snapshot_capabilities.lock().map_err(|_| "remote snapshot registry lock poisoned".to_string())?;
        let snapshot_id = loop {
            let mut bytes = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut bytes);
            let candidate = RemoteSnapshotId::from_bytes(bytes);
            if !candidate.is_zero() && !registry.capabilities.contains_key(&candidate) {
                break candidate;
            }
        };
        registry.capabilities.insert(snapshot_id, handle.clone());
        *registry.references.entry(handle).or_insert(0) += 1;
        Ok(snapshot_id)
    }

    fn claim_snapshot(self: &Arc<Self>, snapshot_id: RemoteSnapshotId) -> Result<SnapshotClaim, String> {
        let mut registry = self.snapshot_capabilities.lock().map_err(|_| "remote snapshot registry lock poisoned".to_string())?;
        let handle = registry.capabilities.remove(&snapshot_id).ok_or_else(|| "remote snapshot capability is unavailable".to_string())?;
        Ok(SnapshotClaim { session: self.clone(), handle })
    }

    fn release_snapshot(&self, handle: &SnapshotHandle) {
        let should_remove = self
            .snapshot_capabilities
            .lock()
            .ok()
            .map(|mut registry| {
                let Some(references) = registry.references.get_mut(handle) else {
                    return false;
                };
                *references = references.saturating_sub(1);
                if *references == 0 {
                    registry.references.remove(handle);
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if should_remove {
            let _ = self.snapshot_store.remove(handle);
        }
    }

    fn materialize_snapshot(&self, handle: &SnapshotHandle, destination: &Path) -> Result<(), String> {
        let _operation = self.snapshot_operation.lock().map_err(|_| "remote session operation lock poisoned".to_string())?;
        self.snapshot_store.materialize(handle, destination).map(|_| ())
    }

    fn new_job_path(&self) -> Result<PathBuf, String> {
        let path = self.jobs_root.join(format!("job-{}", NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed)));
        if path.exists() {
            return Err("loopback job path already exists".to_string());
        }
        Ok(path)
    }
}

#[derive(Default)]
struct SnapshotCapabilityRegistry {
    capabilities: BTreeMap<RemoteSnapshotId, SnapshotHandle>,
    references: HashMap<SnapshotHandle, usize>,
}

struct SnapshotClaim {
    session: Arc<RunRemoteSession>,
    handle: SnapshotHandle,
}

impl SnapshotClaim {
    fn handle(&self) -> &SnapshotHandle {
        &self.handle
    }

    fn clone_for_worker(&self) -> Result<Self, String> {
        let mut registry = self.session.snapshot_capabilities.lock().map_err(|_| "remote snapshot registry lock poisoned".to_string())?;
        let references =
            registry.references.get_mut(&self.handle).ok_or_else(|| "remote snapshot capability reference is unavailable".to_string())?;
        *references = references.checked_add(1).ok_or_else(|| "remote snapshot capability reference count overflow".to_string())?;
        Ok(Self { session: self.session.clone(), handle: self.handle.clone() })
    }
}

impl Drop for SnapshotClaim {
    fn drop(&mut self) {
        self.session.release_snapshot(&self.handle);
    }
}

impl RemoteSnapshotAuthority for RunRemoteSession {
    fn snapshot_available(&self, session: WorkspaceSessionId, snapshot_id: RemoteSnapshotId) -> bool {
        if session != self.session_id {
            return false;
        }
        self.snapshot_capabilities.lock().map(|registry| registry.capabilities.contains_key(&snapshot_id)).unwrap_or(false)
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
    target_environment: Arc<BTreeMap<String, String>>,
    resources: RemoteResourcePolicy,
}

impl LoopbackBackend {
    pub fn new(session: Arc<RunRemoteSession>, tools: BTreeMap<String, PathBuf>) -> Self {
        Self {
            session,
            tools: Arc::new(tools),
            target_environment: Arc::new(trusted_target_environment()),
            resources: RemoteResourcePolicy::default(),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.resources.build_timeout = timeout;
        self
    }

    pub fn with_output_limit(mut self, max_output_bytes: u64) -> Self {
        self.resources.max_output_bytes = max_output_bytes;
        self
    }

    pub fn with_target_environment(mut self, environment: BTreeMap<String, String>) -> Self {
        let mut trusted = trusted_target_environment();
        trusted.extend(environment);
        self.target_environment = Arc::new(trusted);
        self
    }
}

impl RemoteBackend for LoopbackBackend {
    fn execute<'a>(
        &'a self, request: AuthorizedRemoteRequest, events: mpsc::Sender<RemoteBackendEvent>,
    ) -> RemoteFuture<'a, Result<(), RemoteBackendError>> {
        let session = self.session.clone();
        let tools = self.tools.clone();
        let target_environment = self.target_environment.clone();
        let resources = self.resources;
        Box::pin(async move {
            match request.request().operation() {
                RemoteOperation::Sync => execute_sync(session, events).await,
                RemoteOperation::Build(build) => execute_build(session, tools, target_environment, resources, build, events).await,
            }
        })
    }
}

async fn execute_sync(session: Arc<RunRemoteSession>, events: mpsc::Sender<RemoteBackendEvent>) -> Result<(), RemoteBackendError> {
    send_event(&events, RemoteBackendEvent::SyncProgress { completed_bytes: 0, total_bytes: None }).await?;
    let sync = tokio::task::spawn_blocking(move || session.sync_snapshot())
        .await
        .map_err(|error| RemoteBackendError::Failed(format!("snapshot worker failed: {error}")))?;
    let snapshot_id = sync.map_err(RemoteBackendError::Failed)?;
    send_event(&events, RemoteBackendEvent::SyncCompleted { snapshot_id }).await
}

async fn execute_build(
    session: Arc<RunRemoteSession>, tools: Arc<BTreeMap<String, PathBuf>>, target_environment: Arc<BTreeMap<String, String>>,
    resources: RemoteResourcePolicy, build: &crate::remote::RemoteBuild, events: mpsc::Sender<RemoteBackendEvent>,
) -> Result<(), RemoteBackendError> {
    let snapshot = session.claim_snapshot(build.snapshot_id()).map_err(RemoteBackendError::Failed)?;
    let executable = tools
        .get(build.tool().as_str())
        .ok_or_else(|| RemoteBackendError::Spawn(format!("loopback tool is not configured: {}", build.tool().as_str())))?;
    let job_path = session.new_job_path().map_err(RemoteBackendError::Failed)?;
    let _job = JobGuard { path: job_path.clone() };
    let materialization_claim = snapshot.clone_for_worker().map_err(RemoteBackendError::Failed)?;
    let destination = job_path.clone();
    let snapshot_handle = snapshot.handle().clone();
    tokio::task::spawn_blocking({
        let session = session.clone();
        move || {
            let result = session.materialize_snapshot(&snapshot_handle, &destination);
            drop(materialization_claim);
            result
        }
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
    command.env_clear();
    for (key, value) in build.env() {
        command.env(key, value);
    }
    for (key, value) in target_environment.iter() {
        command.env(key, value);
    }
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
    let output_bytes = Arc::new(AtomicU64::new(0));
    let mut stdout_task = tokio::spawn(pump(stdout, RemoteStream::Stdout, events.clone(), output_bytes.clone(), resources.max_output_bytes));
    let mut stderr_task = tokio::spawn(pump(stderr, RemoteStream::Stderr, events.clone(), output_bytes, resources.max_output_bytes));
    let mut child_wait = Box::pin(child.wait());
    let mut timeout_sleep = Box::pin(sleep(resources.build_timeout));
    let mut post_exit_drain_sleep = Box::pin(sleep(POST_CHILD_EXIT_DRAIN_TIMEOUT));
    let mut final_drain_sleep = Box::pin(sleep(POST_CHILD_EXIT_FINAL_DRAIN_TIMEOUT));
    let mut child_status = None;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut failure = None;
    let mut post_exit_drain_active = false;
    let mut final_drain_active = false;
    let mut group_killed = false;

    while child_status.is_none() || !stdout_done || !stderr_done {
        tokio::select! {
            status = &mut child_wait, if child_status.is_none() => {
                let status = status.map_err(|error| RemoteBackendError::Failed(format!("wait for loopback tool: {error}")));
                child_status = Some(status);
                if failure.is_some() || child_status.as_ref().is_some_and(Result::is_err) {
                    kill_process_group(process_group.as_ref());
                    group_killed = true;
                    final_drain_sleep.as_mut().reset(tokio::time::Instant::now() + POST_CHILD_EXIT_FINAL_DRAIN_TIMEOUT);
                    final_drain_active = true;
                } else {
                    // The child is the process-group leader. Keep draining useful
                    // pipe data briefly, while terminating descendants that
                    // inherited the build descriptors.
                    terminate_process_group(process_group.as_ref());
                    post_exit_drain_sleep.as_mut().reset(tokio::time::Instant::now() + POST_CHILD_EXIT_DRAIN_TIMEOUT);
                    post_exit_drain_active = true;
                }
            }
            result = &mut stdout_task, if !stdout_done => {
                stdout_done = true;
                if let Err(error) = join_pump(result).await {
                    failure.get_or_insert(error);
                    kill_process_group(process_group.as_ref());
                    group_killed = true;
                    post_exit_drain_active = false;
                    if child_status.is_some() {
                        final_drain_sleep.as_mut().reset(tokio::time::Instant::now() + POST_CHILD_EXIT_FINAL_DRAIN_TIMEOUT);
                        final_drain_active = true;
                    }
                }
            }
            result = &mut stderr_task, if !stderr_done => {
                stderr_done = true;
                if let Err(error) = join_pump(result).await {
                    failure.get_or_insert(error);
                    kill_process_group(process_group.as_ref());
                    group_killed = true;
                    post_exit_drain_active = false;
                    if child_status.is_some() {
                        final_drain_sleep.as_mut().reset(tokio::time::Instant::now() + POST_CHILD_EXIT_FINAL_DRAIN_TIMEOUT);
                        final_drain_active = true;
                    }
                }
            }
            _ = &mut timeout_sleep, if child_status.is_none() => {
                failure.get_or_insert(RemoteBackendError::Timeout);
                kill_process_group(process_group.as_ref());
                group_killed = true;
            }
            _ = &mut post_exit_drain_sleep, if post_exit_drain_active => {
                post_exit_drain_active = false;
                kill_process_group(process_group.as_ref());
                group_killed = true;
                final_drain_sleep.as_mut().reset(tokio::time::Instant::now() + POST_CHILD_EXIT_FINAL_DRAIN_TIMEOUT);
                final_drain_active = true;
            }
            _ = &mut final_drain_sleep, if final_drain_active => {
                final_drain_active = false;
                if !stdout_done {
                    if let Some(error) = abort_pump(&mut stdout_task).await { failure.get_or_insert(error); }
                    stdout_done = true;
                }
                if !stderr_done {
                    if let Some(error) = abort_pump(&mut stderr_task).await { failure.get_or_insert(error); }
                    stderr_done = true;
                }
            }
        }
    }

    if failure.is_none() || !group_killed {
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
    mut reader: R, stream: RemoteStream, events: mpsc::Sender<RemoteBackendEvent>, output_bytes: Arc<AtomicU64>, max_output_bytes: u64,
) -> Result<(), RemoteBackendError> {
    let mut buffer = [0u8; 8192];
    loop {
        let count = reader.read(&mut buffer).await.map_err(|error| RemoteBackendError::Failed(format!("read loopback output: {error}")))?;
        if count == 0 {
            return Ok(());
        }
        let total = output_bytes.fetch_add(count as u64, Ordering::Relaxed).saturating_add(count as u64);
        if total > max_output_bytes {
            return Err(RemoteBackendError::OutputLimit { limit: max_output_bytes });
        }
        let event = match stream {
            RemoteStream::Stdout => RemoteBackendEvent::Stdout(buffer[..count].to_vec()),
            RemoteStream::Stderr => RemoteBackendEvent::Stderr(buffer[..count].to_vec()),
        };
        events.send(event).await.map_err(|_| RemoteBackendError::Cancelled)?;
    }
}

fn trusted_target_environment() -> BTreeMap<String, String> {
    BTreeMap::from([(String::from("PATH"), String::from(LOOPBACK_PATH))])
}

async fn join_pump(result: Result<Result<(), RemoteBackendError>, tokio::task::JoinError>) -> Result<(), RemoteBackendError> {
    result.map_err(|error| RemoteBackendError::Failed(format!("loopback output task failed: {error}")))?
}

async fn abort_pump(task: &mut tokio::task::JoinHandle<Result<(), RemoteBackendError>>) -> Option<RemoteBackendError> {
    if !task.is_finished() {
        task.abort();
    }
    match task.await {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error),
        Err(error) if error.is_cancelled() => None,
        Err(error) => Some(RemoteBackendError::Failed(format!("loopback output task failed: {error}"))),
    }
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
    signal_process_group(group, libc::SIGTERM);
    signal_process_group(group, libc::SIGKILL);
}

fn terminate_process_group(group: Option<&ProcessGroupGuard>) {
    signal_process_group(group, libc::SIGTERM);
}

fn signal_process_group(group: Option<&ProcessGroupGuard>, signal: libc::c_int) {
    if let Some(group) = group {
        if group.pgid > 0 {
            // A negative PID targets the Unix process group, so this remains
            // effective after the original group leader has exited.
            unsafe {
                libc::kill(-group.pgid, signal);
            }
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
