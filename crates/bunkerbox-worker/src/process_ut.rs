use super::*;
use crate::platform;
use crate::worker::FrameWriter;
use bunkerbox_worker_protocol::{WorkerBuild, WorkerMessage, WorkerRequestId, WorkerSessionId, WorkerUploadId};
use std::fs;
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};
use tempfile::tempdir;

#[test]
fn direct_process_execution_preserves_literal_arguments_and_reaps_group() {
    let temp = tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let jobs = platform::open_root(temp.path()).unwrap();
    let job = JobWorkspace::create(&jobs).unwrap();
    let build = WorkerBuild::new(
        "echo",
        "/bin/echo",
        vec!["literal $(argument)".to_string()],
        "",
        Vec::new(),
        vec![("PATH".to_string(), "/usr/bin:/bin".to_string())],
        WorkerUploadId([3; 16]),
    )
    .unwrap();
    let writer = FrameWriter::new(Vec::new());
    let status = execute_build(&job, &build, WorkerRequestId([1; 16]), WorkerSessionId([2; 16]), &writer, &|| false).unwrap();
    assert_eq!(status, 0);
    let bytes = writer.into_inner().unwrap();
    let mut reader = Cursor::new(bytes);
    let mut output = Vec::new();
    while let Some(message) = WorkerMessage::read_blocking_optional(&mut reader).unwrap() {
        if let WorkerMessage::Stdout { data, .. } = message {
            output.extend_from_slice(&data);
        }
    }
    assert_eq!(output, b"literal $(argument)\n");
}

#[test]
fn disconnect_and_inherited_pipes_do_not_leave_a_build_running() {
    let temp = tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let jobs = platform::open_root(temp.path()).unwrap();
    let script = temp.path().join("descendant.sh");
    fs::write(&script, b"#!/bin/sh\nprintf before\n(sleep 10) &\nexit 0\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let job = JobWorkspace::create(&jobs).unwrap();
    let build = WorkerBuild::new(
        "descendant",
        script.to_string_lossy(),
        Vec::new(),
        "",
        Vec::new(),
        vec![("PATH".to_string(), "/usr/bin:/bin".to_string())],
        WorkerUploadId([4; 16]),
    )
    .unwrap();
    let writer = FrameWriter::new(Vec::new());
    let started = Instant::now();
    let status = execute_build(&job, &build, WorkerRequestId([5; 16]), WorkerSessionId([6; 16]), &writer, &|| false).unwrap();
    assert_eq!(status, 0);
    assert!(started.elapsed() < Duration::from_secs(2));

    let job = JobWorkspace::create(&jobs).unwrap();
    let writer = FrameWriter::new(Vec::new());
    let error = execute_build(&job, &build, WorkerRequestId([7; 16]), WorkerSessionId([8; 16]), &writer, &|| true).unwrap_err();
    assert!(error.contains("disconnected"));
    assert!(started.elapsed() < Duration::from_secs(2));
}
