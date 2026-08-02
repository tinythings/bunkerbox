use crate::vscomm::{RemoteBuild, RemoteEvent, RemoteEventKind, RemoteRequest, RemoteTool, RequestId, WorkspaceRelativePath, WorkspaceSessionId};
use std::io::{Read, Write};

pub fn remote_sync_request(request_id: RequestId, session_id: WorkspaceSessionId) -> RemoteRequest {
    RemoteRequest::sync(request_id, session_id)
}

pub fn remote_build_request(
    request_id: RequestId, session_id: WorkspaceSessionId, cwd: impl Into<String>, tool: impl Into<String>, argv: Vec<String>,
    env: Vec<(String, String)>,
) -> Result<RemoteRequest, String> {
    let build = RemoteBuild::new(WorkspaceRelativePath::new(cwd)?, RemoteTool::new(tool)?, argv, env)?;
    Ok(RemoteRequest::build(request_id, session_id, build))
}

pub fn execute_remote_request_to<S: Read + Write, WOut: Write, WErr: Write>(
    stream: &mut S, request: RemoteRequest, stdout: &mut WOut, stderr: &mut WErr,
) -> Result<i32, String> {
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
            RemoteEventKind::Completed { exit_code } => return Ok(exit_code),
        }
    }
}
