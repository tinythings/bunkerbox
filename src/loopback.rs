use crate::artifact::{ArtifactLimits, ArtifactPolicy, ArtifactPublication, LocalArtifactSpool};
use crate::remote::{
    AuthorizedRemoteRequest, RemoteBackend, RemoteBackendError, RemoteBackendEvent, RemoteExecutionControl, RemoteFuture, RemoteOperation,
    RemoteResourcePolicy, RemoteSnapshotAuthority, RemoteSnapshotId, RemoteTargetId, WorkspaceSessionId,
};
use crate::snapshot::{SnapshotBuilder, SnapshotEntry, SnapshotExport, SnapshotHandle, SnapshotStore};
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
use tokio::sync::{mpsc, Notify};
use tokio::time::sleep;

pub const LOOPBACK_BUILD_TIMEOUT: Duration = crate::remote::DEFAULT_REMOTE_BUILD_TIMEOUT;
pub const MAX_OUTSTANDING_SNAPSHOT_CAPABILITIES: usize = 16;
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

    pub(crate) fn jobs_root(&self) -> &Path {
        &self.jobs_root
    }

    pub fn snapshot_store(&self) -> SnapshotStore {
        self.snapshot_store.clone()
    }

    #[cfg(test)]
    pub(crate) fn snapshot_capability_count(&self) -> usize {
        self.snapshot_capabilities.lock().map(|registry| registry.capabilities.len()).unwrap_or(0)
    }

    pub fn sync_snapshot(&self) -> Result<RemoteSnapshotId, String> {
        self.sync_snapshot_for_request(true)?.ok_or_else(|| "retained snapshot capability was not created".to_string())
    }

    fn sync_snapshot_for_request(&self, retain_capability: bool) -> Result<Option<RemoteSnapshotId>, String> {
        self.sync_snapshot_for_request_with_control(retain_capability, RemoteExecutionControl::new())
    }

    pub(crate) fn sync_snapshot_for_request_with_control(
        &self, retain_capability: bool, control: RemoteExecutionControl,
    ) -> Result<Option<RemoteSnapshotId>, String> {
        let _operation = self.snapshot_operation.lock().map_err(|_| "remote session operation lock poisoned".to_string())?;
        let snapshot = self.snapshot_builder.build_root_with_control(&self.workspace_root, self.session_id, control.clone())?;
        let handle = snapshot.handle().clone();
        if !retain_capability {
            self.discard_unclaimed_snapshot(&handle)?;
            return Ok(None);
        }

        if control.is_cancelled() {
            self.discard_unclaimed_snapshot(&handle)?;
            return Err("snapshot synchronization cancelled".to_string());
        }
        match self.register_snapshot(handle.clone()) {
            Ok(snapshot_id) => {
                if control.is_cancelled() {
                    self.abort_snapshot_capability(snapshot_id)?;
                    return Err("snapshot synchronization cancelled".to_string());
                }
                Ok(Some(snapshot_id))
            }
            Err(error) => {
                let cleanup = self.discard_unclaimed_snapshot(&handle);
                if let Err(cleanup_error) = cleanup {
                    return Err(format!("{error}; snapshot cleanup failed: {cleanup_error}"));
                }
                Err(error)
            }
        }
    }

    fn register_snapshot(&self, handle: SnapshotHandle) -> Result<RemoteSnapshotId, String> {
        let mut registry = self.snapshot_capabilities.lock().map_err(|_| "remote snapshot registry lock poisoned".to_string())?;
        if registry.capabilities.len() >= MAX_OUTSTANDING_SNAPSHOT_CAPABILITIES {
            return Err(format!("remote snapshot capability limit reached ({MAX_OUTSTANDING_SNAPSHOT_CAPABILITIES})"));
        }
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

    fn discard_unclaimed_snapshot(&self, handle: &SnapshotHandle) -> Result<(), String> {
        let referenced =
            self.snapshot_capabilities.lock().map_err(|_| "remote snapshot registry lock poisoned".to_string())?.references.contains_key(handle);
        if referenced {
            Ok(())
        } else {
            self.snapshot_store.remove(handle)
        }
    }

    #[allow(dead_code)]
    pub(crate) fn abort_snapshot_capability(&self, snapshot_id: RemoteSnapshotId) -> Result<(), String> {
        let handle = self
            .snapshot_capabilities
            .lock()
            .map_err(|_| "remote snapshot registry lock poisoned".to_string())?
            .capabilities
            .remove(&snapshot_id)
            .ok_or_else(|| "remote snapshot capability is unavailable".to_string())?;
        self.release_snapshot(&handle);
        Ok(())
    }

    pub(crate) fn claim_snapshot(self: &Arc<Self>, snapshot_id: RemoteSnapshotId) -> Result<SnapshotClaim, String> {
        let mut registry = self.snapshot_capabilities.lock().map_err(|_| "remote snapshot registry lock poisoned".to_string())?;
        let handle = registry.capabilities.remove(&snapshot_id).ok_or_else(|| "remote snapshot capability is unavailable".to_string())?;
        Ok(SnapshotClaim { session: self.clone(), handle })
    }

    #[allow(dead_code)]
    pub(crate) fn claim_snapshot_for_export(self: &Arc<Self>, snapshot_id: RemoteSnapshotId) -> Result<SnapshotExportClaim, String> {
        let handle = {
            let mut registry = self.snapshot_capabilities.lock().map_err(|_| "remote snapshot registry lock poisoned".to_string())?;
            let handle = registry.capabilities.get(&snapshot_id).cloned().ok_or_else(|| "remote snapshot capability is unavailable".to_string())?;
            let references = registry.references.entry(handle.clone()).or_insert(0);
            *references = references.checked_add(1).ok_or_else(|| "remote snapshot capability reference count overflow".to_string())?;
            handle
        };
        let snapshot = match self.snapshot_store.resolve_export(&handle) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.release_snapshot(&handle);
                return Err(error);
            }
        };
        Ok(SnapshotExportClaim { session: self.clone(), handle, snapshot })
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

    fn materialize_snapshot_with_control(&self, handle: &SnapshotHandle, destination: &Path, control: RemoteExecutionControl) -> Result<(), String> {
        let _operation = self.snapshot_operation.lock().map_err(|_| "remote session operation lock poisoned".to_string())?;
        self.snapshot_store.materialize_with_control(handle, destination, control).map(|_| ())
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

pub(crate) struct SnapshotClaim {
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

#[allow(dead_code)]
pub(crate) struct SnapshotExportClaim {
    session: Arc<RunRemoteSession>,
    handle: SnapshotHandle,
    snapshot: SnapshotExport,
}

#[allow(dead_code)]
impl SnapshotExportClaim {
    pub(crate) fn handle(&self) -> &SnapshotHandle {
        &self.handle
    }

    pub(crate) fn entries(&self) -> &[SnapshotEntry] {
        self.snapshot.entries()
    }

    pub(crate) fn total_file_bytes(&self) -> u64 {
        self.snapshot.total_file_bytes()
    }

    pub(crate) fn read_file(&self, entry: &SnapshotEntry) -> Result<Vec<u8>, String> {
        self.snapshot.read_file(entry)
    }

    pub(crate) fn read_file_bounded(&self, entry: &SnapshotEntry, max_bytes: u64) -> Result<Vec<u8>, String> {
        self.snapshot.read_file_bounded(entry, max_bytes)
    }

    pub(crate) fn read_file_bounded_with_control(
        &self, entry: &SnapshotEntry, max_bytes: u64, control: &RemoteExecutionControl,
    ) -> Result<Vec<u8>, String> {
        self.snapshot.read_file_bounded_with_control(entry, max_bytes, control)
    }
}

impl Drop for SnapshotExportClaim {
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
    artifact_policy: ArtifactPolicy,
    artifact_limits: ArtifactLimits,
}

impl LoopbackBackend {
    pub fn new(session: Arc<RunRemoteSession>, tools: BTreeMap<String, PathBuf>) -> Self {
        Self {
            session,
            tools: Arc::new(tools),
            target_environment: Arc::new(trusted_target_environment()),
            resources: RemoteResourcePolicy::default(),
            artifact_policy: ArtifactPolicy::default(),
            artifact_limits: ArtifactLimits::default(),
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

    pub fn with_resources(mut self, resources: RemoteResourcePolicy) -> Self {
        self.resources = resources;
        self
    }

    pub fn with_target_environment(mut self, environment: BTreeMap<String, String>) -> Self {
        let mut trusted = trusted_target_environment();
        trusted.extend(environment);
        self.target_environment = Arc::new(trusted);
        self
    }

    pub fn with_artifacts(mut self, policy: ArtifactPolicy, limits: ArtifactLimits) -> Self {
        self.artifact_policy = policy;
        self.artifact_limits = limits;
        self
    }
}

impl RemoteBackend for LoopbackBackend {
    fn execute<'a>(
        &'a self, request: AuthorizedRemoteRequest, control: RemoteExecutionControl, events: mpsc::Sender<RemoteBackendEvent>,
    ) -> RemoteFuture<'a, Result<(), RemoteBackendError>> {
        let session = self.session.clone();
        let tools = self.tools.clone();
        let target_environment = self.target_environment.clone();
        let resources = self.resources;
        let artifact_policy = self.artifact_policy.clone();
        let artifact_limits = self.artifact_limits;
        let request_id = request.request_id().0;
        let options = LoopbackBuildOptions { resources, artifact_policy, artifact_limits, request_id };
        Box::pin(async move {
            match request.request().operation() {
                RemoteOperation::Sync(sync) => execute_sync(session, sync.retain_capability(), resources, control, events).await,
                RemoteOperation::Build(build) => execute_build(session, tools, target_environment, options, build, control, events).await,
                RemoteOperation::Cancel { .. } => Err(RemoteBackendError::Failed("cancel is handled by the remote broker".to_string())),
            }
        })
    }
}

impl LoopbackBackend {
    pub async fn execute(&self, request: AuthorizedRemoteRequest, events: mpsc::Sender<RemoteBackendEvent>) -> Result<(), RemoteBackendError> {
        <Self as RemoteBackend>::execute(self, request, RemoteExecutionControl::new(), events).await
    }
}

#[derive(Clone)]
struct LoopbackBuildOptions {
    resources: RemoteResourcePolicy,
    artifact_policy: ArtifactPolicy,
    artifact_limits: ArtifactLimits,
    request_id: [u8; 16],
}

async fn execute_sync(
    session: Arc<RunRemoteSession>, retain_capability: bool, resources: RemoteResourcePolicy, control: RemoteExecutionControl,
    events: mpsc::Sender<RemoteBackendEvent>,
) -> Result<(), RemoteBackendError> {
    control.set_phase(crate::remote::RemoteLifecyclePhase::Syncing);
    send_event(&events, RemoteBackendEvent::SyncProgress { completed_bytes: 0, total_bytes: None }).await?;
    let snapshot_control = control.clone();
    let mut sync_task = tokio::task::spawn_blocking(move || session.sync_snapshot_for_request_with_control(retain_capability, snapshot_control));
    let sync = tokio::select! {
        result = tokio::time::timeout(resources.sync_timeout, &mut sync_task) => match result {
            Ok(result) => result.map_err(|error| RemoteBackendError::Failed(format!("snapshot worker failed: {error}")))?,
            Err(_) => {
                control.cancel();
                let _ = tokio::time::timeout(resources.cleanup_timeout, &mut sync_task).await;
                return Err(RemoteBackendError::Deadline { cause: crate::remote::RemoteTimeoutCause::Sync });
            }
        },
        _ = control.cancelled() => {
            let _ = tokio::time::timeout(resources.cleanup_timeout, &mut sync_task).await;
            return Err(RemoteBackendError::Cancelled);
        },
    };
    match sync.map_err(RemoteBackendError::Failed)? {
        Some(snapshot_id) => send_event(&events, RemoteBackendEvent::SyncCompleted { snapshot_id }).await,
        None => send_event(&events, RemoteBackendEvent::Completed { exit_code: 0 }).await,
    }
}

async fn execute_build(
    session: Arc<RunRemoteSession>, tools: Arc<BTreeMap<String, PathBuf>>, target_environment: Arc<BTreeMap<String, String>>,
    options: LoopbackBuildOptions, build: &crate::remote::RemoteBuild, control: RemoteExecutionControl, events: mpsc::Sender<RemoteBackendEvent>,
) -> Result<(), RemoteBackendError> {
    let LoopbackBuildOptions { resources, artifact_policy, artifact_limits, request_id } = options;
    control.set_phase(crate::remote::RemoteLifecyclePhase::Transferring);
    if control.is_cancelled() {
        return Err(RemoteBackendError::Cancelled);
    }
    let snapshot = session.claim_snapshot(build.snapshot_id()).map_err(RemoteBackendError::Failed)?;
    let executable = tools
        .get(build.tool().as_str())
        .ok_or_else(|| RemoteBackendError::Spawn(format!("loopback tool is not configured: {}", build.tool().as_str())))?;
    let job_path = session.new_job_path().map_err(RemoteBackendError::Failed)?;
    let mut job = JobGuard { path: Some(job_path.clone()) };
    let materialization_claim = snapshot.clone_for_worker().map_err(RemoteBackendError::Failed)?;
    let destination = job_path.clone();
    let snapshot_handle = snapshot.handle().clone();
    let materialize_control = control.clone();
    let mut materialize = tokio::task::spawn_blocking({
        let session = session.clone();
        move || {
            let result = session.materialize_snapshot_with_control(&snapshot_handle, &destination, materialize_control);
            drop(materialization_claim);
            result
        }
    });
    tokio::select! {
        result = tokio::time::timeout(resources.sync_timeout, &mut materialize) => match result {
            Ok(result) => result
                .map_err(|error| RemoteBackendError::Failed(format!("materialization worker failed: {error}")))?
                .map_err(RemoteBackendError::Failed)?,
            Err(_) => {
                control.cancel();
                let _ = tokio::time::timeout(resources.cleanup_timeout, &mut materialize).await;
                return Err(RemoteBackendError::Deadline { cause: crate::remote::RemoteTimeoutCause::Sync });
            }
        },
        _ = control.cancelled() => {
            let _ = tokio::time::timeout(resources.cleanup_timeout, &mut materialize).await;
            return Err(RemoteBackendError::Cancelled);
        },
    }

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
    control.set_phase(crate::remote::RemoteLifecyclePhase::Building);
    let process_group = child.id().map(|pid| ProcessGroupGuard { pgid: pid as i32, active: true });
    let stdout = child.stdout.take().ok_or_else(|| RemoteBackendError::Failed("loopback child has no stdout".to_string()))?;
    let stderr = child.stderr.take().ok_or_else(|| RemoteBackendError::Failed("loopback child has no stderr".to_string()))?;
    let output_bytes = Arc::new(AtomicU64::new(0));
    let output_activity = Arc::new(Notify::new());
    let mut stdout_task = tokio::spawn(pump(
        stdout,
        RemoteStream::Stdout,
        events.clone(),
        output_bytes.clone(),
        output_activity.clone(),
        control.clone(),
        resources.max_output_bytes,
    ));
    let mut stderr_task = tokio::spawn(pump(
        stderr,
        RemoteStream::Stderr,
        events.clone(),
        output_bytes,
        output_activity.clone(),
        control.clone(),
        resources.max_output_bytes,
    ));
    let mut child_wait = Box::pin(child.wait());
    let mut timeout_sleep = Box::pin(sleep(resources.build_timeout));
    let mut idle_output_sleep = Box::pin(sleep(resources.idle_output_timeout));
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
                failure.get_or_insert(RemoteBackendError::Deadline { cause: crate::remote::RemoteTimeoutCause::Build });
                kill_process_group(process_group.as_ref());
                group_killed = true;
            }
            _ = &mut idle_output_sleep, if child_status.is_none() => {
                failure.get_or_insert(RemoteBackendError::Deadline { cause: crate::remote::RemoteTimeoutCause::IdleOutput });
                kill_process_group(process_group.as_ref());
                group_killed = true;
            }
            _ = output_activity.notified(), if child_status.is_none() => {
                idle_output_sleep.as_mut().reset(tokio::time::Instant::now() + resources.idle_output_timeout);
            }
            _ = control.cancelled(), if child_status.is_none() || !stdout_done || !stderr_done => {
                failure.get_or_insert(RemoteBackendError::Cancelled);
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
        control.set_phase(crate::remote::RemoteLifecyclePhase::Cleanup);
        let cleanup_error = job.cleanup(resources.cleanup_timeout).await.err();
        return Err(cleanup_error.unwrap_or(error));
    }
    let status = match child_status.unwrap() {
        Ok(status) => status,
        Err(error) => {
            control.set_phase(crate::remote::RemoteLifecyclePhase::Cleanup);
            let cleanup_error = job.cleanup(resources.cleanup_timeout).await.err();
            return Err(cleanup_error.unwrap_or(error));
        }
    };
    let exit_code = status.code().unwrap_or(-1);
    if exit_code == 0 && artifact_policy.is_enabled() {
        control.set_phase(crate::remote::RemoteLifecyclePhase::ArtifactHandling);
        let job_root = job_path.clone();
        let workspace_root = session.workspace_root().to_path_buf();
        let spool_parent = session.jobs_root().to_path_buf();
        let policy = artifact_policy.clone();
        let limits = artifact_limits;
        let artifact_control = control.clone();
        let artifact_result = tokio::select! {
            result = tokio::time::timeout(
                limits.timeout,
                tokio::task::spawn_blocking(move || {
                    let control = artifact_control;
                    if control.is_cancelled() {
                        return Err(RemoteBackendError::Cancelled);
                    }
                let spool = LocalArtifactSpool::capture_with_control(&job_root, &spool_parent, &policy, limits, &control)
                    .map_err(|error| RemoteBackendError::Transport { class: crate::remote::RemoteFailureClass::ArtifactManifest, message: error })?;
                let manifest = spool.manifest().clone();
                let mut publication = ArtifactPublication::new(&workspace_root, request_id, manifest)
                    .map_err(|error| RemoteBackendError::Transport { class: crate::remote::RemoteFailureClass::ArtifactTransfer, message: error })?;
                for index in 0..spool.manifest().entries().len() {
                    if control.is_cancelled() {
                        return Err(RemoteBackendError::Cancelled);
                    }
                    let mut source = spool.open_entry(index).map_err(|error| RemoteBackendError::Transport {
                        class: crate::remote::RemoteFailureClass::ArtifactTransfer,
                        message: error,
                    })?;
                    publication.copy_from_reader_with_control(index, &mut source, &control).map_err(|error| RemoteBackendError::Transport {
                        class: crate::remote::RemoteFailureClass::ArtifactTransfer,
                        message: error,
                    })?;
                }
                if control.is_cancelled() {
                    return Err(RemoteBackendError::Cancelled);
                }
                publication
                    .publish()
                    .map_err(|error| RemoteBackendError::Transport { class: crate::remote::RemoteFailureClass::ArtifactTransfer, message: error })
                }),
            ) => match result {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => Err(RemoteBackendError::Failed(format!("artifact worker failed: {error}"))),
                Err(_) => {
                    control.cancel();
                    Err(RemoteBackendError::Deadline { cause: crate::remote::RemoteTimeoutCause::Artifact })
                }
            },
            _ = control.cancelled() => Err(RemoteBackendError::Cancelled),
        };
        if let Err(error) = artifact_result {
            control.set_phase(crate::remote::RemoteLifecyclePhase::Cleanup);
            let cleanup_error = job.cleanup(resources.cleanup_timeout).await.err();
            return Err(if matches!(error, RemoteBackendError::Cancelled) { error } else { cleanup_error.unwrap_or(error) });
        }
    }
    control.set_phase(crate::remote::RemoteLifecyclePhase::Cleanup);
    job.cleanup(resources.cleanup_timeout).await?;
    control.set_phase(crate::remote::RemoteLifecyclePhase::Finalizing);
    send_event(&events, RemoteBackendEvent::Completed { exit_code }).await
}

#[derive(Clone, Copy)]
enum RemoteStream {
    Stdout,
    Stderr,
}

async fn pump<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R, stream: RemoteStream, events: mpsc::Sender<RemoteBackendEvent>, output_bytes: Arc<AtomicU64>, output_activity: Arc<Notify>,
    control: RemoteExecutionControl, max_output_bytes: u64,
) -> Result<(), RemoteBackendError> {
    let mut buffer = [0u8; 8192];
    loop {
        let count = tokio::select! {
            result = reader.read(&mut buffer) => result.map_err(|error| RemoteBackendError::Failed(format!("read loopback output: {error}")))?,
            _ = control.cancelled() => return Err(RemoteBackendError::Cancelled),
        };
        if count == 0 {
            return Ok(());
        }
        let total = output_bytes.fetch_add(count as u64, Ordering::Relaxed).saturating_add(count as u64);
        if total > max_output_bytes {
            return Err(RemoteBackendError::OutputLimit { limit: max_output_bytes });
        }
        output_activity.notify_waiters();
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
    path: Option<PathBuf>,
}

impl JobGuard {
    async fn cleanup(&mut self, cleanup_timeout: Duration) -> Result<(), RemoteBackendError> {
        let Some(path) = self.path.take() else { return Ok(()) };
        let result = tokio::time::timeout(cleanup_timeout, tokio::task::spawn_blocking(move || fs::remove_dir_all(path))).await;
        match result {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(Ok(Err(error))) => Err(RemoteBackendError::Transport {
                class: crate::remote::RemoteFailureClass::Cleanup,
                message: format!("remove loopback job: {error}"),
            }),
            Ok(Err(error)) => Err(RemoteBackendError::Failed(format!("loopback cleanup worker failed: {error}"))),
            Err(_) => Err(RemoteBackendError::Deadline { cause: crate::remote::RemoteTimeoutCause::Cleanup }),
        }
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_dir_all(path);
        }
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
