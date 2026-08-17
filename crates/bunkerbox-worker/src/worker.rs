use crate::process::{self, JobWorkspace, OutputSink};
use crate::storage::{UploadStore, UploadTransaction};
use bunkerbox_worker_protocol::{
    WorkerErrorKind, WorkerMessage, WorkerOperation, WorkerRequestId, WorkerSessionId, WorkerUploadId, MAX_WORKER_ERROR_BYTES,
};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

pub struct FrameWriter<W: Write + Send> {
    writer: Mutex<W>,
}

impl<W: Write + Send> FrameWriter<W> {
    pub fn new(writer: W) -> Self {
        Self { writer: Mutex::new(writer) }
    }

    pub fn send_message(&self, message: &WorkerMessage) -> Result<(), String> {
        let mut writer = self.writer.lock().map_err(|_| "worker protocol writer lock poisoned".to_string())?;
        message.write_blocking(&mut *writer).map_err(|error| error.to_string())
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
        let Some(hello) = input.next()? else {
            return Ok(());
        };
        let (request_id, session_id, response, version) = match hello {
            WorkerMessage::Hello { request_id, session_id, response, version } => (request_id, session_id, response, version),
            _ => return Err("worker did not receive Hello first".to_string()),
        };
        if response {
            return Err("worker received a Hello response instead of a request".to_string());
        }
        if version != bunkerbox_worker_protocol::WORKER_PROTOCOL_VERSION {
            return Err(format!("unsupported worker protocol version: {version}"));
        }
        if session_id.0 == [0; 16] {
            return Err("worker session ID must be nonzero".to_string());
        }
        writer.send_message(&WorkerMessage::hello(request_id, session_id, true))?;

        let mut active_upload: Option<ActiveUpload> = None;
        loop {
            let Some(message) = input.next()? else {
                return Ok(());
            };
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
                    self.handle_build(&input, writer, request_id, session_id, build)?;
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
        build: bunkerbox_worker_protocol::WorkerBuild,
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
        writer.send_message(&WorkerMessage::completed(request_id, session_id, WorkerOperation::Build, exit_code))?;

        let Some(cleanup) = input.next()? else {
            return Ok(());
        };
        match cleanup {
            WorkerMessage::Cleanup { request_id: cleanup_request, session_id: cleanup_session, upload_token }
                if cleanup_request == request_id && cleanup_session == session_id && upload_token == build.upload_token() =>
            {
                match self.store.cleanup(session_id, upload_token) {
                    Ok(()) => writer.send_message(&WorkerMessage::completed(request_id, session_id, WorkerOperation::Cleanup, 0)),
                    Err(error) => send_error(writer, request_id, session_id, WorkerOperation::Cleanup, WorkerErrorKind::Cleanup, &error),
                }
            }
            other => send_error(
                writer,
                other.request_id(),
                session_id,
                WorkerOperation::Cleanup,
                WorkerErrorKind::WorkerProtocol,
                "unexpected worker cleanup message",
            ),
        }
    }
}

struct InputChannel {
    receiver: Receiver<Result<Option<WorkerMessage>, String>>,
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
            match bunkerbox_worker_protocol::WorkerMessage::read_blocking_optional(&mut input) {
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

    fn next(&self) -> Result<Option<WorkerMessage>, String> {
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
