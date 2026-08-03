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
