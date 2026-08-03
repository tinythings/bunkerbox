use crate::remote::RemoteSnapshotId;
use crate::vscomm::{RemoteBuild, RemoteEvent, RemoteEventKind, RemoteRequest, RemoteTool, RequestId, WorkspaceRelativePath, WorkspaceSessionId};
use rand::RngCore;
use std::env;
use std::io::{Read, Write};
use std::path::Path;

pub fn remote_sync_request(request_id: RequestId, session_id: WorkspaceSessionId) -> RemoteRequest {
    RemoteRequest::sync(request_id, session_id)
}

pub fn remote_diagnostic_sync_request(request_id: RequestId, session_id: WorkspaceSessionId) -> RemoteRequest {
    RemoteRequest::diagnostic_sync(request_id, session_id)
}

pub fn remote_session_from_env() -> Result<WorkspaceSessionId, String> {
    env::var("BUNKERBOX_REMOTE_SESSION")
        .map_err(|_| "BUNKERBOX_REMOTE_SESSION is missing".to_string())
        .and_then(|value| WorkspaceSessionId::from_hex(&value))
}

pub fn logical_workspace_cwd(path: &Path) -> Result<String, String> {
    let relative = path.strip_prefix("/workspace").map_err(|_| "current directory must be under /workspace".to_string())?;
    let value = relative.to_str().ok_or_else(|| "current directory is not valid UTF-8".to_string())?.to_string();
    crate::remote::WorkspaceRelativePath::new(&value)?;
    Ok(value)
}

pub fn new_request_id() -> RequestId {
    let mut bytes = [0; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    RequestId(bytes)
}

pub fn remote_tool_enabled(tool: &str) -> bool {
    env::var("BUNKERBOX_REMOTE_TOOLS").ok().is_some_and(|tools| tools.split(',').any(|candidate| candidate == tool))
}

pub fn remote_environment_names() -> Vec<String> {
    env::var("BUNKERBOX_REMOTE_ENV_NAMES")
        .ok()
        .map(|names| names.split(',').filter(|name| !name.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

pub fn selected_remote_environment(names: impl IntoIterator<Item = String>) -> Vec<(String, String)> {
    names
        .into_iter()
        .filter(|name| !never_forward_environment(name))
        .filter_map(|name| env::var_os(&name).and_then(|value| value.into_string().ok().map(|value| (name, value))))
        .collect()
}

fn never_forward_environment(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(upper.as_str(), "PATH" | "HOME" | "SSH_AUTH_SOCK" | "SSH_AGENT_PID" | "GITHUB_TOKEN" | "GITLAB_TOKEN" | "NPM_TOKEN" | "KUBECONFIG")
        || upper.starts_with("BUNKERBOX_")
        || upper.starts_with("AWS_")
        || upper.starts_with("GCP_")
        || upper.starts_with("GOOGLE_")
        || upper.starts_with("AZURE_")
        || upper.starts_with("DOCKER_")
        || upper.starts_with("CARGO_REGISTRIES_")
        || upper.starts_with("XDG_")
        || upper.ends_with("_PROXY")
        || upper.ends_with("_TOKEN")
        || upper.ends_with("_PASSWORD")
        || upper.ends_with("_PASS")
        || upper.ends_with("_SECRET")
        || upper.ends_with("_KEY")
}

#[cfg(test)]
#[path = "remote_client_ut.rs"]
mod tests;

pub fn remote_build_request(
    request_id: RequestId, session_id: WorkspaceSessionId, cwd: impl Into<String>, tool: impl Into<String>, argv: Vec<String>,
    env: Vec<(String, String)>, snapshot_id: RemoteSnapshotId,
) -> Result<RemoteRequest, String> {
    let snapshot_id = crate::vscomm::RemoteSnapshotId(*snapshot_id.as_bytes());
    let build = RemoteBuild::new(WorkspaceRelativePath::new(cwd)?, RemoteTool::new(tool)?, argv, env, snapshot_id)?;
    Ok(RemoteRequest::build(request_id, session_id, build))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteCompletion {
    Synced(RemoteSnapshotId),
    Completed(i32),
}

pub fn execute_remote_request_to<S: Read + Write, WOut: Write, WErr: Write>(
    stream: &mut S, request: RemoteRequest, stdout: &mut WOut, stderr: &mut WErr,
) -> Result<RemoteCompletion, String> {
    let request_id = request.request_id;
    request.to_frame()?.write(stream).map_err(|e| format!("send remote request: {e}"))?;

    loop {
        let frame = crate::vscomm::Frame::read(stream).map_err(|e| format!("read remote event: {e}"))?;
        let event = RemoteEvent::from_frame(frame)?;
        if event.request_id != request_id {
            return Err("remote event request ID mismatch".to_string());
        }
        match event.kind {
            RemoteEventKind::SyncProgress { .. } => {}
            RemoteEventKind::SyncCompleted { snapshot_id } => {
                let snapshot_id = RemoteSnapshotId::from_bytes(snapshot_id.0);
                if snapshot_id.is_zero() {
                    return Err("remote snapshot ID must be nonzero".to_string());
                }
                return Ok(RemoteCompletion::Synced(snapshot_id));
            }
            RemoteEventKind::Stdout(data) => {
                stdout.write_all(&data).map_err(|e| format!("stdout: {e}"))?;
                stdout.flush().map_err(|e| format!("flush stdout: {e}"))?;
            }
            RemoteEventKind::Stderr(data) => {
                stderr.write_all(&data).map_err(|e| format!("stderr: {e}"))?;
                stderr.flush().map_err(|e| format!("flush stderr: {e}"))?;
            }
            RemoteEventKind::Error { message, .. } => return Err(message),
            RemoteEventKind::Cancelled => return Err("remote operation cancelled".to_string()),
            RemoteEventKind::Completed { exit_code } => return Ok(RemoteCompletion::Completed(exit_code)),
        }
    }
}
