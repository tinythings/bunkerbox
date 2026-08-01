use super::{configure, diagnostic, diagnostic_bytes};
use std::fs;

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
