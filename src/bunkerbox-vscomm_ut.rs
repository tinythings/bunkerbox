use super::{handle_response, handle_response_to, Frame, FrameType};

#[test]
fn forward_stdout_and_stderr_frames() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    assert_eq!(handle_response_to(Frame::new(FrameType::Stdout, b"visible output".to_vec()), &mut stdout, &mut stderr).unwrap(), None);
    assert_eq!(handle_response_to(Frame::new(FrameType::Stderr, b"command failed".to_vec()), &mut stdout, &mut stderr).unwrap(), None);
    assert_eq!(stdout, b"visible output");
    assert_eq!(stderr, b"command failed");
}

#[test]
fn preserve_exit_status() {
    assert_eq!(handle_response(Frame::new(FrameType::Exit, (-17i32).to_le_bytes().to_vec())).unwrap(), Some(-17));
}
