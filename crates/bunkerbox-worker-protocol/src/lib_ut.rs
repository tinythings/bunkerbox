use super::*;
use std::io::{self, Cursor, Read};
use tokio::io::AsyncWriteExt;

fn ids() -> (WorkerRequestId, WorkerSessionId, WorkerUploadId) {
    (WorkerRequestId([1; 16]), WorkerSessionId([2; 16]), WorkerUploadId([3; 16]))
}

fn file_entry(path: &str) -> WorkerUploadEntry {
    WorkerUploadEntry::file(path, 0o644, 4, [9; WORKER_DIGEST_LEN]).unwrap()
}

fn build() -> WorkerBuild {
    WorkerBuild::new(
        "make",
        "/usr/bin/make",
        vec!["release mode".into(), "$(literal); still one arg".into()],
        "src",
        vec![("CC".into(), "gcc".into())],
        vec![("PATH".into(), "/usr/bin".into())],
        WorkerUploadId([3; 16]),
    )
    .unwrap()
}

fn messages() -> Vec<WorkerMessage> {
    let (request_id, session_id, upload_id) = ids();
    vec![
        WorkerMessage::Hello { request_id, session_id, version: WORKER_PROTOCOL_VERSION, response: false },
        WorkerMessage::UploadBegin {
            request_id,
            session_id,
            upload_id,
            entries: vec![WorkerUploadEntry::directory("src", 0o755).unwrap(), file_entry("src/main.rs")],
        },
        WorkerMessage::UploadEntry { request_id, session_id, upload_id, entry_index: 1, entry: file_entry("src/main.rs") },
        WorkerMessage::UploadFileChunk {
            request_id,
            session_id,
            upload_id,
            path: WorkerRelativePath::new("src/main.rs").unwrap(),
            offset: 0,
            data: b"data".to_vec(),
        },
        WorkerMessage::UploadComplete { request_id, session_id, upload_id },
        WorkerMessage::Build { request_id, session_id, build: build() },
        WorkerMessage::Cleanup { request_id, session_id, upload_token: upload_id },
        WorkerMessage::SyncProgress { request_id, session_id, upload_id, completed_bytes: 4, total_bytes: Some(8) },
        WorkerMessage::Stdout { request_id, session_id, data: b"out".to_vec() },
        WorkerMessage::Stderr { request_id, session_id, data: b"err".to_vec() },
        WorkerMessage::Completed { request_id, session_id, operation: WorkerOperation::Build, exit_code: -17 },
        WorkerMessage::Error {
            request_id,
            session_id,
            operation: WorkerOperation::Cleanup,
            kind: WorkerErrorKind::Cleanup,
            message: "cleanup failed".into(),
        },
    ]
}

#[test]
fn all_message_variants_round_trip() {
    for message in messages() {
        let frame = message.encode().unwrap();
        assert_eq!(&frame[..4], &WORKER_PROTOCOL_MAGIC);
        assert_eq!(frame[4..6], WORKER_PROTOCOL_VERSION.to_le_bytes());
        assert_eq!(frame[6], message.kind().as_u8());
        assert_eq!(WorkerMessage::decode(&frame).unwrap(), message);
    }
}

#[test]
fn blocking_helpers_preserve_the_async_wire_encoding() {
    let message = WorkerMessage::Stdout { request_id: ids().0, session_id: ids().1, data: b"blocking parity".to_vec() };
    let mut encoded = Vec::new();
    message.write_blocking(&mut encoded).unwrap();
    assert_eq!(WorkerMessage::decode(&encoded).unwrap(), message);
    assert_eq!(WorkerMessage::read_blocking(&mut Cursor::new(encoded)).unwrap(), message);
}

#[test]
fn blocking_reader_handles_fragmented_frames_and_clean_eof() {
    let message = WorkerMessage::Stderr { request_id: ids().0, session_id: ids().1, data: b"fragmented".to_vec() };
    let reader = OneByteReader { bytes: message.encode().unwrap(), offset: 0 };
    let mut reader = reader;
    assert_eq!(WorkerMessage::read_blocking_optional(&mut reader).unwrap(), Some(message));
    assert_eq!(WorkerMessage::read_blocking_optional(&mut reader).unwrap(), None);
}

struct OneByteReader {
    bytes: Vec<u8>,
    offset: usize,
}

impl Read for OneByteReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.offset == self.bytes.len() {
            return Ok(0);
        }
        buffer[0] = self.bytes[self.offset];
        self.offset += 1;
        Ok(1)
    }
}

#[tokio::test]
async fn async_helpers_handle_fragmented_duplex_frames() {
    let message = WorkerMessage::Build { request_id: ids().0, session_id: ids().1, build: build() };
    let frame = message.encode().unwrap();
    let (mut reader, mut writer) = tokio::io::duplex(3);
    let sender = tokio::spawn(async move {
        for part in frame.chunks(2) {
            writer.write_all(part).await.unwrap();
            tokio::task::yield_now().await;
        }
    });
    let decoded = WorkerMessage::read_async(&mut reader).await.unwrap();
    sender.await.unwrap();
    assert_eq!(decoded, message);
}

#[tokio::test]
async fn async_write_helper_emits_a_decodable_frame() {
    let message = WorkerMessage::Stdout { request_id: ids().0, session_id: ids().1, data: b"exact".to_vec() };
    let (mut reader, mut writer) = tokio::io::duplex(128);
    let expected = message.clone();
    let sender = tokio::spawn(async move { message.write_async(&mut writer).await.unwrap() });
    assert_eq!(WorkerMessage::read_async(&mut reader).await.unwrap(), expected);
    sender.await.unwrap();
}

#[test]
fn malformed_headers_and_lengths_are_rejected() {
    let frame =
        WorkerMessage::Hello { request_id: ids().0, session_id: ids().1, version: WORKER_PROTOCOL_VERSION, response: false }.encode().unwrap();

    let mut bad_magic = frame.clone();
    bad_magic[0] ^= 1;
    assert!(WorkerMessage::decode(&bad_magic).is_err());

    let mut bad_version = frame.clone();
    bad_version[4..6].copy_from_slice(&(WORKER_PROTOCOL_VERSION + 1).to_le_bytes());
    assert!(WorkerMessage::decode(&bad_version).is_err());

    let mut bad_kind = frame.clone();
    bad_kind[6] = 255;
    assert!(WorkerMessage::decode(&bad_kind).is_err());

    let mut oversized = frame[..WORKER_FRAME_HEADER_LEN].to_vec();
    oversized[7..11].copy_from_slice(&((MAX_WORKER_FRAME_PAYLOAD as u32) + 1).to_le_bytes());
    assert!(WorkerMessage::decode(&oversized).is_err());

    assert!(WorkerMessage::decode(&frame[..frame.len() - 1]).is_err());
    let mut extra = frame.clone();
    extra.push(0);
    assert!(WorkerMessage::decode(&extra).is_err());
}

#[test]
fn invalid_utf8_and_trailing_payload_are_rejected() {
    let (request_id, session_id, _) = ids();
    let mut frame =
        WorkerMessage::Error { request_id, session_id, operation: WorkerOperation::Build, kind: WorkerErrorKind::Build, message: "x".into() }
            .encode()
            .unwrap();
    let message_start = WORKER_FRAME_HEADER_LEN + 32 + 1 + 1 + 4;
    frame[message_start] = 0xff;
    assert!(WorkerMessage::decode(&frame).is_err());

    let mut hello = WorkerMessage::hello(request_id, session_id, false).encode().unwrap();
    hello.push(0);
    assert!(WorkerMessage::decode(&hello).is_err());
}

#[test]
fn paths_tools_and_executables_are_strictly_validated() {
    for path in ["/absolute", "../parent", "a/../b", "./name", "a//b", "a\\b", ""] {
        assert!(validate_worker_relative_path(path).is_err(), "accepted path {path:?}");
    }
    assert!(validate_worker_relative_path("src/main.rs").is_ok());
    assert!(validate_worker_relative_path(&"a".repeat(MAX_WORKER_PATH_COMPONENT_BYTES + 1)).is_err());
    assert!(WorkerTool::new("make;rm").is_err());
    assert!(WorkerTool::new("/usr/bin/make").is_err());
    assert!(WorkerExecutablePath::new("make").is_err());
    assert!(WorkerExecutablePath::new("/usr/bin/make -f").is_err());
    assert!(WorkerExecutablePath::new("/usr/bin/../bin/make").is_err());
    assert!(WorkerExecutablePath::new("/usr/bin/make").is_ok());
}

#[test]
fn manifests_require_sorted_paths_and_valid_metadata() {
    assert!(validate_upload_manifest(&[file_entry("z"), file_entry("a")]).is_err());
    assert!(WorkerUploadEntry::new("file", WorkerEntryKind::File, 0o644, 1, None).is_err());
    assert!(WorkerUploadEntry::new("dir", WorkerEntryKind::Directory, 0o755, 1, None).is_err());
    assert!(WorkerUploadEntry::new("dir", WorkerEntryKind::Directory, 0o755, 0, Some([1; 32])).is_err());
    assert!(WorkerUploadEntry::new("file", WorkerEntryKind::File, 0o100000, 1, Some([1; 32])).is_err());
    assert!(WorkerUploadEntry::new("file", WorkerEntryKind::File, 0o644, 1, Some([1; 32])).is_ok());
}

#[test]
fn duplicate_environment_keys_and_bad_argv_are_rejected() {
    assert!(WorkerBuild::new(
        "make",
        "/usr/bin/make",
        vec!["x".into()],
        "",
        vec![("CC".into(), "one".into()), ("CC".into(), "two".into())],
        Vec::new(),
        ids().2,
    )
    .is_err());

    assert!(WorkerBuild::new("make", "/usr/bin/make", vec!["x\0y".into()], "", Vec::new(), Vec::new(), ids().2,).is_err());

    assert!(WorkerBuild::new("make", "/usr/bin/make", Vec::new(), "", vec![("BAD-NAME".into(), "value".into())], Vec::new(), ids().2,).is_err());

    assert!(WorkerBuild::new("make", "/usr/bin/make", vec!["arg".into(); MAX_WORKER_ARG_COUNT + 1], "", Vec::new(), Vec::new(), ids().2,).is_err());

    assert!(WorkerBuild::new(
        "make",
        "/usr/bin/make",
        Vec::new(),
        "",
        vec![("A".into(), "value".into()); MAX_WORKER_ENV_COUNT + 1],
        Vec::new(),
        ids().2,
    )
    .is_err());
}

#[test]
fn bounded_chunks_output_errors_and_progress_are_rejected() {
    let (request_id, session_id, upload_id) = ids();
    let path = WorkerRelativePath::new("file").unwrap();
    assert!(WorkerMessage::UploadFileChunk { request_id, session_id, upload_id, path: path.clone(), offset: 0, data: Vec::new() }.encode().is_err());
    assert!(WorkerMessage::UploadFileChunk { request_id, session_id, upload_id, path, offset: 0, data: vec![0; MAX_WORKER_CHUNK_BYTES + 1] }
        .encode()
        .is_err());
    assert!(WorkerMessage::Stdout { request_id, session_id, data: vec![0; MAX_WORKER_OUTPUT_BYTES + 1] }.encode().is_err());
    assert!(WorkerMessage::Error {
        request_id,
        session_id,
        operation: WorkerOperation::Build,
        kind: WorkerErrorKind::Build,
        message: "x".repeat(MAX_WORKER_ERROR_BYTES + 1),
    }
    .encode()
    .is_err());
    assert!(WorkerMessage::SyncProgress { request_id, session_id, upload_id, completed_bytes: 2, total_bytes: Some(1) }.encode().is_err());
}

#[test]
fn stdout_stderr_and_exact_completion_preserve_bytes_and_exit_code() {
    let (request_id, session_id, _) = ids();
    let stdout = WorkerMessage::Stdout { request_id, session_id, data: b"out\0with bytes".to_vec() };
    let stderr = WorkerMessage::Stderr { request_id, session_id, data: b"err".to_vec() };
    let completed = WorkerMessage::Completed { request_id, session_id, operation: WorkerOperation::Build, exit_code: i32::MIN };
    assert_eq!(WorkerMessage::decode(&stdout.encode().unwrap()).unwrap(), stdout);
    assert_eq!(WorkerMessage::decode(&stderr.encode().unwrap()).unwrap(), stderr);
    assert_eq!(WorkerMessage::decode(&completed.encode().unwrap()).unwrap(), completed);
}

#[test]
fn build_has_no_shell_packing_and_keeps_literal_arguments() {
    let message = WorkerMessage::Build { request_id: ids().0, session_id: ids().1, build: build() };
    let decoded = WorkerMessage::decode(&message.encode().unwrap()).unwrap();
    let WorkerMessage::Build { build, .. } = decoded else { panic!("expected build") };
    assert_eq!(build.argv, vec!["release mode", "$(literal); still one arg"]);
    assert_eq!(build.argv.len(), 2);
    assert_eq!(build.tool.as_str(), "make");
    assert_eq!(build.trusted_executable.as_str(), "/usr/bin/make");
}

#[test]
fn error_kinds_preserve_protocol_build_and_cleanup_failures() {
    let (request_id, session_id, _) = ids();
    for (operation, kind) in [
        (WorkerOperation::Protocol, WorkerErrorKind::WorkerProtocol),
        (WorkerOperation::Build, WorkerErrorKind::Build),
        (WorkerOperation::Cleanup, WorkerErrorKind::Cleanup),
    ] {
        let message = WorkerMessage::Error { request_id, session_id, operation, kind, message: "failure".into() };
        assert_eq!(WorkerMessage::decode(&message.encode().unwrap()).unwrap(), message);
    }
}

#[test]
fn unknown_nested_kinds_and_flags_are_rejected() {
    let message = WorkerMessage::UploadBegin { request_id: ids().0, session_id: ids().1, upload_id: ids().2, entries: Vec::new() };
    let mut frame = message.encode().unwrap();
    frame[WORKER_FRAME_HEADER_LEN + 48..WORKER_FRAME_HEADER_LEN + 52].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(WorkerMessage::decode(&frame).is_err());

    let hello = WorkerMessage::hello(ids().0, ids().1, false);
    let mut frame = hello.encode().unwrap();
    frame[WORKER_FRAME_HEADER_LEN + 34] = 9;
    assert!(WorkerMessage::decode(&frame).is_err());
}
