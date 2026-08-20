use super::*;
use crate::platform;
use bunkerbox_worker_protocol::{
    WorkerArtifactPath, WorkerBuild, WorkerEntryKind, WorkerMessage, WorkerOperation, WorkerRequestId, WorkerSessionId, WorkerUploadEntry,
    WorkerUploadId, WORKER_ARTIFACT_PROTOCOL_VERSION,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use tempfile::{tempdir, TempDir};

const REQUEST_ID: WorkerRequestId = WorkerRequestId([1; 16]);
const SESSION_ID: WorkerSessionId = WorkerSessionId([2; 16]);
const UPLOAD_ID: WorkerUploadId = WorkerUploadId([3; 16]);

struct Fixture {
    _temp: TempDir,
    root: std::fs::File,
    script: String,
}

fn fixture() -> Fixture {
    let temp = tempdir().unwrap();
    let root_path = temp.path().join("worker-root");
    fs::create_dir(&root_path).unwrap();
    fs::set_permissions(&root_path, fs::Permissions::from_mode(0o700)).unwrap();
    let script_path = temp.path().join("tool.sh");
    fs::write(&script_path, b"#!/bin/sh\nprintf '%s\\n' \"$CHECK\"\nprintf 'stdout:%s\\n' \"$1\"\nprintf 'stderr:%s\\n' \"$2\" >&2\nexit 7\n")
        .unwrap();
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
    let root = platform::open_root(&root_path).unwrap();
    Fixture { _temp: temp, root, script: script_path.to_string_lossy().into_owned() }
}

fn file_entry(path: &str, contents: &[u8]) -> WorkerUploadEntry {
    let digest: [u8; 32] = Sha256::digest(contents).into();
    WorkerUploadEntry::file(path, 0o644, contents.len() as u64, digest).unwrap()
}

fn upload_messages(contents: &[u8]) -> Vec<WorkerMessage> {
    vec![
        WorkerMessage::hello(REQUEST_ID, SESSION_ID, false),
        WorkerMessage::UploadBegin {
            request_id: REQUEST_ID,
            session_id: SESSION_ID,
            upload_id: UPLOAD_ID,
            entries: vec![WorkerUploadEntry::directory("src", 0o755).unwrap(), file_entry("src/input.txt", contents)],
        },
        WorkerMessage::UploadFileChunk {
            request_id: REQUEST_ID,
            session_id: SESSION_ID,
            upload_id: UPLOAD_ID,
            path: bunkerbox_worker_protocol::WorkerRelativePath::new("src/input.txt").unwrap(),
            offset: 0,
            data: contents.to_vec(),
        },
        WorkerMessage::UploadComplete { request_id: REQUEST_ID, session_id: SESSION_ID, upload_id: UPLOAD_ID },
    ]
}

fn encode_messages(messages: &[WorkerMessage]) -> Vec<u8> {
    messages.iter().flat_map(|message| message.encode().unwrap()).collect()
}

fn run_messages<W: std::io::Write + Send>(service: &WorkerService, messages: Vec<WorkerMessage>, writer: &FrameWriter<W>) -> Result<(), String> {
    service.run(Cursor::new(encode_messages(&messages)), writer)
}

fn decode_output(bytes: &[u8]) -> Vec<WorkerMessage> {
    let mut reader = Cursor::new(bytes);
    let mut messages = Vec::new();
    loop {
        match WorkerMessage::read_blocking_optional(&mut reader).unwrap() {
            Some(message) => messages.push(message),
            None => return messages,
        }
    }
}

fn output_bytes(writer: FrameWriter<Vec<u8>>) -> Vec<u8> {
    writer.into_inner().unwrap()
}

fn build_message(script: &str) -> WorkerMessage {
    WorkerMessage::Build {
        request_id: WorkerRequestId([4; 16]),
        session_id: SESSION_ID,
        build: WorkerBuild::new(
            "tool",
            script,
            vec!["literal space".to_string(), "$(not-a-shell-expansion)".to_string()],
            "src",
            vec![("CHECK".to_string(), "guest".to_string()), ("PATH".to_string(), "guest-path".to_string())],
            vec![("CHECK".to_string(), "target".to_string()), ("PATH".to_string(), "/usr/bin:/bin".to_string())],
            UPLOAD_ID,
        )
        .unwrap(),
    }
}

#[test]
fn upload_persists_across_worker_instances_and_build_cleans_it() {
    let fixture = fixture();
    let contents = b"snapshot contents\n";
    let upload_service = WorkerService::new(&fixture.root).unwrap();
    let upload_writer = FrameWriter::new(Vec::new());
    run_messages(&upload_service, upload_messages(contents), &upload_writer).unwrap();
    let upload_output = decode_output(&output_bytes(upload_writer));
    assert!(matches!(upload_output.as_slice(), [WorkerMessage::Hello { response: true, .. }, WorkerMessage::UploadComplete { .. }]));
    drop(upload_service);

    let build_service = WorkerService::new(&fixture.root).unwrap();
    let build = build_message(&fixture.script);
    let WorkerMessage::Build { request_id, session_id, build } = build.clone() else { unreachable!() };
    let build_writer = FrameWriter::new(Vec::new());
    let messages = vec![
        WorkerMessage::hello(WorkerRequestId([4; 16]), SESSION_ID, false),
        WorkerMessage::Build { request_id, session_id, build: build.clone() },
        WorkerMessage::Cleanup { request_id, session_id, upload_token: UPLOAD_ID },
    ];
    run_messages(&build_service, messages, &build_writer).unwrap();
    let output = decode_output(&output_bytes(build_writer));
    assert!(output
        .iter()
        .any(|message| matches!(message, WorkerMessage::Stdout { data, .. } if data.windows(b"target".len()).any(|window| window == b"target"))));
    assert!(output.iter().any(|message| matches!(message, WorkerMessage::Stdout { data, .. } if data.windows(b"stdout:literal space".len()).any(|window| window == b"stdout:literal space"))));
    assert!(output.iter().any(|message| matches!(message, WorkerMessage::Stderr { data, .. } if data.windows(b"stderr:$(not-a-shell-expansion)".len()).any(|window| window == b"stderr:$(not-a-shell-expansion)"))));
    assert!(output.iter().any(|message| matches!(message, WorkerMessage::Completed { operation: WorkerOperation::Build, exit_code: 7, .. })));
    assert!(output.iter().any(|message| matches!(message, WorkerMessage::Completed { operation: WorkerOperation::Cleanup, exit_code: 0, .. })));

    let missing = build_service;
    let missing_writer = FrameWriter::new(Vec::new());
    let missing_messages = vec![WorkerMessage::hello(WorkerRequestId([5; 16]), SESSION_ID, false), build_message(&fixture.script)];
    run_messages(&missing, missing_messages, &missing_writer).unwrap();
    let missing_output = decode_output(&output_bytes(missing_writer));
    assert!(missing_output.iter().any(|message| matches!(
        message,
        WorkerMessage::Error { operation: WorkerOperation::Upload, kind: bunkerbox_worker_protocol::WorkerErrorKind::Upload, .. }
    )));
}

#[test]
fn malformed_uploads_are_not_buildable() {
    let fixture = fixture();
    let service = WorkerService::new(&fixture.root).unwrap();
    let entry = file_entry("src/input.txt", b"expected");
    let messages = vec![
        WorkerMessage::hello(REQUEST_ID, SESSION_ID, false),
        WorkerMessage::UploadBegin {
            request_id: REQUEST_ID,
            session_id: SESSION_ID,
            upload_id: UPLOAD_ID,
            entries: vec![WorkerUploadEntry::directory("src", 0o755).unwrap(), entry],
        },
        WorkerMessage::UploadFileChunk {
            request_id: REQUEST_ID,
            session_id: SESSION_ID,
            upload_id: UPLOAD_ID,
            path: bunkerbox_worker_protocol::WorkerRelativePath::new("src/input.txt").unwrap(),
            offset: 1,
            data: b"wrong".to_vec(),
        },
    ];
    let writer = FrameWriter::new(Vec::new());
    run_messages(&service, messages, &writer).unwrap();
    let output = decode_output(&output_bytes(writer));
    assert!(output.iter().any(|message| matches!(message, WorkerMessage::Error { operation: WorkerOperation::Upload, .. })));
    drop(service);

    let service = WorkerService::new(&fixture.root).unwrap();
    let writer = FrameWriter::new(Vec::new());
    let messages = vec![WorkerMessage::hello(WorkerRequestId([6; 16]), SESSION_ID, false), build_message(&fixture.script)];
    run_messages(&service, messages, &writer).unwrap();
    let output = decode_output(&output_bytes(writer));
    assert!(output.iter().any(|message| matches!(message, WorkerMessage::Error { operation: WorkerOperation::Upload, .. })));
}

#[test]
fn root_and_protocol_paths_are_confined() {
    let fixture = fixture();
    assert!(platform::open_root(Path::new(&fixture.script)).is_err());
    assert!(bunkerbox_worker_protocol::WorkerUploadEntry::file("../escape", 0o644, 1, [0; 32]).is_err());
    assert!(bunkerbox_worker_protocol::WorkerUploadEntry::new("src/a", WorkerEntryKind::Directory, 0o755, 0, None).is_ok());
}

#[test]
fn artifact_capable_build_emits_manifest_fetches_from_spool_and_cleans() {
    let fixture = fixture();
    let success_script = fixture._temp.path().join("success.sh");
    fs::write(&success_script, b"#!/bin/sh\nprintf 'data' > result\nexit 0\n").unwrap();
    fs::set_permissions(&success_script, fs::Permissions::from_mode(0o755)).unwrap();

    let (mut host, worker) = UnixStream::pair().unwrap();
    let worker_input = worker.try_clone().unwrap();
    let service = WorkerService::new(&fixture.root).unwrap();
    let worker_thread = thread::spawn(move || {
        let writer = FrameWriter::new(worker);
        service.run(worker_input, &writer)
    });

    let request = WorkerRequestId([4; 16]);
    write_v2(&mut host, &WorkerMessage::hello_for_version(request, SESSION_ID, false, WORKER_ARTIFACT_PROTOCOL_VERSION));
    write_v2(
        &mut host,
        &WorkerMessage::UploadBegin {
            request_id: request,
            session_id: SESSION_ID,
            upload_id: UPLOAD_ID,
            entries: vec![WorkerUploadEntry::directory("src", 0o755).unwrap(), file_entry("src/input", b"input")],
        },
    );
    write_v2(
        &mut host,
        &WorkerMessage::UploadFileChunk {
            request_id: request,
            session_id: SESSION_ID,
            upload_id: UPLOAD_ID,
            path: bunkerbox_worker_protocol::WorkerRelativePath::new("src/input").unwrap(),
            offset: 0,
            data: b"input".to_vec(),
        },
    );
    write_v2(&mut host, &WorkerMessage::UploadComplete { request_id: request, session_id: SESSION_ID, upload_id: UPLOAD_ID });
    let build = WorkerBuild::new("tool", success_script.to_string_lossy().into_owned(), Vec::new(), "src", Vec::new(), Vec::new(), UPLOAD_ID)
        .unwrap()
        .with_artifacts(vec![WorkerArtifactPath::new("src/result").unwrap()], 1024, 2048)
        .unwrap();
    write_v2(&mut host, &WorkerMessage::Build { request_id: request, session_id: SESSION_ID, build });

    let artifact_set_id = loop {
        let (_, message) = WorkerMessage::read_blocking_versioned(&mut host).unwrap();
        match message {
            WorkerMessage::ArtifactManifest { artifact_set_id: id, entries, total_bytes, .. } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].path().as_str(), "src/result");
                assert_eq!(entries[0].size(), 4);
                assert_eq!(total_bytes, 4);
                break id;
            }
            WorkerMessage::Hello { .. } | WorkerMessage::UploadComplete { .. } | WorkerMessage::Completed { .. } => {}
            other => panic!("unexpected worker message: {other:?}"),
        }
    };
    write_v2(&mut host, &WorkerMessage::FetchArtifact { request_id: request, session_id: SESSION_ID, artifact_set_id, entry_index: 0 });
    let (_, chunk) = WorkerMessage::read_blocking_versioned(&mut host).unwrap();
    assert!(matches!(chunk, WorkerMessage::ArtifactChunk { offset: 0, data, .. } if data == b"data"));
    let (_, complete) = WorkerMessage::read_blocking_versioned(&mut host).unwrap();
    assert!(matches!(complete, WorkerMessage::ArtifactComplete { entry_index: 0, .. }));
    write_v2(&mut host, &WorkerMessage::Cleanup { request_id: request, session_id: SESSION_ID, upload_token: UPLOAD_ID });
    let (_, cleanup) = WorkerMessage::read_blocking_versioned(&mut host).unwrap();
    assert!(matches!(cleanup, WorkerMessage::Completed { operation: WorkerOperation::Cleanup, exit_code: 0, .. }));
    drop(host);
    assert_eq!(worker_thread.join().unwrap(), Ok(()));
}

fn write_v2(stream: &mut UnixStream, message: &WorkerMessage) {
    message.write_blocking_version(stream, WORKER_ARTIFACT_PROTOCOL_VERSION).unwrap();
}
