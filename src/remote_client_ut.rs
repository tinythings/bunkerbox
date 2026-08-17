use super::*;
use std::path::Path;

#[test]
fn logical_cwd_is_workspace_relative() {
    assert_eq!(logical_workspace_cwd(Path::new("/workspace/foo/bar")).unwrap(), "foo/bar");
    assert!(logical_workspace_cwd(Path::new("/tmp/foo")).is_err());
}

#[test]
fn selected_environment_uses_only_targeted_names() {
    std::env::set_var("BB_TEST_REMOTE_ALLOWED", "selected");
    std::env::set_var("BB_TEST_REMOTE_PATH", "should-not-forward");
    let values = selected_remote_environment(vec!["BB_TEST_REMOTE_ALLOWED".into(), "PATH".into(), "BB_TEST_REMOTE_PATH".into()]);
    assert_eq!(values, vec![("BB_TEST_REMOTE_ALLOWED".into(), "selected".into()), ("BB_TEST_REMOTE_PATH".into(), "should-not-forward".into())]);
    std::env::remove_var("BB_TEST_REMOTE_ALLOWED");
    std::env::remove_var("BB_TEST_REMOTE_PATH");
}

#[test]
fn cargo_environment_is_empty_even_when_names_are_requested() {
    std::env::set_var("BB_TEST_CARGO_FLAGS", "should-not-forward");
    assert!(selected_remote_environment_for_tool("cargo", vec!["BB_TEST_CARGO_FLAGS".into()]).is_empty());
    std::env::remove_var("BB_TEST_CARGO_FLAGS");
}

#[test]
fn cancel_request_uses_a_distinct_request_and_target_identity() {
    let request = remote_cancel_request(RequestId([8; 16]), WorkspaceSessionId([2; 16]), RequestId([7; 16]));
    assert_eq!(request.request_id, RequestId([8; 16]));
    assert_eq!(request.operation, crate::vscomm::RemoteOperation::Cancel { target_request_id: RequestId([7; 16]) });
}
