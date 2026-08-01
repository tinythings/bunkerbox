use super::*;
use std::io::{self, Cursor, Read};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

fn encoded_frame(frame_type: u16, payload_len: u32, payload: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(6 + payload.len());
    encoded.extend_from_slice(&frame_type.to_le_bytes());
    encoded.extend_from_slice(&payload_len.to_le_bytes());
    encoded.extend_from_slice(payload);
    encoded
}

fn frame_error(result: io::Result<Frame>) -> io::Error {
    match result {
        Ok(_) => panic!("expected frame read to fail"),
        Err(error) => error,
    }
}

struct FragmentedReader {
    data: Vec<u8>,
    offset: usize,
    chunk_size: usize,
}

impl FragmentedReader {
    fn new(data: Vec<u8>, chunk_size: usize) -> Self {
        Self { data, offset: 0, chunk_size }
    }
}

impl Read for FragmentedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.offset == self.data.len() {
            return Ok(0);
        }

        let amount = self.chunk_size.min(buf.len()).min(self.data.len() - self.offset);
        buf[..amount].copy_from_slice(&self.data[self.offset..self.offset + amount]);
        self.offset += amount;
        Ok(amount)
    }
}

impl AsyncRead for FragmentedReader {
    fn poll_read(mut self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if self.offset == self.data.len() {
            return Poll::Ready(Ok(()));
        }

        let amount = self.chunk_size.min(buf.remaining()).min(self.data.len() - self.offset);
        buf.put_slice(&self.data[self.offset..self.offset + amount]);
        self.offset += amount;
        Poll::Ready(Ok(()))
    }
}

#[test]
fn read_zero_length_frame() {
    let frame = Frame::read(&mut Cursor::new(encoded_frame(FrameType::Stdout as u16, 0, &[]))).unwrap();
    assert!(matches!(frame.frame_type, FrameType::Stdout));
    assert!(frame.payload.is_empty());
}

#[test]
fn read_maximum_size_frame() {
    let payload = vec![0xA5; MAX_FRAME_PAYLOAD];
    let frame = Frame::read(&mut Cursor::new(encoded_frame(FrameType::Stdout as u16, MAX_FRAME_PAYLOAD as u32, &payload))).unwrap();
    assert_eq!(frame.payload, payload);
}

#[test]
fn read_oversized_frame_before_payload_read() {
    let encoded = encoded_frame(FrameType::Stdout as u16, (MAX_FRAME_PAYLOAD + 1) as u32, &[]);
    let error = frame_error(Frame::read(&mut Cursor::new(encoded)));
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("frame payload too large"));
}

#[test]
fn read_unknown_frame_type() {
    let error = frame_error(Frame::read(&mut Cursor::new(encoded_frame(99, 0, &[]))));
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("unknown frame type"));
}

#[test]
fn read_unknown_frame_type_with_oversized_payload() {
    let error = frame_error(Frame::read(&mut Cursor::new(encoded_frame(99, (MAX_FRAME_PAYLOAD + 1) as u32, &[]))));
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("unknown frame type"));
}

#[test]
fn read_truncated_header() {
    let error = frame_error(Frame::read(&mut Cursor::new(vec![1, 0, 0])));
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn read_truncated_payload() {
    let error = frame_error(Frame::read(&mut Cursor::new(encoded_frame(FrameType::Stdout as u16, 4, &[1, 2]))));
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn read_fragmented_frame() {
    let payload = b"fragmented";
    let frame = Frame::read(&mut FragmentedReader::new(encoded_frame(FrameType::Stderr as u16, payload.len() as u32, payload), 1)).unwrap();
    assert!(matches!(frame.frame_type, FrameType::Stderr));
    assert_eq!(frame.payload, payload);
}

#[test]
fn write_preserves_wire_format() {
    let mut encoded = Vec::new();
    Frame::new(FrameType::Exit, vec![1, 2, 3]).write(&mut encoded).unwrap();
    assert_eq!(encoded, encoded_frame(FrameType::Exit as u16, 3, &[1, 2, 3]));
}

#[test]
fn write_rejects_oversized_frame() {
    let mut encoded = Vec::new();
    let error = Frame::new(FrameType::Stdout, vec![0; MAX_FRAME_PAYLOAD + 1]).write(&mut encoded).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(encoded.is_empty());
}

#[tokio::test]
async fn async_read_fragmented_frame() {
    let payload = b"fragmented async";
    let mut reader = FragmentedReader::new(encoded_frame(FrameType::Stdout as u16, payload.len() as u32, payload), 1);
    let frame = Frame::read_async(&mut reader).await.unwrap();
    assert!(matches!(frame.frame_type, FrameType::Stdout));
    assert_eq!(frame.payload, payload);
}

#[tokio::test]
async fn async_read_zero_length_frame() {
    let mut reader = FragmentedReader::new(encoded_frame(FrameType::Stdout as u16, 0, &[]), 1);
    let frame = Frame::read_async(&mut reader).await.unwrap();
    assert!(matches!(frame.frame_type, FrameType::Stdout));
    assert!(frame.payload.is_empty());
}

#[tokio::test]
async fn async_read_maximum_size_frame() {
    let payload = vec![0x5A; MAX_FRAME_PAYLOAD];
    let mut reader = FragmentedReader::new(encoded_frame(FrameType::Stdout as u16, MAX_FRAME_PAYLOAD as u32, &payload), MAX_FRAME_PAYLOAD);
    let frame = Frame::read_async(&mut reader).await.unwrap();
    assert_eq!(frame.payload, payload);
}

#[tokio::test]
async fn async_read_oversized_frame() {
    let mut reader = FragmentedReader::new(encoded_frame(FrameType::Stdout as u16, (MAX_FRAME_PAYLOAD + 1) as u32, &[]), 1);
    let error = frame_error(Frame::read_async(&mut reader).await);
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[tokio::test]
async fn async_read_unknown_frame_type() {
    let mut reader = FragmentedReader::new(encoded_frame(99, 0, &[]), 1);
    let error = frame_error(Frame::read_async(&mut reader).await);
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("unknown frame type"));
}

#[tokio::test]
async fn async_read_truncated_header() {
    let mut reader = FragmentedReader::new(vec![1, 0, 0], 1);
    let error = frame_error(Frame::read_async(&mut reader).await);
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
}

#[tokio::test]
async fn async_read_truncated_payload() {
    let mut reader = FragmentedReader::new(encoded_frame(FrameType::Stdout as u16, 4, &[1, 2]), 1);
    let error = frame_error(Frame::read_async(&mut reader).await);
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
}

#[tokio::test]
async fn async_write_rejects_oversized_frame() {
    let mut encoded = Vec::new();
    let error = Frame::new(FrameType::Stdout, vec![0; MAX_FRAME_PAYLOAD + 1]).write_async(&mut encoded).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(encoded.is_empty());
}
