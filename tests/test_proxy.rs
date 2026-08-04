use bunkerbox::proxy::FilterProxy;
use std::os::unix::fs::FileTypeExt;

#[tokio::test]
async fn proxy_unix_stop_removes_socket() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("proxy.sock");

    let handle = FilterProxy::new(vec!["localhost".into()]).bind_unix(&sock_path).await.unwrap();
    assert!(std::fs::symlink_metadata(&sock_path).unwrap().file_type().is_socket());

    handle.stop();

    assert!(!sock_path.exists());
}

#[tokio::test]
async fn proxy_unix_drop_removes_socket() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("proxy.sock");

    {
        let _handle = FilterProxy::new(vec!["localhost".into()]).bind_unix(&sock_path).await.unwrap();
        assert!(std::fs::symlink_metadata(&sock_path).unwrap().file_type().is_socket());
    }

    assert!(!sock_path.exists());
}
