use super::*;

#[test]
fn trusted_worker_limits_are_parsed_from_fixed_flags() {
    let config = parse_args(vec![
        "--stdio".into(),
        "--workspace-root".into(),
        "/var/lib/bunkerbox-worker".into(),
        "--build-timeout-ms".into(),
        "2500".into(),
        "--max-worker-uploads".into(),
        "3".into(),
        "--max-worker-upload-bytes".into(),
        "4096".into(),
        "--max-worker-jobs".into(),
        "2".into(),
        "--max-worker-job-bytes".into(),
        "8192".into(),
        "--max-worker-artifact-spools".into(),
        "2".into(),
        "--max-worker-artifact-spool-bytes".into(),
        "16384".into(),
        "--max-worker-state-entries".into(),
        "99".into(),
    ])
    .unwrap();
    assert_eq!(config.root, PathBuf::from("/var/lib/bunkerbox-worker"));
    assert_eq!(config.build_timeout, Duration::from_millis(2500));
    assert_eq!(config.limits.max_uploads, 3);
    assert_eq!(config.limits.max_job_bytes, 8192);
    assert_eq!(config.limits.max_state_entries, 99);
}

#[test]
fn trusted_worker_arguments_fail_closed() {
    assert!(parse_args(vec!["--workspace-root".into(), "/tmp/root".into()]).is_err());
    assert!(parse_args(vec!["--stdio".into(), "--workspace-root".into(), "relative".into()]).is_err());
    assert!(parse_args(vec!["--stdio".into(), "--stdio".into(), "--workspace-root".into(), "/tmp/root".into()]).is_err());
    assert!(parse_args(vec!["--stdio".into(), "--workspace-root".into(), "/tmp/root".into(), "--build-timeout-ms".into(), "0".into()]).is_err());
}
