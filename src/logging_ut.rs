use super::{clear_status_fd, configure, diagnostic, diagnostic_bytes, prompt_password, set_status_fd};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::thread;

#[test]
fn diagnostics_write_to_file_without_terminal_output() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bunkerbox.log");
    configure(false, Some(path.to_string_lossy().into_owned()));

    diagnostic("daemon warning");
    diagnostic_bytes("stderr", b"/home/bo/Pictures/Screenshots/path.png");

    let contents = fs::read_to_string(&path).unwrap();
    assert!(contents.contains("daemon warning"));
    assert!(contents.contains("stderr: /home/bo/Pictures/Screenshots/path.png"));

    configure(false, None);
}

#[test]
fn password_prompt_uses_one_response_channel_and_hides_on_completion() {
    let mut fds = [-1; 2];
    assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) }, 0);
    let input = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let mut peer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    let peer_thread = thread::spawn(move || {
        let mut reader = BufReader::new(peer.try_clone().unwrap());
        let mut show = String::new();
        reader.read_line(&mut show).unwrap();
        assert!(show.contains("password"));
        peer.write_all(b"secret\n").unwrap();
        let mut hide = String::new();
        reader.read_line(&mut hide).unwrap();
        assert!(hide.contains("password"));
        assert!(!hide.contains("secret"));
    });

    set_status_fd(input.as_raw_fd());
    assert_eq!(prompt_password("Password", "Enter password").unwrap(), "secret");
    clear_status_fd();
    drop(input);
    peer_thread.join().unwrap();
}
