use super::{handle_response, Frame, FrameType};

#[test]
fn discard_stdout_and_stderr_frames() {
    assert_eq!(handle_response(Frame::new(FrameType::Stdout, b"visible output".to_vec())).unwrap(), None);
    assert_eq!(handle_response(Frame::new(FrameType::Stderr, b"/home/bo/Pictures/Screenshots/path.png".to_vec())).unwrap(), None);
}

#[test]
fn preserve_exit_status() {
    assert_eq!(handle_response(Frame::new(FrameType::Exit, (-17i32).to_le_bytes().to_vec())).unwrap(), Some(-17));
}
