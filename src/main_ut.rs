use super::{
    decode_workspace_handoff, encode_workspace_handoff, read_run_handoff, read_startup_ready, remote_tool_names, write_run_handoff,
    write_startup_ready,
};
use bunkerbox::cfg::RemoteToolSpec;
use bunkerbox::vscomm::WorkspaceSessionId;
use std::fs::File;
use std::os::fd::FromRawFd;
use std::path::Path;

#[test]
fn workspace_handoff_round_trips_a_path() {
    let frame = encode_workspace_handoff(b"/workspace/project").unwrap();
    let path = decode_workspace_handoff(&frame).unwrap();

    assert_eq!(path, Path::new("/workspace/project"));
}

#[test]
fn ui_message_is_not_a_workspace_handoff() {
    let ui_message = b"@popup\0info\0Bunkerbox\0Starting...\0\n";

    assert!(decode_workspace_handoff(ui_message).is_err());
}

#[test]
fn workspace_handoff_rejects_truncated_payload() {
    let frame = encode_workspace_handoff(b"/workspace/project").unwrap();

    assert!(decode_workspace_handoff(&frame[..frame.len() - 1]).is_err());
}

#[test]
fn run_handoff_round_trips_path_and_session() {
    let (parent, child) = unsafe {
        let mut fds = [-1; 2];
        assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
        (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1]))
    };
    let mut child = child;
    write_run_handoff(&mut child, Path::new("/workspace/project"), WorkspaceSessionId([7; 16])).unwrap();
    drop(child);
    let mut parent = parent;
    let (path, session) = read_run_handoff(&mut parent).unwrap();
    assert_eq!(path, Path::new("/workspace/project"));
    assert_eq!(session, WorkspaceSessionId([7; 16]));
}

#[test]
fn run_handoff_rejects_zero_session() {
    let mut payload = b"/workspace/project".to_vec();
    payload.push(0);
    payload.extend_from_slice(&[0; 16]);
    let frame = encode_workspace_handoff(&payload).unwrap();
    let path = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(path.path(), frame).unwrap();
    let mut file = File::open(path.path()).unwrap();
    assert!(read_run_handoff(&mut file).is_err());
}

#[test]
fn remote_tool_names_preserve_configured_order() {
    assert_eq!(
        remote_tool_names(&[RemoteToolSpec { name: "make".into(), allow_args: true }, RemoteToolSpec { name: "cargo".into(), allow_args: false },]),
        vec!["make", "cargo"]
    );
}

#[test]
fn startup_ready_handoff_round_trips() {
    let (parent, child) = unsafe {
        let mut fds = [-1; 2];
        assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
        (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1]))
    };
    let mut child = child;
    write_startup_ready(&mut child).unwrap();
    drop(child);
    let mut parent = parent;
    read_startup_ready(&mut parent).unwrap();
}

#[test]
fn startup_ready_handoff_rejects_wrong_type() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), b"BAD!").unwrap();
    let mut file = File::open(file.path()).unwrap();
    assert!(read_startup_ready(&mut file).is_err());
}
