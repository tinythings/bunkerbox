use crate::process::{self, JobWorkspace, OutputSink};
use crate::storage::{ArtifactSpool, UploadStore, UploadTransaction};
use bunkerbox_worker_protocol::{
    WorkerErrorKind, WorkerMessage, WorkerOperation, WorkerRequestId, WorkerSessionId, WorkerUploadId, MAX_WORKER_CHUNK_BYTES,
    MAX_WORKER_ERROR_BYTES, WORKER_ARTIFACT_PROTOCOL_VERSION, WORKER_PROTOCOL_VERSION,
};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

pub struct FrameWriter<W: Write + Send> {
    writer: Mutex<W>,
    version: AtomicU16,
}

impl<W: Write + Send> FrameWriter<W> {
    pub fn new(writer: W) -> Self {
        Self { writer: Mutex::new(writer), version: AtomicU16::new(WORKER_PROTOCOL_VERSION) }
    }

    pub fn set_version(&self, version: u16) {
        self.version.store(version, Ordering::Release);
    }

    pub fn send_message(&self, message: &WorkerMessage) -> Result<(), String> {
        let mut writer = self.writer.lock().map_err(|_| "worker protocol writer lock poisoned".to_string())?;
        message.write_blocking_version(&mut *writer, self.version.load(Ordering::Acquire)).map_err(|error| error.to_string())
    }

    #[cfg(test)]
    pub(crate) fn into_inner(self) -> Result<W, String> {
        self.writer.into_inner().map_err(|_| "worker protocol writer lock poisoned".to_string())
    }
}

impl<W: Write + Send> OutputSink for FrameWriter<W> {
    fn send(&self, message: WorkerMessage) -> Result<(), String> {
        self.send_message(&message)
    }
}

pub struct WorkerService {
    store: UploadStore,
}

impl WorkerService {
    pub fn new(root: &std::fs::File) -> Result<Self, String> {
        let store = UploadStore::new(root)?;
        let jobs = store.jobs_directory()?;
        process::cleanup_stale_jobs(&jobs).map_err(|error| format!("clean stale worker jobs: {error}"))?;
        Ok(Self { store })
    }

    pub fn run<R: Read + Send + 'static, W: Write + Send>(&self, input: R, writer: &FrameWriter<W>) -> Result<(), String> {
        let input = InputChannel::spawn(input);
        let Some((hello_version, hello)) = input.next()? else {
            return Ok(());
        };
        let (request_id, session_id, response, version) = match hello {
            WorkerMessage::Hello { request_id, session_id, response, version } => (request_id, session_id, response, version),
            _ => return Err("worker did not receive Hello first".to_string()),
        };
        if response {
            return Err("worker received a Hello response instead of a request".to_string());
        }
        if version != hello_version {
            return Err("worker Hello version does not match frame version".to_string());
        }
        if version != WORKER_PROTOCOL_VERSION && version != WORKER_ARTIFACT_PROTOCOL_VERSION {
            return Err(format!("unsupported worker protocol version: {version}"));
        }
        if session_id.0 == [0; 16] {
            return Err("worker session ID must be nonzero".to_string());
        }
        writer.set_version(version);
        writer.send_message(&WorkerMessage::hello_for_version(request_id, session_id, true, version))?;

        let mut active_upload: Option<ActiveUpload> = None;
        loop {
            let Some((message_version, message)) = input.next()? else {
                return Ok(());
            };
            if message_version != version {
                send_error(
                    writer,
                    message.request_id(),
                    session_id,
                    WorkerOperation::Protocol,
                    WorkerErrorKind::WorkerProtocol,
                    "worker frame version changed during connection",
                )?;
                return Ok(());
            }
            if message.session_id() != session_id {
                send_error(
                    writer,
                    message.request_id(),
                    session_id,
                    WorkerOperation::Protocol,
                    WorkerErrorKind::WorkerProtocol,
                    "worker session correlation mismatch",
                )?;
                return Ok(());
            }

            if let Some(active) = active_upload.as_mut() {
                match message {
                    WorkerMessage::UploadFileChunk { request_id: received_request, session_id: received_session, upload_id, path, offset, data } => {
                        if received_request != active.request_id || received_session != session_id || upload_id != active.upload_id {
                            send_error(
                                writer,
                                received_request,
                                session_id,
                                WorkerOperation::Upload,
                                WorkerErrorKind::WorkerProtocol,
                                "worker upload correlation mismatch",
                            )?;
                            return Ok(());
                        }
                        if let Err(error) = active.transaction.accept_chunk(&path, offset, &data) {
                            send_error(writer, received_request, session_id, WorkerOperation::Upload, WorkerErrorKind::Upload, &error)?;
                            return Ok(());
                        }
                    }
                    WorkerMessage::UploadComplete { request_id: received_request, session_id: received_session, upload_id } => {
                        if received_request != active.request_id || received_session != session_id || upload_id != active.upload_id {
                            send_error(
                                writer,
                                received_request,
                                session_id,
                                WorkerOperation::Upload,
                                WorkerErrorKind::WorkerProtocol,
                                "worker upload completion correlation mismatch",
                            )?;
                            return Ok(());
                        }
                        if let Err(error) = active.transaction.commit() {
                            send_error(writer, received_request, session_id, WorkerOperation::Upload, WorkerErrorKind::Upload, &error)?;
                            return Ok(());
                        }
                        writer.send_message(&WorkerMessage::UploadComplete { request_id: received_request, session_id, upload_id })?;
                        active_upload = None;
                    }
                    _ => {
                        send_error(
                            writer,
                            message.request_id(),
                            session_id,
                            WorkerOperation::Upload,
                            WorkerErrorKind::WorkerProtocol,
                            "unexpected message during worker upload",
                        )?;
                        return Ok(());
                    }
                }
                continue;
            }

            match message {
                WorkerMessage::UploadBegin { request_id, session_id, upload_id, entries } => match self.store.begin(session_id, upload_id, entries) {
                    Ok(transaction) => active_upload = Some(ActiveUpload { request_id, upload_id, transaction }),
                    Err(error) => {
                        send_error(writer, request_id, session_id, WorkerOperation::Upload, WorkerErrorKind::Upload, &error)?;
                        return Ok(());
                    }
                },
                WorkerMessage::Build { request_id, session_id, build } => {
                    self.handle_build(&input, writer, request_id, session_id, build, version)?;
                    return Ok(());
                }
                _ => {
                    send_error(
                        writer,
                        message.request_id(),
                        session_id,
                        WorkerOperation::Protocol,
                        WorkerErrorKind::WorkerProtocol,
                        "unexpected worker message",
                    )?;
                    return Ok(());
                }
            }
        }
    }

    fn handle_build<W: Write + Send>(
        &self, input: &InputChannel, writer: &FrameWriter<W>, request_id: WorkerRequestId, session_id: WorkerSessionId,
        build: bunkerbox_worker_protocol::WorkerBuild, protocol_version: u16,
    ) -> Result<(), String> {
        let upload = match self.store.open_completed(session_id, build.upload_token()) {
            Ok(upload) => upload,
            Err(error) => {
                send_error(writer, request_id, session_id, WorkerOperation::Upload, WorkerErrorKind::Upload, &error)?;
                return Ok(());
            }
        };
        let jobs = self.store.jobs_directory()?;
        let job = match JobWorkspace::create(&jobs) {
            Ok(job) => job,
            Err(error) => {
                send_error(writer, request_id, session_id, WorkerOperation::Build, WorkerErrorKind::Build, &error)?;
                return Ok(());
            }
        };
        if let Err(error) = upload.materialize(job.root()) {
            send_error(writer, request_id, session_id, WorkerOperation::Upload, WorkerErrorKind::Upload, &error)?;
            return Ok(());
        }
        let exit_code = match process::execute_build(&job, &build, request_id, session_id, writer, &|| input.disconnected()) {
            Ok(exit_code) => exit_code,
            Err(error) => {
                send_error(writer, request_id, session_id, WorkerOperation::Build, WorkerErrorKind::Build, &error)?;
                return Ok(());
            }
        };
        let artifact_spool = if exit_code == 0 && !build.artifact_paths().is_empty() {
            match ArtifactSpool::capture(&jobs, job.root(), build.artifact_paths(), build.artifact_max_file_bytes(), build.artifact_max_total_bytes())
            {
                Ok(spool) => Some(spool),
                Err(error) => {
                    send_error(writer, request_id, session_id, WorkerOperation::Artifact, WorkerErrorKind::Artifact, &error)?;
                    return self.wait_for_cleanup(
                        input,
                        writer,
                        BuildCleanup { request_id, session_id, upload_token: build.upload_token(), protocol_version },
                    );
                }
            }
        } else {
            None
        };

        writer.send_message(&WorkerMessage::completed(request_id, session_id, WorkerOperation::Build, exit_code))?;
        if let Some(spool) = artifact_spool {
            writer.send_message(&WorkerMessage::ArtifactManifest {
                request_id,
                session_id,
                artifact_set_id: spool.artifact_set_id(),
                entries: spool.entries().to_vec(),
                total_bytes: spool.total_bytes(),
            })?;
            return self.wait_for_artifacts(
                input,
                writer,
                BuildCleanup { request_id, session_id, upload_token: build.upload_token(), protocol_version },
                spool,
            );
        }
        self.wait_for_cleanup(input, writer, BuildCleanup { request_id, session_id, upload_token: build.upload_token(), protocol_version })
    }

    fn wait_for_cleanup<W: Write + Send>(&self, input: &InputChannel, writer: &FrameWriter<W>, cleanup: BuildCleanup) -> Result<(), String> {
        let BuildCleanup { request_id, session_id, upload_token, protocol_version } = cleanup;
        loop {
            let Some((version, cleanup)) = input.next()? else {
                return Ok(());
            };
            if version != protocol_version {
                return Err("worker cleanup frame version changed".to_string());
            }
            match cleanup {
                WorkerMessage::Cleanup { request_id: cleanup_request, session_id: cleanup_session, upload_token: received_upload }
                    if cleanup_request == request_id && cleanup_session == session_id && received_upload == upload_token =>
                {
                    return match self.store.cleanup(session_id, received_upload) {
                        Ok(()) => writer.send_message(&WorkerMessage::completed(request_id, session_id, WorkerOperation::Cleanup, 0)),
                        Err(error) => send_error(writer, request_id, session_id, WorkerOperation::Cleanup, WorkerErrorKind::Cleanup, &error),
                    };
                }
                other => {
                    send_error(
                        writer,
                        other.request_id(),
                        session_id,
                        WorkerOperation::Cleanup,
                        WorkerErrorKind::WorkerProtocol,
                        "unexpected worker cleanup message",
                    )?;
                }
            }
        }
    }

    fn wait_for_artifacts<W: Write + Send>(
        &self, input: &InputChannel, writer: &FrameWriter<W>, cleanup: BuildCleanup, spool: ArtifactSpool,
    ) -> Result<(), String> {
        let BuildCleanup { request_id, session_id, upload_token, protocol_version } = cleanup;
        let mut fetched = vec![false; spool.entries().len()];
        loop {
            let Some((version, message)) = input.next()? else {
                return Ok(());
            };
            if version != protocol_version {
                return Err("worker artifact frame version changed".to_string());
            }
            match message {
                WorkerMessage::FetchArtifact { request_id: fetch_request, session_id: fetch_session, artifact_set_id, entry_index }
                    if fetch_session == session_id && artifact_set_id == spool.artifact_set_id() =>
                {
                    let index = usize::try_from(entry_index).map_err(|_| "worker artifact index is invalid".to_string())?;
                    if index >= fetched.len() || fetched[index] {
                        send_error(
                            writer,
                            fetch_request,
                            session_id,
                            WorkerOperation::Artifact,
                            WorkerErrorKind::Artifact,
                            "worker artifact was fetched more than once or is out of range",
                        )?;
                        continue;
                    }
                    let mut file = match spool.open_entry(index) {
                        Ok(file) => file,
                        Err(error) => {
                            send_error(writer, fetch_request, session_id, WorkerOperation::Artifact, WorkerErrorKind::Artifact, &error)?;
                            continue;
                        }
                    };
                    let mut offset = 0u64;
                    let mut buffer = [0u8; MAX_WORKER_CHUNK_BYTES];
                    loop {
                        let count = file.read(&mut buffer).map_err(|error| format!("read worker artifact: {error}"))?;
                        if count == 0 {
                            break;
                        }
                        writer.send_message(&WorkerMessage::ArtifactChunk {
                            request_id: fetch_request,
                            session_id,
                            artifact_set_id,
                            entry_index,
                            offset,
                            data: buffer[..count].to_vec(),
                        })?;
                        offset = offset.checked_add(count as u64).ok_or_else(|| "worker artifact offset overflow".to_string())?;
                    }
                    writer.send_message(&WorkerMessage::ArtifactComplete { request_id: fetch_request, session_id, artifact_set_id, entry_index })?;
                    fetched[index] = true;
                }
                WorkerMessage::Cleanup { request_id: cleanup_request, session_id: cleanup_session, upload_token: received_upload }
                    if cleanup_request == request_id && cleanup_session == session_id && received_upload == upload_token =>
                {
                    return match self.store.cleanup(session_id, received_upload) {
                        Ok(()) => writer.send_message(&WorkerMessage::completed(request_id, session_id, WorkerOperation::Cleanup, 0)),
                        Err(error) => send_error(writer, request_id, session_id, WorkerOperation::Cleanup, WorkerErrorKind::Cleanup, &error),
                    };
                }
                other => {
                    send_error(
                        writer,
                        other.request_id(),
                        session_id,
                        WorkerOperation::Artifact,
                        WorkerErrorKind::WorkerProtocol,
                        "unexpected worker artifact message",
                    )?;
                }
            }
        }
    }
}

struct BuildCleanup {
    request_id: WorkerRequestId,
    session_id: WorkerSessionId,
    upload_token: WorkerUploadId,
    protocol_version: u16,
}

struct InputChannel {
    receiver: Receiver<Result<Option<(u16, WorkerMessage)>, String>>,
    eof_seen: Arc<AtomicBool>,
    pending: Arc<AtomicUsize>,
}

impl InputChannel {
    fn spawn<R: Read + Send + 'static>(mut input: R) -> Self {
        let (sender, receiver) = mpsc::channel();
        let eof_seen = Arc::new(AtomicBool::new(false));
        let eof_seen_for_thread = eof_seen.clone();
        let pending = Arc::new(AtomicUsize::new(0));
        let pending_for_thread = pending.clone();
        std::thread::spawn(move || loop {
            match bunkerbox_worker_protocol::WorkerMessage::read_blocking_optional_versioned(&mut input) {
                Ok(Some(message)) => {
                    pending_for_thread.fetch_add(1, Ordering::Release);
                    if sender.send(Ok(Some(message))).is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    eof_seen_for_thread.store(true, Ordering::Release);
                    let _ = sender.send(Ok(None));
                    return;
                }
                Err(error) => {
                    eof_seen_for_thread.store(true, Ordering::Release);
                    let _ = sender.send(Err(error.to_string()));
                    return;
                }
            }
        });
        Self { receiver, eof_seen, pending }
    }

    fn next(&self) -> Result<Option<(u16, WorkerMessage)>, String> {
        let result = self.receiver.recv().map_err(|_| "worker input reader stopped".to_string())?;
        if matches!(&result, Ok(Some(_))) {
            self.pending.fetch_sub(1, Ordering::AcqRel);
        }
        result
    }

    fn disconnected(&self) -> bool {
        self.eof_seen.load(Ordering::Acquire) && self.pending.load(Ordering::Acquire) == 0
    }
}

struct ActiveUpload {
    request_id: WorkerRequestId,
    upload_id: WorkerUploadId,
    transaction: UploadTransaction,
}

fn send_error<W: Write + Send>(
    writer: &FrameWriter<W>, request_id: WorkerRequestId, session_id: WorkerSessionId, operation: WorkerOperation, kind: WorkerErrorKind,
    message: &str,
) -> Result<(), String> {
    let message = truncate_message(message);
    writer.send_message(&WorkerMessage::error(request_id, session_id, operation, kind, message))
}

fn truncate_message(message: &str) -> &str {
    if message.len() <= MAX_WORKER_ERROR_BYTES {
        return message;
    }
    let mut end = MAX_WORKER_ERROR_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    &message[..end]
}

pub fn run_stdio(root: &std::path::Path) -> Result<(), String> {
    let root = crate::platform::open_root(root)?;
    let service = WorkerService::new(&root)?;
    let stdin = io::stdin();
    let input = stdin;
    let writer = Arc::new(FrameWriter::new(io::stdout()));
    service.run(input, writer.as_ref())
}

#[cfg(test)]
#[path = "worker_ut.rs"]
mod tests;
