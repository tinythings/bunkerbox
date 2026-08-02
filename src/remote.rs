#![allow(dead_code)]

use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;

pub const MAX_REMOTE_STRING_BYTES: usize = 4 * 1024;
pub const MAX_REMOTE_TOOL_BYTES: usize = 256;
pub const MAX_REMOTE_ARG_COUNT: usize = 256;
pub const MAX_REMOTE_ARG_BYTES: usize = 4 * 1024;
pub const MAX_REMOTE_ENV_COUNT: usize = 64;
pub const MAX_REMOTE_ENV_KEY_BYTES: usize = 256;
pub const MAX_REMOTE_ENV_VALUE_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestId(pub [u8; 16]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceSessionId(pub [u8; 16]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        if value.is_empty() {
            return Err("remote tool is empty".to_string());
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBuild {
    cwd: WorkspaceRelativePath,
    tool: RemoteTool,
    argv: Vec<String>,
    env: Vec<(String, String)>,
}

impl RemoteBuild {
    pub fn new(cwd: WorkspaceRelativePath, tool: RemoteTool, argv: Vec<String>, env: Vec<(String, String)>) -> Result<Self, String> {
        validate_count(argv.len(), MAX_REMOTE_ARG_COUNT, "remote argv")?;
        argv.iter().try_for_each(|arg| validate_string("remote argument", arg, MAX_REMOTE_ARG_BYTES))?;
        validate_count(env.len(), MAX_REMOTE_ENV_COUNT, "remote environment")?;
        env.iter().try_for_each(|(key, value)| {
            validate_string("remote environment key", key, MAX_REMOTE_ENV_KEY_BYTES)?;
            if key.is_empty() || key.contains('=') {
                return Err("remote environment key is invalid".to_string());
            }
            validate_string("remote environment value", value, MAX_REMOTE_ENV_VALUE_BYTES)
        })?;
        Ok(Self { cwd, tool, argv, env })
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteOperation {
    Sync,
    Build(RemoteBuild),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRequest {
    request_id: RequestId,
    workspace_session_id: WorkspaceSessionId,
    operation: RemoteOperation,
}

impl RemoteRequest {
    pub fn sync(request_id: RequestId, workspace_session_id: WorkspaceSessionId) -> Self {
        Self { request_id, workspace_session_id, operation: RemoteOperation::Sync }
    }

    pub fn build(request_id: RequestId, workspace_session_id: WorkspaceSessionId, build: RemoteBuild) -> Self {
        Self { request_id, workspace_session_id, operation: RemoteOperation::Build(build) }
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAuthorizationError {
    SessionMismatch,
    TargetNotAllowed,
    ToolNotAllowed(String),
}

impl RemoteAuthorizationError {
    pub fn event(&self) -> RemoteBackendEvent {
        RemoteBackendEvent::Error { message: format!("remote authorization rejected: {self:?}") }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAuthorizationPolicy {
    allowed_target: RemoteTargetId,
    allowed_session: WorkspaceSessionId,
    allowed_tools: Vec<String>,
}

impl RemoteAuthorizationPolicy {
    pub fn new(allowed_target: RemoteTargetId, allowed_session: WorkspaceSessionId, allowed_tools: Vec<String>) -> Self {
        Self { allowed_target, allowed_session, allowed_tools }
    }

    pub fn authorize(&self, context: &RemoteExecutionContext, request: RemoteRequest) -> Result<AuthorizedRemoteRequest, RemoteAuthorizationError> {
        if context.workspace_session_id != self.allowed_session || request.workspace_session_id != context.workspace_session_id {
            return Err(RemoteAuthorizationError::SessionMismatch);
        }
        if context.target != self.allowed_target {
            return Err(RemoteAuthorizationError::TargetNotAllowed);
        }

        if let RemoteOperation::Build(build) = request.operation() {
            if !self.allowed_tools.iter().any(|tool| tool == build.tool().as_str()) {
                return Err(RemoteAuthorizationError::ToolNotAllowed(build.tool().as_str().to_string()));
            }
        }

        Ok(AuthorizedRemoteRequest { request, target: context.target })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteBackendEvent {
    SyncProgress { completed_bytes: u64, total_bytes: Option<u64> },
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
    Timeout,
    Cancelled,
}

impl RemoteBackendError {
    pub fn event(&self) -> RemoteBackendEvent {
        match self {
            Self::Failed(message) | Self::Spawn(message) => RemoteBackendEvent::Error { message: message.clone() },
            Self::Timeout => RemoteBackendEvent::Error { message: "remote backend timed out".to_string() },
            Self::Cancelled => RemoteBackendEvent::Cancelled,
        }
    }
}

pub type RemoteFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait RemoteBackend: Send + Sync {
    fn execute<'a>(
        &'a self, request: AuthorizedRemoteRequest, events: mpsc::Sender<RemoteBackendEvent>,
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

fn validate_count(count: usize, max: usize, field: &str) -> Result<(), String> {
    if count > max {
        return Err(format!("{field} exceeds maximum count {max}"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "remote_ut.rs"]
mod tests;
