use crate::artifact::{ArtifactLimits, ArtifactPolicy};
use crate::cfg::EnvMode;
use crate::logging;
use crate::loopback::{LoopbackBackend, RunRemoteSession};
use crate::proxy::{FilterProxy, UnixProxyHandle};
use crate::remote::{
    RemoteAdmissionLimits, RemoteAuthorizationError, RemoteAuthorizationPolicy, RemoteBackend, RemoteBackendError, RemoteBackendEvent, RemoteBuild,
    RemoteEnvironmentPolicy, RemoteExecutionContext, RemoteRequest, RemoteResourcePolicy, RemoteSnapshotId, RemoteTargetId, RemoteTool,
    RemoteToolPolicy, RequestId, WorkspaceRelativePath, WorkspaceSessionId,
};
use crate::remote_target::SshTarget;
use crate::remote_target::{ActiveBuildTarget, BuildTargetCatalog};
use crate::sandbox::{resolve_profile, MergedProfile, NetworkMode};
use crate::ssh::SshBackend;
use crate::vscomm::{validate_exec_request, validate_process_path, ExecRequest, Frame, FrameType, TOOLCHAIN_PORT};
use crate::workspace::WorkspaceCwd;
use rand::{Rng, RngCore};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

const BWRAP_STATUS_FD: RawFd = 3;

#[derive(Clone, Copy)]
enum ProcessStream {
    Stdout,
    Stderr,
}

enum ChildEvent {
    Output(ProcessStream, Vec<u8>),
    StreamClosed,
    LauncherStarted,
    LauncherFailed(String),
}

#[derive(Debug)]
struct SandboxProxyConfig {
    socket_path: PathBuf,
    netrelay_path: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RemoteDispatchError {
    Unauthorized(RemoteAuthorizationError),
    Backend(RemoteBackendError),
    EventSinkClosed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ExecutionKey {
    session_id: crate::remote::WorkspaceSessionId,
    request_id: crate::remote::RequestId,
}

struct AdmissionPermit {
    _global: OwnedSemaphorePermit,
    _target: OwnedSemaphorePermit,
}

struct RemoteAdmission {
    global: Arc<Semaphore>,
    targets: Mutex<BTreeMap<RemoteTargetId, Arc<Semaphore>>>,
    target_limit: usize,
}

impl RemoteAdmission {
    fn new(limits: RemoteAdmissionLimits) -> Self {
        Self { global: Arc::new(Semaphore::new(limits.global_active)), targets: Mutex::new(BTreeMap::new()), target_limit: limits.target_active }
    }

    fn acquire(&self, target_id: RemoteTargetId) -> Result<AdmissionPermit, RemoteBackendError> {
        let global = self.global.clone().try_acquire_owned().map_err(|_| RemoteBackendError::Transport {
            class: crate::remote::RemoteFailureClass::Busy,
            message: "remote global active-execution limit reached".to_string(),
        })?;
        let target_semaphore = self
            .targets
            .lock()
            .map_err(|_| RemoteBackendError::Failed("remote target admission lock poisoned".to_string()))?
            .entry(target_id)
            .or_insert_with(|| Arc::new(Semaphore::new(self.target_limit)))
            .clone();
        let target = match target_semaphore.try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                drop(global);
                return Err(RemoteBackendError::Transport {
                    class: crate::remote::RemoteFailureClass::Busy,
                    message: "remote target active-execution limit reached".to_string(),
                });
            }
        };
        Ok(AdmissionPermit { _global: global, _target: target })
    }
}

struct ExecutionState {
    cancellation_selected: bool,
    terminal: bool,
}

struct ExecutionEntry {
    control: crate::remote::RemoteExecutionControl,
    state: Mutex<ExecutionState>,
    _admission: AdmissionPermit,
}

impl ExecutionEntry {
    fn new(control: crate::remote::RemoteExecutionControl, admission: AdmissionPermit) -> Self {
        Self { control, state: Mutex::new(ExecutionState { cancellation_selected: false, terminal: false }), _admission: admission }
    }

    fn request_cancel(&self) -> CancelSelection {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return CancelSelection::Rejected,
        };
        if state.terminal
            || matches!(self.control.phase(), crate::remote::RemoteLifecyclePhase::Finalizing | crate::remote::RemoteLifecyclePhase::Terminal)
        {
            return CancelSelection::Rejected;
        }
        if state.cancellation_selected {
            return CancelSelection::AlreadySelected;
        }
        state.cancellation_selected = true;
        self.control.set_phase(crate::remote::RemoteLifecyclePhase::Cancelling);
        self.control.cancel();
        CancelSelection::Selected
    }

    fn cancellation_selected(&self) -> bool {
        self.state.lock().map(|state| state.cancellation_selected).unwrap_or(true)
    }

    fn accept_event(&self, event: &RemoteBackendEvent) -> bool {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return false,
        };
        if state.terminal || (state.cancellation_selected && !matches!(event, RemoteBackendEvent::Cancelled)) {
            return false;
        }
        if is_terminal_event(event) {
            state.terminal = true;
            self.control.set_phase(crate::remote::RemoteLifecyclePhase::Terminal);
        }
        true
    }

    fn accept_cancelled(&self) -> bool {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return false,
        };
        if state.terminal || !state.cancellation_selected {
            return false;
        }
        state.terminal = true;
        self.control.set_phase(crate::remote::RemoteLifecyclePhase::Terminal);
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CancelSelection {
    Selected,
    AlreadySelected,
    Rejected,
}

struct RemoteExecutionRegistry {
    entries: Mutex<BTreeMap<ExecutionKey, Arc<ExecutionEntry>>>,
}

impl RemoteExecutionRegistry {
    fn new() -> Self {
        Self { entries: Mutex::new(BTreeMap::new()) }
    }

    fn insert(&self, key: ExecutionKey, entry: Arc<ExecutionEntry>) -> Result<(), RemoteBackendError> {
        let mut entries = self.entries.lock().map_err(|_| RemoteBackendError::Failed("remote execution registry lock poisoned".to_string()))?;
        if entries.contains_key(&key) {
            return Err(RemoteBackendError::Transport {
                class: crate::remote::RemoteFailureClass::Busy,
                message: "remote request ID is already active for this session".to_string(),
            });
        }
        entries.insert(key, entry);
        Ok(())
    }

    fn get(&self, key: &ExecutionKey) -> Option<Arc<ExecutionEntry>> {
        self.entries.lock().ok().and_then(|entries| entries.get(key).cloned())
    }

    fn remove(&self, key: &ExecutionKey) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(key);
        }
    }

    fn cancel_all(&self) {
        if let Ok(entries) = self.entries.lock() {
            for entry in entries.values() {
                let _ = entry.request_cancel();
            }
        };
    }
}

struct ExecutionGuard {
    registry: Arc<RemoteExecutionRegistry>,
    key: ExecutionKey,
    entry: Arc<ExecutionEntry>,
    finished: bool,
}

impl ExecutionGuard {
    fn finish(mut self) {
        self.finished = true;
        self.registry.remove(&self.key);
    }
}

impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.entry.request_cancel();
            self.registry.remove(&self.key);
        }
    }
}

fn is_terminal_event(event: &RemoteBackendEvent) -> bool {
    matches!(
        event,
        RemoteBackendEvent::SyncCompleted { .. }
            | RemoteBackendEvent::Completed { .. }
            | RemoteBackendEvent::Error { .. }
            | RemoteBackendEvent::Cancelled
    )
}

pub struct RemoteBroker {
    policy: RemoteAuthorizationPolicy,
    context: RemoteExecutionContext,
    backend: Arc<dyn RemoteBackend>,
    registry: Arc<RemoteExecutionRegistry>,
    admission: Arc<RemoteAdmission>,
    cleanup_timeout: std::time::Duration,
}

impl RemoteBroker {
    pub fn new(policy: RemoteAuthorizationPolicy, context: RemoteExecutionContext, backend: Arc<dyn RemoteBackend>) -> Self {
        Self {
            policy,
            context,
            backend,
            registry: Arc::new(RemoteExecutionRegistry::new()),
            admission: Arc::new(RemoteAdmission::new(RemoteAdmissionLimits::default())),
            cleanup_timeout: RemoteResourcePolicy::default().cleanup_timeout,
        }
    }

    pub fn with_admission_limits(mut self, limits: RemoteAdmissionLimits) -> Self {
        self.admission = Arc::new(RemoteAdmission::new(limits));
        self
    }

    pub fn with_cleanup_timeout(mut self, cleanup_timeout: std::time::Duration) -> Self {
        self.cleanup_timeout = cleanup_timeout;
        self
    }

    pub fn cancel_request(&self, request_id: crate::remote::RequestId) -> bool {
        let key = ExecutionKey { session_id: self.context.workspace_session_id, request_id };
        self.registry.get(&key).is_some_and(|entry| !matches!(entry.request_cancel(), CancelSelection::Rejected))
    }

    pub fn cleanup_timeout(&self) -> std::time::Duration {
        self.cleanup_timeout
    }

    fn allows_tool(&self, tool: &str) -> bool {
        self.policy.allowed_tools().any(|allowed| allowed == tool)
    }

    fn selected_environment_for_tool(&self, tool: &str, entries: &[(String, String)]) -> Result<Vec<(String, String)>, String> {
        self.policy.selected_environment_for_tool(tool, entries).map_err(|error| format!("remote environment rejected: {error:?}"))
    }

    pub async fn dispatch(&self, request: RemoteRequest, events: tokio::sync::mpsc::Sender<RemoteBackendEvent>) -> Result<(), RemoteDispatchError> {
        let authorized = match self.policy.authorize(&self.context, request) {
            Ok(authorized) => authorized,
            Err(error) => {
                events.send(error.event()).await.map_err(|_| RemoteDispatchError::EventSinkClosed)?;
                return Err(RemoteDispatchError::Unauthorized(error));
            }
        };

        if let crate::remote::RemoteOperation::Cancel { target_request_id } = authorized.request().operation() {
            return self.dispatch_cancel(authorized.request_id(), *target_request_id, events).await;
        }

        let request_id = authorized.request_id();
        let key = ExecutionKey { session_id: self.context.workspace_session_id, request_id };
        let admission = match self.admission.acquire(self.context.target) {
            Ok(admission) => admission,
            Err(error) => {
                events.send(error.event()).await.map_err(|_| RemoteDispatchError::EventSinkClosed)?;
                return Err(RemoteDispatchError::Backend(error));
            }
        };
        let control = crate::remote::RemoteExecutionControl::new();
        let entry = Arc::new(ExecutionEntry::new(control.clone(), admission));
        self.registry.insert(key, entry.clone()).map_err(RemoteDispatchError::Backend)?;
        let guard = ExecutionGuard { registry: self.registry.clone(), key, entry: entry.clone(), finished: false };
        let (backend_events, mut backend_rx) = mpsc::channel(64);
        let mut backend = Box::pin(self.backend.execute(authorized, control.clone(), backend_events));
        let mut backend_result = None;
        let mut terminal_sent = false;
        let mut sink_closed = false;
        let mut terminal_deadline = Box::pin(tokio::time::sleep(self.cleanup_timeout));

        loop {
            if backend_result.is_some() && backend_rx.is_empty() {
                break;
            }
            tokio::select! {
                result = &mut backend, if backend_result.is_none() => {
                    backend_result = Some(result);
                }
                event = backend_rx.recv() => {
                    let Some(event) = event else { continue };
                    if !entry.accept_event(&event) {
                        continue;
                    }
                    let terminal = is_terminal_event(&event);
                    if !sink_closed {
                        tokio::select! {
                            result = events.send(event) => {
                                if result.is_err() {
                                    sink_closed = true;
                                    let _ = entry.request_cancel();
                                }
                            }
                            _ = control.cancelled(), if !terminal => {
                                sink_closed = true;
                                let _ = entry.request_cancel();
                            }
                        }
                    }
                    if terminal {
                        terminal_sent = true;
                        terminal_deadline.as_mut().reset(tokio::time::Instant::now() + self.cleanup_timeout);
                    }
                }
                _ = control.cancelled(), if !terminal_sent => {
                    if entry.accept_cancelled() {
                        terminal_sent = true;
                        terminal_deadline.as_mut().reset(tokio::time::Instant::now() + self.cleanup_timeout);
                        if !sink_closed && events.send(RemoteBackendEvent::Cancelled).await.is_err() {
                            sink_closed = true;
                        }
                    }
                }
                _ = &mut terminal_deadline, if terminal_sent && backend_result.is_none() => {
                    backend_result = Some(Err(RemoteBackendError::Deadline { cause: crate::remote::RemoteTimeoutCause::Cleanup }));
                }
            }
        }

        let result = backend_result.unwrap_or(Ok(()));
        if !terminal_sent {
            if entry.cancellation_selected() {
                if entry.accept_cancelled() && !sink_closed && events.send(RemoteBackendEvent::Cancelled).await.is_err() {
                    sink_closed = true;
                }
            } else {
                let error = match &result {
                    Ok(()) => RemoteBackendError::Failed("remote backend completed without a terminal event".to_string()),
                    Err(error) => error.clone(),
                };
                let event = error.event();
                if entry.accept_event(&event) && !sink_closed && events.send(event).await.is_err() {
                    sink_closed = true;
                }
            }
        }

        drop(backend);
        guard.finish();
        if sink_closed {
            return Err(RemoteDispatchError::EventSinkClosed);
        }
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err(RemoteDispatchError::Backend(error)),
        }
    }

    async fn dispatch_cancel(
        &self, _cancel_request_id: crate::remote::RequestId, target_request_id: crate::remote::RequestId,
        events: tokio::sync::mpsc::Sender<RemoteBackendEvent>,
    ) -> Result<(), RemoteDispatchError> {
        let key = ExecutionKey { session_id: self.context.workspace_session_id, request_id: target_request_id };
        let Some(entry) = self.registry.get(&key) else {
            let error = RemoteAuthorizationError::CancelTargetUnavailable;
            events.send(error.event()).await.map_err(|_| RemoteDispatchError::EventSinkClosed)?;
            return Err(RemoteDispatchError::Unauthorized(error));
        };
        if matches!(entry.control.phase(), crate::remote::RemoteLifecyclePhase::Finalizing | crate::remote::RemoteLifecyclePhase::Terminal) {
            let error = RemoteAuthorizationError::CancelTargetFinalizing;
            events.send(error.event()).await.map_err(|_| RemoteDispatchError::EventSinkClosed)?;
            return Err(RemoteDispatchError::Unauthorized(error));
        }
        match entry.request_cancel() {
            CancelSelection::Selected | CancelSelection::AlreadySelected => {
                events.send(RemoteBackendEvent::Completed { exit_code: 0 }).await.map_err(|_| RemoteDispatchError::EventSinkClosed)?;
                Ok(())
            }
            CancelSelection::Rejected => {
                let error = RemoteAuthorizationError::CancelTargetFinalizing;
                events.send(error.event()).await.map_err(|_| RemoteDispatchError::EventSinkClosed)?;
                Err(RemoteDispatchError::Unauthorized(error))
            }
        }
    }

    pub fn cancel_all(&self) {
        self.registry.cancel_all();
    }
}

struct VsockSession {
    passthrough: Arc<Vec<String>>,
    env_mode: EnvMode,
    workspace: PathBuf,
    merged_profile: Option<Arc<MergedProfile>>,
    proxy_config: Option<Arc<SandboxProxyConfig>>,
    remote_router: Arc<RemoteRouter>,
    local_session_id: crate::remote::WorkspaceSessionId,
    local_capabilities: Mutex<BTreeMap<crate::remote::RemoteSnapshotId, ()>>,
}

pub struct VsockDaemon {
    join_handle: tokio::task::JoinHandle<()>,
    shutdown: tokio::sync::oneshot::Sender<()>,
    sandbox_proxy: Option<UnixProxyHandle>,
    sandbox_proxy_dir: Option<PathBuf>,
}

struct RemoteRouter {
    active_target: ActiveBuildTarget,
    brokers: BTreeMap<String, Arc<RemoteBroker>>,
    snapshots: Mutex<BTreeMap<crate::remote::RemoteSnapshotId, String>>,
    requests: Mutex<BTreeMap<crate::remote::RequestId, Arc<RemoteBroker>>>,
}

impl RemoteRouter {
    fn new(active_target: ActiveBuildTarget, brokers: BTreeMap<String, Arc<RemoteBroker>>) -> Self {
        Self { active_target, brokers, snapshots: Mutex::new(BTreeMap::new()), requests: Mutex::new(BTreeMap::new()) }
    }

    fn active_remote_broker(&self) -> Result<(String, Arc<RemoteBroker>), RemoteDispatchError> {
        let label = self.active_target.current();
        let broker = self.brokers.get(&label).cloned().ok_or(RemoteDispatchError::Unauthorized(RemoteAuthorizationError::TargetNotAllowed))?;
        Ok((label, broker))
    }

    fn is_remote_selected(&self) -> bool {
        self.active_target.current() != "localhost"
    }

    async fn dispatch_transparent_exec<W: AsyncWriteExt + Unpin>(
        &self, request: &ExecRequest, session_id: WorkspaceSessionId, writer: &mut W,
    ) -> Result<(), String> {
        let (label, broker) = self.active_remote_broker().map_err(|error| format!("remote dispatch failed: {error:?}"))?;
        if !broker.allows_tool(&request.command) {
            return Err(format!("remote tool is not configured for target {label}: {}", request.command));
        }
        let cwd = Path::new(&request.cwd)
            .strip_prefix("/workspace")
            .map_err(|_| "current directory must be under /workspace".to_string())?
            .to_str()
            .ok_or_else(|| "current directory is not valid UTF-8".to_string())?;
        let cwd = WorkspaceRelativePath::new(cwd)?;
        let environment = broker.selected_environment_for_tool(&request.command, &request.env)?;
        let sync_request = RemoteRequest::sync(new_remote_request_id(), session_id);
        let snapshot_id = self
            .dispatch_transparent_operation(&label, broker.clone(), sync_request, writer, true)
            .await?
            .ok_or_else(|| "remote sync did not return a retained capability".to_string())?;
        let build = RemoteBuild::new(cwd, RemoteTool::new(request.command.clone())?, request.args.clone(), environment, snapshot_id)?;
        let build_request = RemoteRequest::build(new_remote_request_id(), session_id, build);
        let result = self.dispatch_transparent_operation(&label, broker, build_request, writer, false).await;
        self.snapshots.lock().map_err(|_| "remote target snapshot lock poisoned".to_string())?.remove(&snapshot_id);
        result.map(|_| ())
    }

    async fn dispatch_transparent_operation<W: AsyncWriteExt + Unpin>(
        &self, label: &str, broker: Arc<RemoteBroker>, request: RemoteRequest, writer: &mut W, sync: bool,
    ) -> Result<Option<RemoteSnapshotId>, String> {
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let mut dispatch = Box::pin(broker.dispatch(request, event_tx));
        let mut dispatch_result = None;
        let mut snapshot_id = None;
        let mut terminal = false;

        loop {
            if let Some(result) = dispatch_result.take() {
                while let Ok(event) = event_rx.try_recv() {
                    self.forward_transparent_event(label, event, sync, writer, &mut snapshot_id, &mut terminal).await?;
                }
                dispatch_result = Some(result);
                break;
            }

            tokio::select! {
                result = &mut dispatch => dispatch_result = Some(result),
                event = event_rx.recv() => {
                    let Some(event) = event else { return Err("remote event stream closed before completion".to_string()) };
                    self.forward_transparent_event(label, event, sync, writer, &mut snapshot_id, &mut terminal).await?;
                }
            }
        }

        let result = dispatch_result.expect("transparent dispatch result is present");
        result.map_err(|error| format!("remote dispatch failed: {error:?}"))?;
        if sync {
            snapshot_id.ok_or_else(|| "remote sync completed without a capability".to_string()).map(Some)
        } else if terminal {
            Ok(None)
        } else {
            Err("remote build completed without an exit status".to_string())
        }
    }

    async fn forward_transparent_event<W: AsyncWriteExt + Unpin>(
        &self, label: &str, event: RemoteBackendEvent, sync: bool, writer: &mut W, snapshot_id: &mut Option<RemoteSnapshotId>, terminal: &mut bool,
    ) -> Result<(), String> {
        match event {
            RemoteBackendEvent::SyncProgress { .. } => {
                if !sync {
                    return Err("remote build returned a sync progress event".to_string());
                }
            }
            RemoteBackendEvent::SyncCompleted { snapshot_id: completed } => {
                if !sync {
                    return Err("remote build returned a sync completion".to_string());
                }
                self.snapshots.lock().map_err(|_| "remote target snapshot lock poisoned".to_string())?.insert(completed, label.to_string());
                *snapshot_id = Some(completed);
                *terminal = true;
            }
            RemoteBackendEvent::Stdout(data) => {
                if sync {
                    return Err("remote sync returned build output".to_string());
                }
                write_frame(writer, &Frame::new(FrameType::Stdout, data)).await?;
            }
            RemoteBackendEvent::Stderr(data) => {
                if sync {
                    return Err("remote sync returned build diagnostics".to_string());
                }
                write_frame(writer, &Frame::new(FrameType::Stderr, data)).await?;
            }
            RemoteBackendEvent::Error { message } => {
                if !sync {
                    write_frame(writer, &Frame::new(FrameType::Stderr, message.as_bytes().to_vec())).await?;
                }
                return Err(format!("remote target {label} request failed: {message}"));
            }
            RemoteBackendEvent::Cancelled => return Err(format!("remote target {label} request was cancelled")),
            RemoteBackendEvent::Completed { exit_code } => {
                if sync {
                    return Err("remote sync returned a build completion".to_string());
                }
                write_frame(writer, &Frame::new(FrameType::Exit, exit_code.to_le_bytes().to_vec())).await?;
                *terminal = true;
            }
        }
        Ok(())
    }

    async fn dispatch<W: AsyncWriteExt + Unpin>(&self, request: RemoteRequest, writer: &mut W) -> Result<(), String> {
        let (broker, label) = match request.operation() {
            crate::remote::RemoteOperation::Sync(_) => {
                let (label, broker) = self.active_remote_broker().map_err(|error| format!("remote dispatch failed: {error:?}"))?;
                (broker, Some(label))
            }
            crate::remote::RemoteOperation::Build(build) => {
                let label = self
                    .snapshots
                    .lock()
                    .map_err(|_| "remote target snapshot lock poisoned".to_string())?
                    .get(&build.snapshot_id())
                    .cloned()
                    .ok_or_else(|| "remote snapshot target binding is unavailable".to_string())?;
                let broker = self.brokers.get(&label).cloned().ok_or_else(|| "remote target binding is unavailable".to_string())?;
                (broker, Some(label))
            }
            crate::remote::RemoteOperation::Cancel { target_request_id } => {
                let broker = self
                    .requests
                    .lock()
                    .map_err(|_| "remote target request lock poisoned".to_string())?
                    .get(target_request_id)
                    .cloned()
                    .ok_or_else(|| "remote cancellation target is unavailable".to_string())?;
                (broker, None)
            }
        };

        let request_id = request.request_id();
        if !matches!(request.operation(), crate::remote::RemoteOperation::Cancel { .. }) {
            self.requests.lock().map_err(|_| "remote target request lock poisoned".to_string())?.insert(request_id, broker.clone());
        }
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let mut dispatch = Box::pin(broker.dispatch(request.clone(), event_tx));
        let result = loop {
            tokio::select! {
                dispatch_result = &mut dispatch => {
                    while let Ok(event) = event_rx.try_recv() {
                        self.forward_event(request_id, label.as_deref(), &request, event, writer).await?;
                    }
                    break dispatch_result;
                }
                event = event_rx.recv() => {
                    let Some(event) = event else { break Err(RemoteDispatchError::EventSinkClosed) };
                    self.forward_event(request_id, label.as_deref(), &request, event, writer).await?;
                }
            }
        };
        if matches!(request.operation(), crate::remote::RemoteOperation::Build(_)) {
            if let crate::remote::RemoteOperation::Build(build) = request.operation() {
                self.snapshots.lock().map_err(|_| "remote target snapshot lock poisoned".to_string())?.remove(&build.snapshot_id());
            }
        }
        self.requests.lock().map_err(|_| "remote target request lock poisoned".to_string())?.remove(&request_id);
        result.map_err(|error| format!("remote dispatch failed: {error:?}"))
    }

    async fn forward_event<W: AsyncWriteExt + Unpin>(
        &self, request_id: crate::remote::RequestId, label: Option<&str>, request: &RemoteRequest, event: RemoteBackendEvent, writer: &mut W,
    ) -> Result<(), String> {
        if let (Some(label), crate::remote::RemoteOperation::Sync(_), RemoteBackendEvent::SyncCompleted { snapshot_id }) =
            (label, request.operation(), &event)
        {
            self.snapshots.lock().map_err(|_| "remote target snapshot lock poisoned".to_string())?.insert(*snapshot_id, label.to_string());
        }
        let response =
            crate::vscomm::RemoteEvent::from_backend_event(request_id, event).to_frame().map_err(|error| format!("encode remote event: {error}"))?;
        write_frame(writer, &response).await
    }

    fn cancel_all(&self) {
        for broker in self.brokers.values() {
            broker.cancel_all();
        }
    }

    fn cleanup_timeout(&self) -> std::time::Duration {
        self.brokers.values().map(|broker| broker.cleanup_timeout()).max().unwrap_or_else(|| RemoteResourcePolicy::default().cleanup_timeout)
    }
}

pub struct RemoteDaemonConfig {
    session: Arc<RunRemoteSession>,
    allowed_tools: Vec<String>,
    tool_policies: Option<Vec<(String, RemoteToolPolicy)>>,
    environment: Option<RemoteEnvironmentPolicy>,
    backend: RemoteBackendSelection,
    resources: RemoteResourcePolicy,
    artifact_policy: ArtifactPolicy,
    artifact_limits: ArtifactLimits,
    admission_limits: RemoteAdmissionLimits,
}

enum RemoteBackendSelection {
    Loopback { tools: std::collections::BTreeMap<String, PathBuf>, target_environment: std::collections::BTreeMap<String, String> },
    Ssh { target: Box<SshTarget> },
}

impl RemoteDaemonConfig {
    pub fn loopback(session: Arc<RunRemoteSession>, allowed_tools: Vec<String>, tools: std::collections::BTreeMap<String, PathBuf>) -> Self {
        Self {
            session,
            allowed_tools,
            tool_policies: None,
            environment: None,
            backend: RemoteBackendSelection::Loopback { tools, target_environment: std::collections::BTreeMap::new() },
            resources: RemoteResourcePolicy::default(),
            artifact_policy: ArtifactPolicy::default(),
            artifact_limits: ArtifactLimits::default(),
            admission_limits: RemoteAdmissionLimits::default(),
        }
    }

    pub fn ssh(session: Arc<RunRemoteSession>, target: SshTarget) -> Result<Self, String> {
        crate::ssh::SshLaunchSpec::from_target(&target)?;
        let target_resources = target.resources();
        Ok(Self {
            session,
            allowed_tools: Vec::new(),
            tool_policies: None,
            environment: None,
            backend: RemoteBackendSelection::Ssh { target: Box::new(target) },
            resources: RemoteResourcePolicy {
                sync_timeout: target_resources.sync_timeout(),
                build_timeout: target_resources.build_timeout(),
                idle_output_timeout: target_resources.idle_output_timeout(),
                cleanup_timeout: target_resources.cleanup_timeout(),
                max_output_bytes: target_resources.max_output_bytes(),
            },
            artifact_policy: ArtifactPolicy::default(),
            artifact_limits: ArtifactLimits::default(),
            admission_limits: RemoteAdmissionLimits::default(),
        })
    }

    pub fn with_policy(mut self, tools: Vec<(String, RemoteToolPolicy)>, environment: RemoteEnvironmentPolicy) -> Self {
        self.tool_policies = Some(tools);
        self.environment = Some(environment);
        self
    }

    pub fn with_target_environment(mut self, environment: std::collections::BTreeMap<String, String>) -> Self {
        if let RemoteBackendSelection::Loopback { target_environment, .. } = &mut self.backend {
            *target_environment = environment;
        }
        self
    }

    pub fn with_resources(mut self, resources: RemoteResourcePolicy) -> Self {
        self.resources = resources;
        self
    }

    pub fn with_artifacts(mut self, policy: ArtifactPolicy, limits: ArtifactLimits) -> Self {
        self.artifact_policy = policy;
        self.artifact_limits = limits;
        self
    }

    pub fn with_admission_limits(mut self, limits: RemoteAdmissionLimits) -> Self {
        self.admission_limits = limits;
        self
    }
}

struct RemoteComponents {
    context: Option<RemoteExecutionContext>,
    policy: Option<RemoteAuthorizationPolicy>,
    backend: Option<Arc<dyn RemoteBackend>>,
    admission_limits: RemoteAdmissionLimits,
    cleanup_timeout: std::time::Duration,
    router: Option<Arc<RemoteRouter>>,
    local_session_id: crate::remote::WorkspaceSessionId,
    legacy_remote: bool,
}

impl VsockDaemon {
    pub fn start_with_remote(
        passthrough: Vec<String>, env_mode: EnvMode, workspace: PathBuf, profiles: Vec<String>, share_dir: PathBuf, allow: Vec<String>,
        remote: RemoteDaemonConfig,
    ) -> Result<Self, String> {
        let RemoteDaemonConfig {
            session,
            allowed_tools,
            tool_policies,
            environment,
            backend,
            resources,
            artifact_policy,
            artifact_limits,
            admission_limits,
        } = remote;
        let remote_policy = match (tool_policies, environment) {
            (Some(tool_policies), Some(environment)) => {
                RemoteAuthorizationPolicy::from_policies(session.target(), session.session_id(), tool_policies, environment)?
            }
            (None, None) => RemoteAuthorizationPolicy::new(session.target(), session.session_id(), allowed_tools),
            _ => return Err("remote tool and environment policies must be configured together".to_string()),
        }
        .with_snapshot_authority(session.clone());
        let remote_context = RemoteExecutionContext { target: session.target(), workspace_session_id: session.session_id() };
        let local_session_id = session.session_id();
        let backend: Arc<dyn RemoteBackend> = match backend {
            RemoteBackendSelection::Loopback { tools, target_environment } => Arc::new(
                LoopbackBackend::new(session, tools)
                    .with_target_environment(target_environment)
                    .with_resources(resources)
                    .with_artifacts(artifact_policy.clone(), artifact_limits),
            ),
            RemoteBackendSelection::Ssh { target } => {
                Arc::new(SshBackend::new(session, *target)?.with_artifacts(artifact_policy.clone(), artifact_limits))
            }
        };
        let remote_components = RemoteComponents {
            context: Some(remote_context),
            policy: Some(remote_policy),
            backend: Some(backend),
            admission_limits,
            cleanup_timeout: resources.cleanup_timeout,
            router: None,
            local_session_id,
            legacy_remote: true,
        };
        Self::start_inner(passthrough, env_mode, workspace, profiles, share_dir, allow, remote_components)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_with_target_catalog(
        passthrough: Vec<String>, env_mode: EnvMode, workspace: PathBuf, profiles: Vec<String>, share_dir: PathBuf, allow: Vec<String>,
        catalog: BuildTargetCatalog, active_target: ActiveBuildTarget, sessions: BTreeMap<String, Arc<RunRemoteSession>>,
        local_session_id: crate::remote::WorkspaceSessionId, global_active: usize,
    ) -> Result<Self, String> {
        let mut brokers = BTreeMap::new();
        let mut cleanup_timeout = RemoteResourcePolicy::default().cleanup_timeout;
        for target in catalog.remote_targets() {
            let label = target.summary().label().to_string();
            let session = sessions.get(&label).cloned().ok_or_else(|| format!("missing remote session for target {label}"))?;
            let environment = RemoteEnvironmentPolicy::from_names(target.project().environment.clone())?;
            let policy = RemoteAuthorizationPolicy::from_policies(session.target(), session.session_id(), target.tool_policies(), environment)?
                .with_snapshot_authority(session.clone());
            let resources = target.target().resources();
            cleanup_timeout = cleanup_timeout.max(resources.cleanup_timeout());
            let backend: Arc<dyn RemoteBackend> = Arc::new(
                SshBackend::new(session.clone(), target.target().clone())?
                    .with_artifacts(target.artifact_policy().clone(), resources.artifact_limits()),
            );
            let target_active = resources.max_active_builds();
            let admission = RemoteAdmissionLimits::new(global_active, target_active)?;
            let context = RemoteExecutionContext { target: session.target(), workspace_session_id: session.session_id() };
            let broker = Arc::new(
                RemoteBroker::new(policy, context, backend).with_admission_limits(admission).with_cleanup_timeout(resources.cleanup_timeout()),
            );
            brokers.insert(label, broker);
        }
        let router = Arc::new(RemoteRouter::new(active_target, brokers));
        let components = RemoteComponents {
            context: None,
            policy: None,
            backend: None,
            admission_limits: RemoteAdmissionLimits::default(),
            cleanup_timeout,
            router: Some(router),
            local_session_id,
            legacy_remote: false,
        };
        Self::start_inner(passthrough, env_mode, workspace, profiles, share_dir, allow, components)
    }

    fn start_inner(
        passthrough: Vec<String>, env_mode: EnvMode, workspace: PathBuf, profiles: Vec<String>, share_dir: PathBuf, allow: Vec<String>,
        remote: RemoteComponents,
    ) -> Result<Self, String> {
        let RemoteComponents {
            context,
            policy,
            backend,
            admission_limits,
            cleanup_timeout,
            router: router_override,
            local_session_id,
            legacy_remote,
        } = remote;
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let merged_profile = if profiles.is_empty() {
            None
        } else {
            let loaded: Vec<_> = profiles.iter().map(|p| resolve_profile(p, &share_dir)).collect::<Result<Vec<_>, _>>()?;
            let host_home = std::env::var_os("HOME").map(PathBuf::from);
            let merged = MergedProfile::from_profiles(&loaded, host_home.as_deref())?;

            let check = std::process::Command::new("bwrap").arg("--version").output().map_err(|e| format!("bwrap not found: {e}"))?;
            if !check.status.success() {
                return Err("bwrap is not functional. Install bubblewrap to use sandbox profiles.".into());
            }

            Some(Arc::new(merged))
        };

        let mut sandbox_proxy: Option<UnixProxyHandle> = None;
        let mut sandbox_proxy_dir: Option<PathBuf> = None;
        let mut proxy_config: Option<SandboxProxyConfig> = None;

        if merged_profile.is_some() && !allow.is_empty() {
            let rt = tokio::runtime::Handle::current();

            let netrelay_path = find_netrelay_binary()?;

            let dir = make_proxy_runtime_dir()?;
            sandbox_proxy_dir = Some(dir.clone());

            let socket_path = dir.join("proxy.sock");
            sandbox_proxy = Some(rt.block_on(FilterProxy::new(allow).bind_unix(&socket_path)).inspect_err(|_| {
                let _ = std::fs::remove_dir(&dir);
            })?);

            proxy_config = Some(SandboxProxyConfig { socket_path, netrelay_path });
        }

        let connections = Arc::new(Mutex::new(Vec::new()));
        let remote_router = router_override.unwrap_or_else(|| {
            let context = context.expect("single remote context is present");
            let policy = policy.expect("single remote policy is present");
            let backend = backend.expect("single remote backend is present");
            let remote_broker =
                Arc::new(RemoteBroker::new(policy, context, backend).with_admission_limits(admission_limits).with_cleanup_timeout(cleanup_timeout));
            let label = if legacy_remote { "legacy-remote" } else { "localhost" };
            let mut brokers = BTreeMap::new();
            brokers.insert(label.to_string(), remote_broker);
            let active = if legacy_remote { ActiveBuildTarget::with_label(label) } else { ActiveBuildTarget::new() };
            Arc::new(RemoteRouter::new(active, brokers))
        });
        let session = Arc::new(VsockSession {
            passthrough: Arc::new(passthrough),
            env_mode,
            workspace,
            merged_profile,
            proxy_config: proxy_config.map(Arc::new),
            remote_router,
            local_session_id,
            local_capabilities: Mutex::new(BTreeMap::new()),
        });

        let listener = tokio_vsock::VsockListener::bind(tokio_vsock::VsockAddr::new(libc::VMADDR_CID_ANY, TOOLCHAIN_PORT)).map_err(|error| {
            if let Some(h) = sandbox_proxy.take() {
                h.stop();
            }
            if let Some(d) = sandbox_proxy_dir.take() {
                let _ = std::fs::remove_dir_all(&d);
            }
            format!("failed to bind toolchain vsock port {TOOLCHAIN_PORT}: {error}")
        })?;

        let join_handle = tokio::spawn(async move {
            let result = daemon_loop(session, listener, shutdown_rx, connections).await;
            if let Err(err) = result {
                logging::diagnostic(&format!("bunkerbox: vsock daemon: {err}"));
            }
        });

        Ok(Self { join_handle, shutdown: shutdown_tx, sandbox_proxy, sandbox_proxy_dir })
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.join_handle.await;
        if let Some(h) = self.sandbox_proxy {
            h.stop();
        }
        if let Some(d) = self.sandbox_proxy_dir {
            let _ = std::fs::remove_dir_all(&d);
        }
    }
}

async fn daemon_loop(
    session: Arc<VsockSession>, listener: tokio_vsock::VsockListener, mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    connections: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
) -> Result<(), String> {
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _peer)) => {
                        let session = session.clone();
                        let connection = tokio::spawn(async move {
                            if let Err(err) = handle_connection(stream, &session).await {
                                logging::diagnostic(&format!("bunkerbox: toolchain vsock session failed: {err}"));
                            }
                        });
                        let mut active = connections.lock().map_err(|_| "connection task lock poisoned".to_string())?;
                        active.retain(|task| !task.is_finished());
                        active.push(connection);
                    }
                    Err(e) => {
                        logging::diagnostic(&format!("bunkerbox: vsock accept error: {e}"));
                    }
                }
            }
            _ = &mut shutdown_rx => {
                break;
            }
        }
    }

    session.remote_router.cancel_all();
    let tasks = connections.lock().map_err(|_| "connection task lock poisoned".to_string())?.drain(..).collect::<Vec<_>>();
    let cleanup_deadline = session.remote_router.cleanup_timeout();
    let mut tasks = tasks;
    if tokio::time::timeout(cleanup_deadline, async {
        for task in &mut tasks {
            let _ = task.await;
        }
    })
    .await
    .is_err()
    {
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
    }

    Ok(())
}

async fn handle_connection(stream: tokio_vsock::VsockStream, session: &VsockSession) -> Result<(), String> {
    let (mut reader, mut writer) = tokio::io::split(stream);

    let frame = Frame::read_async(&mut reader).await.map_err(|e| format!("read frame: {e}"))?;
    if matches!(frame.frame_type, FrameType::RemoteRequest) {
        return dispatch_remote_frame_for_session(frame, session, &mut writer).await;
    }
    if !matches!(frame.frame_type, FrameType::ExecReq) {
        return Err(format!("expected ExecReq or RemoteRequest, got {:?}", frame.frame_type as u16));
    }
    let req = ExecRequest::deserialize(&frame.payload)?;

    if let Err(err) = validate_exec_request(&req) {
        logging::diagnostic(&format!("bunkerbox-vscomm: invalid request: {err}"));
        write_frame(&mut writer, &Frame::new(FrameType::Exit, 1i32.to_le_bytes().to_vec())).await?;
        return Ok(());
    }

    let command = req.command.clone();
    if let Err(err) = dispatch_exec_request_for_session(&req, session, &mut writer).await {
        logging::diagnostic(&format!("bunkerbox: toolchain command '{command}' failed: {err}"));
        let _ = write_frame(&mut writer, &Frame::new(FrameType::Exit, 1i32.to_le_bytes().to_vec())).await;
        return Ok(());
    }

    Ok(())
}

async fn dispatch_exec_request_for_session<W: AsyncWriteExt + Unpin>(
    req: &ExecRequest, session: &VsockSession, writer: &mut W,
) -> Result<(), String> {
    if session.remote_router.is_remote_selected() {
        return session.remote_router.dispatch_transparent_exec(req, session.local_session_id, writer).await;
    }

    if !is_allowed(&session.passthrough, &req.command, &req.args) {
        logging::diagnostic(&format!("bunkerbox-vscomm: command '{}' not whitelisted", req.command));
        write_frame(writer, &Frame::new(FrameType::Exit, 1i32.to_le_bytes().to_vec())).await?;
        return Ok(());
    }

    execute_request(writer, session, req).await
}

fn new_remote_request_id() -> RequestId {
    let mut bytes = [0u8; 16];
    loop {
        rand::thread_rng().fill_bytes(&mut bytes);
        if bytes != [0; 16] {
            return RequestId(bytes);
        }
    }
}

async fn dispatch_remote_frame_for_session<W: AsyncWriteExt + Unpin>(frame: Frame, session: &VsockSession, writer: &mut W) -> Result<(), String> {
    let request = crate::vscomm::RemoteRequest::from_frame(frame).map_err(|err| format!("decode remote request: {err}"))?.into_domain()?;
    let local_snapshot = match request.operation() {
        crate::remote::RemoteOperation::Build(build) if session.remote_router.active_target.current() == "localhost" => {
            session.local_capabilities.lock().map_err(|_| "local target capability lock poisoned".to_string())?.contains_key(&build.snapshot_id())
        }
        _ => false,
    };
    match request.operation() {
        crate::remote::RemoteOperation::Sync(_) if session.remote_router.active_target.current() == "localhost" => {
            dispatch_local_remote_request(request, session, writer).await
        }
        crate::remote::RemoteOperation::Build(_) if local_snapshot => dispatch_local_remote_request(request, session, writer).await,
        _ => session.remote_router.dispatch(request, writer).await,
    }
}

async fn dispatch_local_remote_request<W: AsyncWriteExt + Unpin>(
    request: RemoteRequest, session: &VsockSession, writer: &mut W,
) -> Result<(), String> {
    let request_id = request.request_id();
    if request.workspace_session_id() != session.local_session_id {
        return write_local_remote_event(
            writer,
            request_id,
            RemoteBackendEvent::Error { message: "local remote session does not match this sandbox".to_string() },
        )
        .await;
    }
    match request.operation() {
        crate::remote::RemoteOperation::Sync(sync) => {
            if sync.retain_capability() {
                let snapshot_id = loop {
                    let mut bytes = [0u8; 16];
                    rand::thread_rng().fill_bytes(&mut bytes);
                    let candidate = crate::remote::RemoteSnapshotId::from_bytes(bytes);
                    if !candidate.is_zero()
                        && !session
                            .local_capabilities
                            .lock()
                            .map_err(|_| "local target capability lock poisoned".to_string())?
                            .contains_key(&candidate)
                    {
                        break candidate;
                    }
                };
                session.local_capabilities.lock().map_err(|_| "local target capability lock poisoned".to_string())?.insert(snapshot_id, ());
                write_local_remote_event(writer, request_id, RemoteBackendEvent::SyncCompleted { snapshot_id }).await
            } else {
                write_local_remote_event(writer, request_id, RemoteBackendEvent::Completed { exit_code: 0 }).await
            }
        }
        crate::remote::RemoteOperation::Build(build) => {
            let snapshot_id = build.snapshot_id();
            let available =
                session.local_capabilities.lock().map_err(|_| "local target capability lock poisoned".to_string())?.remove(&snapshot_id).is_some();
            if !available {
                return write_local_remote_event(
                    writer,
                    request_id,
                    RemoteBackendEvent::Error { message: "remote snapshot capability is unavailable".to_string() },
                )
                .await;
            }
            if !is_allowed(&session.passthrough, build.tool().as_str(), build.argv()) {
                return write_local_remote_event(
                    writer,
                    request_id,
                    RemoteBackendEvent::Error { message: "local passthrough authorization rejected".to_string() },
                )
                .await;
            }
            let exec_request = ExecRequest {
                cwd: guest_workspace_cwd(build.cwd())
                    .into_os_string()
                    .into_string()
                    .map_err(|_| "local workspace cwd is not valid UTF-8".to_string())?,
                command: build.tool().as_str().to_string(),
                args: build.argv().to_vec(),
                env: build.env().to_vec(),
            };
            let (mut frame_reader, mut frame_writer) = tokio::io::duplex(64 * 1024);
            let mut execute = Box::pin(execute_request(&mut frame_writer, session, &exec_request));
            loop {
                tokio::select! {
                    result = &mut execute => {
                        if let Err(error) = result {
                            write_local_remote_event(writer, request_id, RemoteBackendEvent::Error { message: error }).await?;
                        }
                        break;
                    }
                    frame = Frame::read_async(&mut frame_reader) => {
                        let frame = frame.map_err(|error| format!("read local execution frame: {error}"))?;
                        match frame.frame_type {
                            FrameType::Stdout => write_local_remote_event(writer, request_id, RemoteBackendEvent::Stdout(frame.payload)).await?,
                            FrameType::Stderr => write_local_remote_event(writer, request_id, RemoteBackendEvent::Stderr(frame.payload)).await?,
                            FrameType::Exit => {
                                if frame.payload.len() != 4 {
                                    return Err("local execution returned malformed exit frame".to_string());
                                }
                                let exit_code = i32::from_le_bytes([frame.payload[0], frame.payload[1], frame.payload[2], frame.payload[3]]);
                                write_local_remote_event(writer, request_id, RemoteBackendEvent::Completed { exit_code }).await?;
                                break;
                            }
                            _ => return Err("local execution returned an unexpected frame".to_string()),
                        }
                    }
                }
            }
            Ok(())
        }
        crate::remote::RemoteOperation::Cancel { .. } => {
            write_local_remote_event(
                writer,
                request_id,
                RemoteBackendEvent::Error { message: "local target cancellation is unavailable".to_string() },
            )
            .await
        }
    }
}

fn guest_workspace_cwd(cwd: &WorkspaceRelativePath) -> PathBuf {
    if cwd.as_str().is_empty() {
        PathBuf::from("/workspace")
    } else {
        Path::new("/workspace").join(cwd.as_str())
    }
}

async fn write_local_remote_event<W: AsyncWriteExt + Unpin>(
    writer: &mut W, request_id: crate::remote::RequestId, event: RemoteBackendEvent,
) -> Result<(), String> {
    let response = crate::vscomm::RemoteEvent::from_backend_event(request_id, event)
        .to_frame()
        .map_err(|error| format!("encode local remote event: {error}"))?;
    write_frame(writer, &response).await
}

pub async fn dispatch_remote_frame<W: AsyncWriteExt + Unpin>(frame: Frame, broker: &RemoteBroker, writer: &mut W) -> Result<(), String> {
    let request = crate::vscomm::RemoteRequest::from_frame(frame).map_err(|err| format!("decode remote request: {err}"))?.into_domain()?;
    let request_id = request.request_id();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(64);
    let mut dispatch = Box::pin(broker.dispatch(request, event_tx));
    let mut dispatch_result: Option<Result<(), RemoteDispatchError>> = None;

    loop {
        if dispatch_result.is_some() {
            let Some(event) = event_rx.recv().await else {
                let result = dispatch_result.take().expect("dispatch result is present");
                return result.map_err(|err| format!("remote dispatch failed: {err:?}"));
            };
            let response =
                crate::vscomm::RemoteEvent::from_backend_event(request_id, event).to_frame().map_err(|err| format!("encode remote event: {err}"))?;
            if let Err(error) = write_frame(writer, &response).await {
                broker.cancel_request(request_id);
                return Err(error);
            }
            continue;
        }

        tokio::select! {
            result = &mut dispatch => dispatch_result = Some(result),
            event = event_rx.recv() => {
                let Some(event) = event else { return Err("remote event stream closed before backend completion".to_string()) };
                let response = crate::vscomm::RemoteEvent::from_backend_event(request_id, event)
                    .to_frame()
                    .map_err(|err| format!("encode remote event: {err}"))?;
                if let Err(error) = write_frame(writer, &response).await {
                    broker.cancel_request(request_id);
                    return Err(error);
                }
            }
        }
    }
}

async fn execute_request<W: AsyncWriteExt + Unpin>(writer: &mut W, session: &VsockSession, req: &ExecRequest) -> Result<(), String> {
    validate_exec_request(req)?;
    let cwd = WorkspaceCwd::resolve(&session.workspace, Path::new(&req.cwd))?;

    let (status_reader, status_writer) = if session.merged_profile.is_some() {
        let (reader, writer) = bwrap_status_pipe()?;
        (Some(reader), Some(writer))
    } else {
        (None, None)
    };
    let mut cmd = build_command(session, req, &cwd)?;
    if let Some(status_writer) = status_writer.as_ref() {
        attach_bwrap_status_fd(&mut cmd, status_writer.as_raw_fd());
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let launcher = if session.merged_profile.is_some() { "bwrap" } else { &req.command };
    let mut child = cmd.spawn().map_err(|e| format!("spawn {launcher} for command '{}': {e}", req.command))?;
    drop(status_writer);

    let child_stdout = child.stdout.take().ok_or_else(|| "no stdout".to_string())?;
    let child_stderr = child.stderr.take().ok_or_else(|| "no stderr".to_string())?;

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(pump_to_channel(child_stdout, ProcessStream::Stdout, event_tx.clone()));
    let stderr_task = tokio::spawn(pump_to_channel(child_stderr, ProcessStream::Stderr, event_tx.clone()));
    let status_task = status_reader.map(|reader| {
        let status_tx = event_tx.clone();
        tokio::task::spawn_blocking(move || monitor_bwrap_status(reader, status_tx))
    });
    drop(event_tx);
    let mut launcher_started = session.merged_profile.is_none();
    let mut launcher_failed = false;
    let mut closed_streams = 0;
    let mut buffered_output = Vec::new();

    while closed_streams < 2 || (session.merged_profile.is_some() && !launcher_started && !launcher_failed) {
        let Some(event) = event_rx.recv().await else { break };
        match event {
            ChildEvent::Output(stream_kind, data) if launcher_failed => {
                let stream = if matches!(stream_kind, ProcessStream::Stdout) { "bwrap stdout" } else { "bwrap stderr" };
                logging::diagnostic_bytes(stream, &data);
            }
            ChildEvent::Output(stream_kind, data) if launcher_started => {
                write_frame(writer, &Frame::new(process_stream_frame_type(stream_kind), data)).await?;
            }
            ChildEvent::Output(stream_kind, data) => buffered_output.push((stream_kind, data)),
            ChildEvent::StreamClosed => closed_streams += 1,
            ChildEvent::LauncherStarted => {
                launcher_started = true;
                for (stream_kind, data) in buffered_output.drain(..) {
                    write_frame(writer, &Frame::new(process_stream_frame_type(stream_kind), data)).await?;
                }
            }
            ChildEvent::LauncherFailed(err) => {
                launcher_failed = true;
                logging::diagnostic(&format!("bwrap setup failed: {err}"));
                for (stream_kind, data) in buffered_output.drain(..) {
                    let stream = if matches!(stream_kind, ProcessStream::Stdout) { "bwrap stdout" } else { "bwrap stderr" };
                    logging::diagnostic_bytes(stream, &data);
                }
            }
        }
    }

    let status = child.wait().await.map_err(|e| format!("wait {}: {e}", req.command))?;
    let exit_code = status.code().unwrap_or(-1);
    write_frame(writer, &Frame::new(FrameType::Exit, exit_code.to_le_bytes().to_vec())).await?;

    stdout_task.await.map_err(|e| format!("stdout task: {e}"))?;
    stderr_task.await.map_err(|e| format!("stderr task: {e}"))?;
    if let Some(status_task) = status_task {
        status_task.await.map_err(|e| format!("bwrap status task: {e}"))?;
    }

    Ok(())
}

fn build_command(session: &VsockSession, req: &ExecRequest, cwd: &WorkspaceCwd) -> Result<Command, String> {
    validate_exec_request(req)?;
    validate_process_path("workspace path", &session.workspace)?;
    validate_process_path("host working directory", cwd.host_path())?;
    let sandbox_cwd = cwd.guest_path();
    validate_process_path("sandbox working directory", &sandbox_cwd)?;
    if let Some(ref merged) = session.merged_profile {
        let mut cmd = Command::new("bwrap");

        cmd.arg("--bind").arg(&session.workspace).arg("/workspace");

        for (name, host_path) in &merged.bin {
            let resolved = if host_path.exists() {
                host_path.clone()
            } else if let Some(found) = find_in_path(name) {
                found
            } else {
                logging::diagnostic(&format!("bunkerbox: warning: binary '{name}' not found, skipping"));
                continue;
            };
            let dest = PathBuf::from("/usr/bin").join(name);
            cmd.arg("--ro-bind").arg(&resolved).arg(&dest);
        }

        cmd.arg("--tmpfs").arg("/home");
        for path in &merged.paths {
            if !path.source.exists() {
                logging::diagnostic(&format!("bunkerbox: warning: profile path '{}' not found, skipping", path.source.display()));
                continue;
            }
            if path.writable {
                cmd.arg("--bind").arg(&path.source).arg(&path.destination);
            } else {
                cmd.arg("--ro-bind").arg(&path.source).arg(&path.destination);
            }
        }

        if let Ok(resolved) = which_sh(&merged.shell) {
            cmd.arg("--ro-bind").arg(&resolved).arg("/bin/sh");
        }

        if matches!(merged.network, NetworkMode::None) {
            cmd.arg("--unshare-net");
        }

        cmd.arg("--unshare-pid");
        cmd.arg("--unshare-user");
        cmd.arg("--uid").arg("0");
        cmd.arg("--gid").arg("0");

        cmd.arg("--proc").arg("/proc");
        cmd.arg("--dev").arg("/dev");
        cmd.arg("--tmpfs").arg("/tmp");

        if sandbox_cwd != Path::new("/") {
            cmd.arg("--dir").arg(&sandbox_cwd);
        }
        cmd.arg("--chdir").arg(&sandbox_cwd);

        cmd.arg("--clearenv");
        cmd.arg("--setenv").arg("PATH").arg("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        cmd.arg("--setenv").arg("HOME").arg("/home");

        if let Some(ref cfg) = session.proxy_config {
            cmd.arg("--dir").arg("/run/bunkerbox");
            cmd.arg("--ro-bind").arg(&cfg.netrelay_path).arg("/run/bunkerbox/netrelay");
            cmd.arg("--ro-bind").arg(&cfg.socket_path).arg("/run/bunkerbox/proxy.sock");
        }

        if session.proxy_config.is_some() {
            let proxy_url = "http://127.0.0.1:20000";
            cmd.arg("--setenv").arg("HTTP_PROXY").arg(proxy_url);
            cmd.arg("--setenv").arg("HTTPS_PROXY").arg(proxy_url);
            cmd.arg("--setenv").arg("http_proxy").arg(proxy_url);
            cmd.arg("--setenv").arg("https_proxy").arg(proxy_url);
        }

        for (key, val) in &merged.env {
            cmd.arg("--setenv").arg(key).arg(val);
        }

        if session.env_mode == EnvMode::Relaxed {
            for (key, val) in &req.env {
                if key == "PATH"
                    || key == "HOME"
                    || key == "VSOCK_CID"
                    || key == "HTTP_PROXY"
                    || key == "HTTPS_PROXY"
                    || key == "http_proxy"
                    || key == "https_proxy"
                    || key.starts_with("BUNKERBOX_")
                    || key.starts_with("XDG_")
                {
                    continue;
                }
                cmd.arg("--setenv").arg(key).arg(val);
            }
        }

        cmd.arg("--json-status-fd").arg(BWRAP_STATUS_FD.to_string());
        cmd.arg("--");
        if session.proxy_config.is_some() {
            cmd.arg("/run/bunkerbox/netrelay");
            cmd.arg("--socket");
            cmd.arg("/run/bunkerbox/proxy.sock");
            cmd.arg("--");
        }
        cmd.arg(&req.command);
        for arg in &req.args {
            cmd.arg(arg);
        }

        Ok(cmd)
    } else {
        let mut cmd = Command::new(&req.command);
        cmd.args(&req.args);
        cmd.current_dir(cwd.host_path());

        if session.env_mode == EnvMode::Relaxed {
            for (key, val) in &req.env {
                if key == "PATH"
                    || key == "HOME"
                    || key == "VSOCK_CID"
                    || key == "HTTP_PROXY"
                    || key == "HTTPS_PROXY"
                    || key == "http_proxy"
                    || key == "https_proxy"
                    || key.starts_with("BUNKERBOX_")
                    || key.starts_with("XDG_")
                {
                    continue;
                }
                cmd.env(key, val);
            }
        }

        Ok(cmd)
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).find_map(|dir| {
            let candidate = dir.join(name);
            candidate.is_file().then_some(candidate)
        })
    })
}

fn which_sh(shell: &Path) -> Result<PathBuf, String> {
    if shell.exists() {
        return Ok(shell.to_path_buf());
    }
    if let Some(name) = shell.file_name().and_then(|n| n.to_str()) {
        if let Some(found) = find_in_path(name) {
            return Ok(found);
        }
    }
    Err(format!("shell not found: {}", shell.display()))
}

fn is_allowed(passthrough: &[String], command: &str, args: &[String]) -> bool {
    for entry in passthrough {
        let entry = entry.trim();
        if let Some(cmd) = entry.strip_suffix(" *") {
            if cmd.trim() == command {
                return true;
            }
        } else {
            let full = if args.is_empty() { command.to_string() } else { format!("{} {}", command, args.join(" ")) };
            if entry == full {
                return true;
            }
        }
    }
    false
}

fn bwrap_status_pipe() -> Result<(File, File), String> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("create bwrap status pipe: {}", std::io::Error::last_os_error()));
    }

    let reader = unsafe { File::from_raw_fd(fds[0]) };
    let writer = unsafe { File::from_raw_fd(fds[1]) };
    Ok((reader, writer))
}

fn attach_bwrap_status_fd(cmd: &mut Command, source_fd: RawFd) {
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(source_fd, BWRAP_STATUS_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if source_fd != BWRAP_STATUS_FD {
                libc::close(source_fd);
            }
            Ok(())
        });
    }
}

fn monitor_bwrap_status(reader: File, tx: tokio::sync::mpsc::UnboundedSender<ChildEvent>) {
    let mut started = false;
    for line in BufReader::new(reader).lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                if !started {
                    let _ = tx.send(ChildEvent::LauncherFailed(format!("read status: {err}")));
                }
                return;
            }
        };

        let status: serde_json::Value = match serde_json::from_str(&line) {
            Ok(status) => status,
            Err(err) => {
                if !started {
                    let _ = tx.send(ChildEvent::LauncherFailed(format!("invalid status JSON: {err}")));
                }
                return;
            }
        };

        if status.get("child-pid").is_some() && !started {
            started = true;
            let _ = tx.send(ChildEvent::LauncherStarted);
        } else if !started {
            if let Some(exit_code) = status.get("exit-code") {
                let _ = tx.send(ChildEvent::LauncherFailed(format!("exited before command start with status {exit_code}")));
                return;
            }
        }
    }

    if !started {
        let _ = tx.send(ChildEvent::LauncherFailed("exited before command start".to_string()));
    }
}

async fn pump_to_channel<R: AsyncReadExt + Unpin>(mut reader: R, stream_kind: ProcessStream, tx: tokio::sync::mpsc::UnboundedSender<ChildEvent>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(ChildEvent::Output(stream_kind, buf[..n].to_vec())).is_err() {
                    return;
                }
            }
            Err(_) => break,
        }
    }
    let _ = tx.send(ChildEvent::StreamClosed);
}

fn process_stream_frame_type(stream: ProcessStream) -> FrameType {
    match stream {
        ProcessStream::Stdout => FrameType::Stdout,
        ProcessStream::Stderr => FrameType::Stderr,
    }
}

async fn write_frame<W: AsyncWriteExt + Unpin>(writer: &mut W, frame: &Frame) -> Result<(), String> {
    frame.write_async(writer).await.map_err(|e| format!("write frame: {e}"))
}

fn find_netrelay_binary() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("locate self: {e}"))?;
    let dir = exe.parent().ok_or("no binary directory")?;
    let sibling = dir.join("bunkerbox-netrelay");
    if sibling.is_file() {
        return Ok(sibling);
    }
    Err("bunkerbox-netrelay not found. Run: make dev".into())
}

fn make_proxy_runtime_dir() -> Result<PathBuf, String> {
    let mut rng = rand::thread_rng();
    let base = std::env::temp_dir();
    for _ in 0..10 {
        let random: u32 = rng.gen();
        let path = base.join(format!("bunkerbox-daemon-{}-{:08x}", std::process::id(), random));
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("mkdir {}: {e}", path.display())),
        }
    }
    Err("failed to create exclusive proxy runtime directory after 10 attempts".to_string())
}

#[cfg(test)]
#[path = "daemon_ut.rs"]
mod daemon_tests;
