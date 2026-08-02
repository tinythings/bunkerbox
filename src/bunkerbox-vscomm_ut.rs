use super::vscomm::{RemoteEvent, RemoteEventKind, RequestId, WorkspaceSessionId};
use super::{execute_remote_request_to, handle_response, handle_response_to, remote_build_request, Frame, FrameType};
use std::io::{self, Read, Write};

struct MemoryStream {
    input: io::Cursor<Vec<u8>>,
    output: Vec<u8>,
}

struct FlushWriter {
    bytes: Vec<u8>,
    flushes: usize,
}

impl Write for FlushWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}

impl MemoryStream {
    fn new(frames: Vec<Frame>) -> Self {
        let mut input = Vec::new();
        for frame in frames {
            frame.write(&mut input).unwrap();
        }
        Self { input: io::Cursor::new(input), output: Vec::new() }
    }
}

impl Read for MemoryStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.input.read(buf)
    }
}

impl Write for MemoryStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.output.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

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

#[test]
fn explicit_remote_client_preserves_streams_status_and_request_id() {
    let request_id = RequestId([9; 16]);
    let request = remote_build_request(request_id, WorkspaceSessionId([8; 16]), "src", "make", vec!["release mode".into()], vec![]).unwrap();
    let responses = vec![
        RemoteEvent { request_id, kind: RemoteEventKind::Stdout(b"out".to_vec()) }.to_frame().unwrap(),
        RemoteEvent { request_id, kind: RemoteEventKind::Stderr(b"err".to_vec()) }.to_frame().unwrap(),
        RemoteEvent { request_id, kind: RemoteEventKind::Completed { exit_code: 23 } }.to_frame().unwrap(),
    ];
    let mut stream = MemoryStream::new(responses);
    let mut stdout = FlushWriter { bytes: Vec::new(), flushes: 0 };
    let mut stderr = FlushWriter { bytes: Vec::new(), flushes: 0 };

    assert_eq!(execute_remote_request_to(&mut stream, request, &mut stdout, &mut stderr).unwrap(), 23);
    assert_eq!(stdout.bytes, b"out");
    assert_eq!(stderr.bytes, b"err");
    assert_eq!(stdout.flushes, 1);
    assert_eq!(stderr.flushes, 1);

    let sent = Frame::read(&mut io::Cursor::new(stream.output)).unwrap();
    let decoded = super::vscomm::RemoteRequest::from_frame(sent).unwrap();
    assert_eq!(decoded.request_id, request_id);
    let super::vscomm::RemoteOperation::Build(build) = decoded.operation else { panic!("expected build") };
    assert_eq!(build.cwd.as_str(), "src");
    assert_eq!(build.argv, ["release mode"]);
}

#[test]
fn explicit_remote_client_returns_remote_failure_without_local_fallback() {
    let request_id = RequestId([4; 16]);
    let request = super::remote_sync_request(request_id, WorkspaceSessionId([5; 16]));
    let response = RemoteEvent {
        request_id,
        kind: RemoteEventKind::Error { code: super::vscomm::RemoteErrorCode::Failed, message: "backend unavailable".into() },
    };
    let mut stream = MemoryStream::new(vec![response.to_frame().unwrap()]);

    let error = execute_remote_request_to(&mut stream, request, &mut Vec::new(), &mut Vec::new()).unwrap_err();
    assert_eq!(error, "backend unavailable");
}

#[test]
fn explicit_remote_client_rejects_mismatched_request_id() {
    let request_id = RequestId([4; 16]);
    let response = RemoteEvent { request_id: RequestId([5; 16]), kind: RemoteEventKind::Completed { exit_code: 0 } };
    let mut stream = MemoryStream::new(vec![response.to_frame().unwrap()]);

    let error =
        execute_remote_request_to(&mut stream, super::remote_sync_request(request_id, WorkspaceSessionId([5; 16])), &mut Vec::new(), &mut Vec::new())
            .unwrap_err();
    assert_eq!(error, "remote event request ID mismatch");
}
