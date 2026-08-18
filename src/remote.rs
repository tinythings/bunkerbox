#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::Notify;

pub const MAX_REMOTE_STRING_BYTES: usize = 4 * 1024;
pub const MAX_REMOTE_TOOL_BYTES: usize = 256;
pub const MAX_REMOTE_ARG_COUNT: usize = 256;
pub const MAX_REMOTE_ARG_BYTES: usize = 4 * 1024;
pub const MAX_REMOTE_ENV_COUNT: usize = 64;
pub const MAX_REMOTE_ENV_KEY_BYTES: usize = 256;
pub const MAX_REMOTE_ENV_VALUE_BYTES: usize = 4 * 1024;
pub const DEFAULT_REMOTE_BUILD_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_REMOTE_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_REMOTE_ENVIRONMENT: &[&str] = &["CC", "CXX", "AR", "RUSTFLAGS", "CFLAGS", "CXXFLAGS", "MAKEFLAGS"];
pub const REMOTE_WRAPPER_STATE_FILE: &str = ".bunkerbox-remote-tools";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub [u8; 16]);

impl RequestId {
    pub fn is_zero(self) -> bool {
        self.0 == [0; 16]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RemoteSnapshotId([u8; 16]);

impl RemoteSnapshotId {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    pub fn is_zero(self) -> bool {
        self.0 == [0; 16]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceSessionId(pub [u8; 16]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RemoteTargetId(pub [u8; 16]);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRelativePath(String);

impl WorkspaceRelativePath {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_string("remote cwd", &value, MAX_REMOTE_STRING_BYTES)?;
        if value.is_empty() {
            return Ok(Self(value));
        }

        if value.starts_with('/') || value.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
            return Err("remote cwd must be a normalized relative path".to_string());
        }

        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTool(String);

impl RemoteTool {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_string("remote tool", &value, MAX_REMOTE_TOOL_BYTES)?;
        if value.is_empty()
            || value == "."
            || value == ".."
            || !value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'+'))
        {
            return Err("remote tool must be a single executable identity".to_string());
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub fn validate_remote_wrapper_name(name: impl Into<String>) -> Result<String, String> {
    let name = name.into();
    RemoteTool::new(name.clone())?;
    if name == "bunkerbox" || name.starts_with("bunkerbox-") || name.starts_with(REMOTE_WRAPPER_STATE_FILE) {
        return Err(format!("remote wrapper name is reserved: {name}"));
    }
    Ok(name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBuild {
    cwd: WorkspaceRelativePath,
    tool: RemoteTool,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    snapshot_id: RemoteSnapshotId,
}

impl RemoteBuild {
    pub fn new(
        cwd: WorkspaceRelativePath, tool: RemoteTool, argv: Vec<String>, env: Vec<(String, String)>, snapshot_id: RemoteSnapshotId,
    ) -> Result<Self, String> {
        validate_count(argv.len(), MAX_REMOTE_ARG_COUNT, "remote argv")?;
        argv.iter().try_for_each(|arg| validate_string("remote argument", arg, MAX_REMOTE_ARG_BYTES))?;
        validate_count(env.len(), MAX_REMOTE_ENV_COUNT, "remote environment")?;
        env.iter().try_for_each(|(key, value)| {
            validate_environment_key(key)?;
            validate_environment_value(value)
        })?;
        if snapshot_id.is_zero() {
            return Err("remote snapshot ID must be nonzero".to_string());
        }
        Ok(Self { cwd, tool, argv, env, snapshot_id })
    }

    pub fn cwd(&self) -> &WorkspaceRelativePath {
        &self.cwd
    }

    pub fn tool(&self) -> &RemoteTool {
        &self.tool
    }

    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    pub fn snapshot_id(&self) -> RemoteSnapshotId {
        self.snapshot_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteResourcePolicy {
    pub sync_timeout: Duration,
    pub build_timeout: Duration,
    pub idle_output_timeout: Duration,
    pub cleanup_timeout: Duration,
    pub max_output_bytes: u64,
}

impl Default for RemoteResourcePolicy {
    fn default() -> Self {
        Self {
            sync_timeout: Duration::from_secs(60),
            build_timeout: DEFAULT_REMOTE_BUILD_TIMEOUT,
            idle_output_timeout: Duration::from_secs(5 * 60),
            cleanup_timeout: Duration::from_secs(5),
            max_output_bytes: DEFAULT_REMOTE_OUTPUT_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEnvironmentPolicy {
    allowed: BTreeSet<String>,
}

impl Default for RemoteEnvironmentPolicy {
    fn default() -> Self {
        Self { allowed: DEFAULT_REMOTE_ENVIRONMENT.iter().map(|name| (*name).to_string()).collect() }
    }
}

impl RemoteEnvironmentPolicy {
    pub fn from_names(names: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut policy = Self::default();
        let mut configured = BTreeSet::new();
        for name in names {
            validate_environment_key(&name)?;
            if forbidden_environment_name(&name) {
                return Err(format!("remote environment variable is forbidden: {name}"));
            }
            if !configured.insert(name.clone()) {
                return Err(format!("duplicate remote environment variable: {name}"));
            }
            policy.allowed.insert(name);
        }
        Ok(policy)
    }

    pub fn allows(&self, name: &str) -> bool {
        self.allowed.contains(name)
    }

    pub fn allowed_names(&self) -> impl Iterator<Item = &str> {
        self.allowed.iter().map(String::as_str)
    }

    fn filter(&self, environment: &[(String, String)]) -> Result<Vec<(String, String)>, RemoteAuthorizationError> {
        let mut seen = BTreeSet::new();
        let mut filtered = Vec::with_capacity(environment.len());
        for (key, value) in environment {
            validate_environment_key(key).map_err(RemoteAuthorizationError::InvalidEnvironment)?;
            validate_environment_value(value).map_err(RemoteAuthorizationError::InvalidEnvironment)?;
            if forbidden_environment_name(key) {
                return Err(RemoteAuthorizationError::ForbiddenEnvironment(key.clone()));
            }
            if !self.allowed.contains(key) {
                return Err(RemoteAuthorizationError::EnvironmentNotAllowed(key.clone()));
            }
            if !seen.insert(key.clone()) {
                return Err(RemoteAuthorizationError::DuplicateEnvironment(key.clone()));
            }
            filtered.push((key.clone(), value.clone()));
        }
        Ok(filtered)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteToolPolicy {
    allow_arbitrary_argv: bool,
    command: Option<String>,
}

impl RemoteToolPolicy {
    pub fn new(allow_arbitrary_argv: bool) -> Self {
        Self { allow_arbitrary_argv, command: None }
    }

    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.command = Some(command.into());
        self
    }

    pub fn allows_arbitrary_argv(&self) -> bool {
        self.allow_arbitrary_argv
    }

    pub fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteSync {
    retain_capability: bool,
}

impl RemoteSync {
    pub fn retain_capability(self) -> bool {
        self.retain_capability
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteAdmissionLimits {
    pub global_active: usize,
    pub target_active: usize,
}

impl Default for RemoteAdmissionLimits {
    fn default() -> Self {
        Self { global_active: 2, target_active: 1 }
    }
}

impl RemoteAdmissionLimits {
    pub const MAX: usize = 64;

    pub fn new(global_active: usize, target_active: usize) -> Result<Self, String> {
        if global_active == 0 || target_active == 0 {
            return Err("remote active-build limits must be positive".to_string());
        }
        if global_active > Self::MAX || target_active > Self::MAX {
            return Err(format!("remote active-build limits must not exceed {}", Self::MAX));
        }
        Ok(Self { global_active, target_active })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteLifecyclePhase {
    Admitting,
    Syncing,
    Connecting,
    Transferring,
    Building,
    ArtifactHandling,
    Cancelling,
    Cleanup,
    Finalizing,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteTimeoutCause {
    Connection,
    Sync,
    Build,
    IdleOutput,
    Artifact,
    Cleanup,
}

impl RemoteTimeoutCause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::Sync => "sync",
            Self::Build => "build",
            Self::IdleOutput => "idle output",
            Self::Artifact => "artifact",
            Self::Cleanup => "cleanup",
        }
    }
}

#[derive(Clone)]
pub struct RemoteCancellation {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Default for RemoteCancellation {
    fn default() -> Self {
        Self { cancelled: Arc::new(AtomicBool::new(false)), notify: Arc::new(Notify::new()) }
    }
}

impl RemoteCancellation {
    pub fn cancel(&self) -> bool {
        if self.cancelled.swap(true, Ordering::AcqRel) {
            false
        } else {
            self.notify.notify_waiters();
            true
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        while !self.is_cancelled() {
            self.notify.notified().await;
        }
    }
}

#[derive(Clone)]
pub struct RemoteExecutionControl {
    cancellation: RemoteCancellation,
    phase: Arc<AtomicU8>,
}

impl Default for RemoteExecutionControl {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteExecutionControl {
    pub fn new() -> Self {
        Self { cancellation: RemoteCancellation::default(), phase: Arc::new(AtomicU8::new(0)) }
    }

    pub fn cancel(&self) -> bool {
        self.cancellation.cancel()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    pub fn phase(&self) -> RemoteLifecyclePhase {
        match self.phase.load(Ordering::Acquire) {
            1 => RemoteLifecyclePhase::Syncing,
            2 => RemoteLifecyclePhase::Connecting,
            3 => RemoteLifecyclePhase::Transferring,
            4 => RemoteLifecyclePhase::Building,
            5 => RemoteLifecyclePhase::ArtifactHandling,
            6 => RemoteLifecyclePhase::Cancelling,
            7 => RemoteLifecyclePhase::Cleanup,
            8 => RemoteLifecyclePhase::Finalizing,
            9 => RemoteLifecyclePhase::Terminal,
            _ => RemoteLifecyclePhase::Admitting,
        }
    }

    pub fn set_phase(&self, phase: RemoteLifecyclePhase) {
        let value = match phase {
            RemoteLifecyclePhase::Admitting => 0,
            RemoteLifecyclePhase::Syncing => 1,
            RemoteLifecyclePhase::Connecting => 2,
            RemoteLifecyclePhase::Transferring => 3,
            RemoteLifecyclePhase::Building => 4,
            RemoteLifecyclePhase::ArtifactHandling => 5,
            RemoteLifecyclePhase::Cancelling => 6,
            RemoteLifecyclePhase::Cleanup => 7,
            RemoteLifecyclePhase::Finalizing => 8,
            RemoteLifecyclePhase::Terminal => 9,
        };
        self.phase.fetch_max(value, Ordering::AcqRel);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteOperation {
    Sync(RemoteSync),
    Build(RemoteBuild),
    Cancel { target_request_id: RequestId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRequest {
    request_id: RequestId,
    workspace_session_id: WorkspaceSessionId,
    operation: RemoteOperation,
}

impl RemoteRequest {
    pub fn sync(request_id: RequestId, workspace_session_id: WorkspaceSessionId) -> Self {
        Self::sync_with_capability(request_id, workspace_session_id, true)
    }

    pub fn diagnostic_sync(request_id: RequestId, workspace_session_id: WorkspaceSessionId) -> Self {
        Self::sync_with_capability(request_id, workspace_session_id, false)
    }

    fn sync_with_capability(request_id: RequestId, workspace_session_id: WorkspaceSessionId, retain_capability: bool) -> Self {
        Self { request_id, workspace_session_id, operation: RemoteOperation::Sync(RemoteSync { retain_capability }) }
    }

    pub fn build(request_id: RequestId, workspace_session_id: WorkspaceSessionId, build: RemoteBuild) -> Self {
        Self { request_id, workspace_session_id, operation: RemoteOperation::Build(build) }
    }

    pub fn cancel(request_id: RequestId, workspace_session_id: WorkspaceSessionId, target_request_id: RequestId) -> Self {
        Self { request_id, workspace_session_id, operation: RemoteOperation::Cancel { target_request_id } }
    }

    pub fn request_id(&self) -> RequestId {
        self.request_id
    }

    pub fn workspace_session_id(&self) -> WorkspaceSessionId {
        self.workspace_session_id
    }

    pub fn operation(&self) -> &RemoteOperation {
        &self.operation
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteExecutionContext {
    pub target: RemoteTargetId,
    pub workspace_session_id: WorkspaceSessionId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedRemoteRequest {
    request: RemoteRequest,
    target: RemoteTargetId,
    target_command: Option<String>,
}

impl AuthorizedRemoteRequest {
    pub fn request_id(&self) -> RequestId {
        self.request.request_id
    }

    pub fn target(&self) -> RemoteTargetId {
        self.target
    }

    pub fn request(&self) -> &RemoteRequest {
        &self.request
    }

    pub fn target_command(&self) -> Option<&str> {
        self.target_command.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAuthorizationError {
    InvalidRequestId,
    InvalidCancelTarget,
    CancelTargetUnavailable,
    CancelTargetFinalizing,
    SessionMismatch,
    TargetNotAllowed,
    ToolNotAllowed(String),
    ToolArgumentsNotAllowed(String),
    InvalidEnvironment(String),
    ForbiddenEnvironment(String),
    EnvironmentNotAllowed(String),
    DuplicateEnvironment(String),
    SnapshotNotAllowed,
}

impl RemoteAuthorizationError {
    pub fn event(&self) -> RemoteBackendEvent {
        RemoteBackendEvent::Error { message: format!("remote authorization rejected: {self:?}") }
    }
}

#[derive(Clone)]
pub struct RemoteAuthorizationPolicy {
    allowed_target: RemoteTargetId,
    allowed_session: WorkspaceSessionId,
    allowed_tools: BTreeMap<String, RemoteToolPolicy>,
    environment: RemoteEnvironmentPolicy,
    snapshot_authority: Option<Arc<dyn RemoteSnapshotAuthority>>,
}

pub trait RemoteSnapshotAuthority: Send + Sync {
    fn snapshot_available(&self, session: WorkspaceSessionId, snapshot_id: RemoteSnapshotId) -> bool;
}

impl RemoteAuthorizationPolicy {
    pub fn new(allowed_target: RemoteTargetId, allowed_session: WorkspaceSessionId, allowed_tools: Vec<String>) -> Self {
        let allowed_tools = allowed_tools.into_iter().map(|tool| (tool, RemoteToolPolicy::new(true))).collect();
        Self { allowed_target, allowed_session, allowed_tools, environment: RemoteEnvironmentPolicy::default(), snapshot_authority: None }
    }

    pub fn from_policies(
        allowed_target: RemoteTargetId, allowed_session: WorkspaceSessionId, tools: impl IntoIterator<Item = (String, RemoteToolPolicy)>,
        environment: RemoteEnvironmentPolicy,
    ) -> Result<Self, String> {
        let mut allowed_tools = BTreeMap::new();
        for (tool, policy) in tools {
            RemoteTool::new(tool.clone())?;
            if let Some(command) = policy.command() {
                RemoteTool::new(command.to_string())?;
            }
            if allowed_tools.insert(tool.clone(), policy).is_some() {
                return Err(format!("duplicate remote tool policy: {tool}"));
            }
        }
        Ok(Self { allowed_target, allowed_session, allowed_tools, environment, snapshot_authority: None })
    }

    pub fn with_snapshot_authority(mut self, authority: Arc<dyn RemoteSnapshotAuthority>) -> Self {
        self.snapshot_authority = Some(authority);
        self
    }

    pub fn allowed_tools(&self) -> impl Iterator<Item = &str> {
        self.allowed_tools.keys().map(String::as_str)
    }

    pub fn authorize(&self, context: &RemoteExecutionContext, request: RemoteRequest) -> Result<AuthorizedRemoteRequest, RemoteAuthorizationError> {
        if request.request_id.is_zero() {
            return Err(RemoteAuthorizationError::InvalidRequestId);
        }
        if context.workspace_session_id != self.allowed_session || request.workspace_session_id != context.workspace_session_id {
            return Err(RemoteAuthorizationError::SessionMismatch);
        }
        if context.target != self.allowed_target {
            return Err(RemoteAuthorizationError::TargetNotAllowed);
        }

        let mut target_command = None;
        let request = match request.operation() {
            RemoteOperation::Sync(_) => request,
            RemoteOperation::Cancel { target_request_id } => {
                if target_request_id.is_zero() {
                    return Err(RemoteAuthorizationError::InvalidCancelTarget);
                }
                request
            }
            RemoteOperation::Build(build) => {
                if self.snapshot_authority.as_ref().is_none_or(|authority| !authority.snapshot_available(self.allowed_session, build.snapshot_id())) {
                    return Err(RemoteAuthorizationError::SnapshotNotAllowed);
                }
                let Some(tool_policy) = self.allowed_tools.get(build.tool().as_str()).cloned() else {
                    return Err(RemoteAuthorizationError::ToolNotAllowed(build.tool().as_str().to_string()));
                };
                if !tool_policy.allows_arbitrary_argv() && !build.argv().is_empty() {
                    return Err(RemoteAuthorizationError::ToolArgumentsNotAllowed(build.tool().as_str().to_string()));
                }
                let environment = if build.tool().as_str() == "cargo" {
                    if let Some((name, _)) = build.env().first() {
                        return Err(RemoteAuthorizationError::ForbiddenEnvironment(name.clone()));
                    }
                    Vec::new()
                } else {
                    self.environment.filter(build.env())?
                };
                target_command = tool_policy.command().map(str::to_string);
                let filtered = RemoteBuild::new(build.cwd.clone(), build.tool.clone(), build.argv.clone(), environment, build.snapshot_id())
                    .map_err(RemoteAuthorizationError::InvalidEnvironment)?;
                RemoteRequest::build(request.request_id, request.workspace_session_id, filtered)
            }
        };

        Ok(AuthorizedRemoteRequest { request, target: context.target, target_command })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteBackendEvent {
    SyncProgress { completed_bytes: u64, total_bytes: Option<u64> },
    SyncCompleted { snapshot_id: RemoteSnapshotId },
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Error { message: String },
    Cancelled,
    Completed { exit_code: i32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteBackendError {
    Failed(String),
    Spawn(String),
    Transport { class: RemoteFailureClass, message: String },
    Deadline { cause: RemoteTimeoutCause },
    Timeout,
    OutputLimit { limit: u64 },
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteFailureClass {
    Dns,
    Connect,
    Authentication,
    HostIdentity,
    WorkerUnavailable,
    WorkerVersion,
    WorkerProtocol,
    SnapshotTransfer,
    ArtifactManifest,
    ArtifactTransfer,
    Busy,
    Disconnect,
    Cleanup,
}

impl RemoteFailureClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dns => "dns",
            Self::Connect => "connect",
            Self::Authentication => "authentication",
            Self::HostIdentity => "host identity",
            Self::WorkerUnavailable => "worker unavailable",
            Self::WorkerVersion => "worker version",
            Self::WorkerProtocol => "worker protocol",
            Self::SnapshotTransfer => "snapshot transfer",
            Self::ArtifactManifest => "artifact manifest",
            Self::ArtifactTransfer => "artifact transfer",
            Self::Busy => "busy",
            Self::Disconnect => "disconnect",
            Self::Cleanup => "cleanup",
        }
    }
}

impl RemoteBackendError {
    pub fn event(&self) -> RemoteBackendEvent {
        match self {
            Self::Failed(message) | Self::Spawn(message) => RemoteBackendEvent::Error { message: message.clone() },
            Self::Transport { class, message } => RemoteBackendEvent::Error { message: format!("remote {} failure: {message}", class.as_str()) },
            Self::Deadline { cause } => RemoteBackendEvent::Error { message: format!("remote {} deadline exceeded", cause.as_str()) },
            Self::Timeout => RemoteBackendEvent::Error { message: "remote backend timed out".to_string() },
            Self::OutputLimit { limit } => RemoteBackendEvent::Error { message: format!("remote output exceeded limit of {limit} bytes") },
            Self::Cancelled => RemoteBackendEvent::Cancelled,
        }
    }
}

pub type RemoteFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait RemoteBackend: Send + Sync {
    fn execute<'a>(
        &'a self, request: AuthorizedRemoteRequest, control: RemoteExecutionControl, events: mpsc::Sender<RemoteBackendEvent>,
    ) -> RemoteFuture<'a, Result<(), RemoteBackendError>>;
}

fn validate_string(field: &str, value: &str, max: usize) -> Result<(), String> {
    if value.as_bytes().contains(&0) {
        return Err(format!("{field} contains a NUL byte"));
    }
    if value.len() > max {
        return Err(format!("{field} exceeds maximum length {max}"));
    }
    Ok(())
}

fn validate_environment_key(key: &str) -> Result<(), String> {
    validate_string("remote environment key", key, MAX_REMOTE_ENV_KEY_BYTES)?;
    let mut characters = key.bytes();
    let Some(first) = characters.next() else {
        return Err("remote environment key is empty".to_string());
    };
    if !(first == b'_' || first.is_ascii_alphabetic()) || !characters.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric()) {
        return Err("remote environment key is not a valid variable name".to_string());
    }
    Ok(())
}

fn validate_environment_value(value: &str) -> Result<(), String> {
    validate_string("remote environment value", value, MAX_REMOTE_ENV_VALUE_BYTES)?;
    if value.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err("remote environment value contains control data".to_string());
    }
    Ok(())
}

fn forbidden_environment_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "SSH_AUTH_SOCK"
            | "SSH_AGENT_PID"
            | "GITHUB_TOKEN"
            | "GITLAB_TOKEN"
            | "NPM_TOKEN"
            | "KUBECONFIG"
            | "HOME"
            | "CARGO_HOME"
            | "RUSTUP_HOME"
            | "XDG_CONFIG_HOME"
            | "XDG_DATA_HOME"
            | "PATH"
            | "PWD"
            | "OLDPWD"
            | "TMP"
            | "TMPDIR"
            | "TEMP"
            | "USER"
            | "LOGNAME"
            | "SHELL"
            | "BASH_ENV"
            | "ENV"
            | "CDPATH"
            | "LD_PRELOAD"
            | "LD_LIBRARY_PATH"
            | "PYTHONPATH"
            | "PERL5LIB"
            | "RUBYLIB"
            | "NODE_PATH"
            | "GOPATH"
            | "GOMODCACHE"
            | "TOKEN"
            | "PASSWORD"
            | "PASS"
            | "SECRET"
            | "KEY"
            | "GIT_SSH_COMMAND"
    ) || upper.starts_with("AWS_")
        || upper.starts_with("GCP_")
        || upper.starts_with("GOOGLE_")
        || upper.starts_with("AZURE_")
        || upper.starts_with("DOCKER_")
        || upper.starts_with("CARGO_REGISTRIES_")
        || upper.starts_with("XDG_")
        || upper.starts_with("BUNKERBOX_")
        || upper.ends_with("_PROXY")
        || upper.ends_with("_TOKEN")
        || upper.ends_with("_PASSWORD")
        || upper.ends_with("_PASS")
        || upper.ends_with("_SECRET")
        || upper.ends_with("_KEY")
        || upper.contains("CREDENTIAL")
        || upper.contains("PRIVATE_KEY")
}

fn validate_count(count: usize, max: usize, field: &str) -> Result<(), String> {
    if count > max {
        return Err(format!("{field} exceeds maximum count {max}"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "remote_ut.rs"]
mod tests;
