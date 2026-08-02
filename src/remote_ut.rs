use super::*;

fn request(tool: &str) -> RemoteRequest {
    RemoteRequest::build(
        RequestId([1; 16]),
        WorkspaceSessionId([2; 16]),
        RemoteBuild::new(
            WorkspaceRelativePath::new("src").unwrap(),
            RemoteTool::new(tool).unwrap(),
            vec!["build".into()],
            vec![("MODE".into(), "debug".into())],
        )
        .unwrap(),
    )
}

fn context() -> RemoteExecutionContext {
    RemoteExecutionContext { target: RemoteTargetId([3; 16]), workspace_session_id: WorkspaceSessionId([2; 16]) }
}

#[test]
fn policy_authorizes_typed_request() {
    let policy = RemoteAuthorizationPolicy::new(RemoteTargetId([3; 16]), WorkspaceSessionId([2; 16]), vec!["make".into()]);
    let authorized = policy.authorize(&context(), request("make")).unwrap();

    assert_eq!(authorized.request_id(), RequestId([1; 16]));
    assert_eq!(authorized.target(), RemoteTargetId([3; 16]));
    assert_eq!(authorized.request().workspace_session_id, WorkspaceSessionId([2; 16]));
}

#[test]
fn policy_rejects_unapproved_tool() {
    let policy = RemoteAuthorizationPolicy::new(RemoteTargetId([3; 16]), WorkspaceSessionId([2; 16]), vec!["cargo".into()]);

    assert_eq!(policy.authorize(&context(), request("make")), Err(RemoteAuthorizationError::ToolNotAllowed("make".into())));
}

#[test]
fn backend_errors_have_typed_events() {
    assert_eq!(RemoteBackendError::Spawn("could not start".into()).event(), RemoteBackendEvent::Error { message: "could not start".into() });
    assert_eq!(RemoteBackendError::Timeout.event(), RemoteBackendEvent::Error { message: "remote backend timed out".into() });
    assert_eq!(RemoteBackendError::Cancelled.event(), RemoteBackendEvent::Cancelled);
}
