use super::{monitor_bwrap_status, ChildEvent};
use std::io::Write;

#[test]
fn bwrap_status_reports_command_start() {
    let mut status = tempfile::NamedTempFile::new().unwrap();
    writeln!(status, "{{\"child-pid\":1234}}").unwrap();
    writeln!(status, "{{\"exit-code\":0}}").unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    monitor_bwrap_status(status.reopen().unwrap(), tx);

    assert!(matches!(rx.try_recv().unwrap(), ChildEvent::LauncherStarted));
    assert!(rx.try_recv().is_err());
}

#[test]
fn bwrap_status_reports_setup_failure_without_child() {
    let mut status = tempfile::NamedTempFile::new().unwrap();
    writeln!(status, "{{\"exit-code\":1}}").unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    monitor_bwrap_status(status.reopen().unwrap(), tx);

    assert!(matches!(rx.try_recv().unwrap(), ChildEvent::LauncherFailed(_)));
}
