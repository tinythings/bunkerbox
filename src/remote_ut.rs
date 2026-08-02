use super::*;

fn request(tool: &str) -> RemoteRequest {
    RemoteRequest::build(
        RequestId([1; 16]),
        WorkspaceSessionId([2; 16]),
        RemoteBuild::new(
            WorkspaceRelativePath::new("src").unwrap(),
            RemoteTool::new(tool).unwrap(),
            vec!["build".into()],
            vec![("CC".into(), "cc".into())],
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

#[test]
fn environment_policy_preserves_allowed_entries() {
    let policy = RemoteAuthorizationPolicy::new(RemoteTargetId([3; 16]), WorkspaceSessionId([2; 16]), vec!["make".into()]);
    let authorized = policy.authorize(&context(), request("make")).unwrap();
    let RemoteOperation::Build(build) = authorized.request().operation() else { panic!("expected build") };
    assert_eq!(build.env(), [("CC".into(), "cc".into())]);
}

#[test]
fn environment_policy_rejects_forbidden_and_unlisted_entries() {
    for (name, expected) in [
        ("SSH_AUTH_SOCK", RemoteAuthorizationError::ForbiddenEnvironment("SSH_AUTH_SOCK".into())),
        ("UNTRUSTED", RemoteAuthorizationError::EnvironmentNotAllowed("UNTRUSTED".into())),
    ] {
        let build = RemoteBuild::new(
            WorkspaceRelativePath::new("src").unwrap(),
            RemoteTool::new("make").unwrap(),
            Vec::new(),
            vec![(name.into(), "value".into())],
        )
        .unwrap();
        let request = RemoteRequest::build(RequestId([1; 16]), WorkspaceSessionId([2; 16]), build);
        let policy = RemoteAuthorizationPolicy::new(RemoteTargetId([3; 16]), WorkspaceSessionId([2; 16]), vec!["make".into()]);
        assert_eq!(policy.authorize(&context(), request), Err(expected));
    }
}

#[test]
fn environment_policy_rejects_duplicates_and_control_data() {
    let duplicate = RemoteBuild::new(
        WorkspaceRelativePath::new("src").unwrap(),
        RemoteTool::new("make").unwrap(),
        Vec::new(),
        vec![("CC".into(), "one".into()), ("CC".into(), "two".into())],
    )
    .unwrap();
    let policy = RemoteAuthorizationPolicy::new(RemoteTargetId([3; 16]), WorkspaceSessionId([2; 16]), vec!["make".into()]);
    let request = RemoteRequest::build(RequestId([1; 16]), WorkspaceSessionId([2; 16]), duplicate);
    assert_eq!(policy.authorize(&context(), request), Err(RemoteAuthorizationError::DuplicateEnvironment("CC".into())));

    assert!(RemoteBuild::new(
        WorkspaceRelativePath::new("src").unwrap(),
        RemoteTool::new("make").unwrap(),
        Vec::new(),
        vec![("CC".into(), "bad\nvalue".into())],
    )
    .is_err());
}

#[test]
fn command_policy_rejects_unapproved_arguments() {
    let policy = RemoteAuthorizationPolicy::from_policies(
        RemoteTargetId([3; 16]),
        WorkspaceSessionId([2; 16]),
        [("make".into(), RemoteToolPolicy::new(false))],
        RemoteEnvironmentPolicy::default(),
    )
    .unwrap();
    assert_eq!(policy.authorize(&context(), request("make")), Err(RemoteAuthorizationError::ToolArgumentsNotAllowed("make".into())));
}
