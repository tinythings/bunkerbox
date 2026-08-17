use crate::platform;
use crate::storage;
use bunkerbox_worker_protocol::{WorkerBuild, WorkerMessage, WorkerRequestId, WorkerSessionId};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub const WORKER_BUILD_TIMEOUT: Duration = Duration::from_secs(30);
pub const WORKER_MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
const OUTPUT_BUFFER_BYTES: usize = 8192;
const POST_EXIT_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
const FINAL_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_STALE_JOBS: usize = 256;

pub trait OutputSink: Send + Sync {
    fn send(&self, message: WorkerMessage) -> Result<(), String>;
}

pub struct JobWorkspace {
    parent: File,
    name: String,
    lock_name: String,
    root: File,
    lock: File,
}

impl JobWorkspace {
    pub fn create(parent: &File) -> Result<Self, String> {
        for _ in 0..32 {
            let name = format!("job-{}-{}", unsafe { libc::getpid() }, storage::next_job_id());
            let lock_name = format!("{name}.lock");
            let lock = match platform::create_file_at(parent, &lock_name, 0o600) {
                Ok(lock) => lock,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("create worker job lock: {error}")),
            };
            let locked = match platform::lock_exclusive(&lock) {
                Ok(locked) => locked,
                Err(error) => {
                    let _ = platform::unlink_at(parent, &lock_name, 0);
                    return Err(format!("lock worker job: {error}"));
                }
            };
            if !locked {
                let _ = platform::unlink_at(parent, &lock_name, 0);
                continue;
            }
            if let Err(error) = platform::create_dir_at(parent, &name, 0o700) {
                let _ = platform::unlink_at(parent, &lock_name, 0);
                if error.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(format!("create worker job workspace: {error}"));
            }
            let root = match platform::open_dir_at(parent, &name) {
                Ok(root) => root,
                Err(error) => {
                    let _ = platform::remove_tree_at(parent, &name);
                    let _ = platform::unlink_at(parent, &lock_name, 0);
                    return Err(format!("open worker job workspace: {error}"));
                }
            };
            return Ok(Self {
                parent: parent.try_clone().map_err(|error| format!("clone worker jobs directory: {error}"))?,
                name,
                lock_name,
                root,
                lock,
            });
        }
        Err("could not reserve a unique worker job workspace".to_string())
    }

    pub fn root(&self) -> &File {
        &self.root
    }
}

impl Drop for JobWorkspace {
    fn drop(&mut self) {
        let _ = &self.lock;
        let _ = platform::remove_tree_at(&self.parent, &self.name);
        let _ = platform::unlink_at(&self.parent, &self.lock_name, 0);
    }
}

pub fn cleanup_stale_jobs(parent: &File) -> io::Result<()> {
    for lock_name in platform::list_names(parent)?.into_iter().take(MAX_STALE_JOBS) {
        let Some(name) = lock_name.strip_suffix(".lock") else { continue };
        if !name.starts_with("job-") {
            continue;
        }
        let Ok(lock) = platform::open_lock_at(parent, &lock_name) else { continue };
        if platform::lock_exclusive(&lock)? {
            let _ = platform::remove_tree_at(parent, name);
            let _ = platform::unlink_at(parent, &lock_name, 0);
        }
    }
    Ok(())
}

pub fn execute_build<S: OutputSink>(
    job: &JobWorkspace, build: &WorkerBuild, request_id: WorkerRequestId, session_id: WorkerSessionId, sink: &S, disconnected: &dyn Fn() -> bool,
) -> Result<i32, String> {
    validate_executable(build.trusted_executable_path())?;
    let cwd = storage::open_relative_directory(job.root(), build.cwd().as_str())?;
    let cwd_fd = cwd.as_raw_fd();
    let mut command = std::process::Command::new(build.trusted_executable_path());
    command.args(build.argv()).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    command.env_clear();
    for (key, value) in build.guest_env() {
        command.env(key, value);
    }
    for (key, value) in build.target_env() {
        command.env(key, value);
    }
    unsafe {
        command.pre_exec(move || {
            platform::set_process_group()?;
            platform::change_directory(cwd_fd)?;
            Ok(())
        });
    }

    let mut child = command.spawn().map_err(|error| format!("spawn worker tool: {error}"))?;
    let pgid = child.id() as libc::pid_t;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            kill_group(pgid);
            let _ = child.wait();
            return Err("worker child has no stdout".to_string());
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            kill_group(pgid);
            let _ = child.wait();
            return Err("worker child has no stderr".to_string());
        }
    };
    if let Err(error) = platform::set_nonblocking_fd(stdout.as_raw_fd()) {
        kill_group(pgid);
        let _ = child.wait();
        return Err(format!("set worker stdout nonblocking: {error}"));
    }
    if let Err(error) = platform::set_nonblocking_fd(stderr.as_raw_fd()) {
        kill_group(pgid);
        let _ = child.wait();
        return Err(format!("set worker stderr nonblocking: {error}"));
    }

    let stop = Arc::new(AtomicBool::new(false));
    let (events_tx, events_rx) = mpsc::channel();
    let stdout_thread = spawn_pump(stdout, StreamKind::Stdout, events_tx.clone(), stop.clone());
    let stderr_thread = spawn_pump(stderr, StreamKind::Stderr, events_tx, stop.clone());

    let started = Instant::now();
    let mut child_status = None;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut output_total = 0u64;
    let mut failure = None;
    let mut post_exit_deadline = None;
    let mut final_deadline = None;
    let mut group_killed = false;

    while child_status.is_none() || !stdout_done || !stderr_done {
        if child_status.is_none() && failure.is_none() && disconnected() {
            failure = Some("worker protocol input disconnected during build".to_string());
            kill_group(pgid);
            group_killed = true;
            final_deadline = Some(Instant::now() + FINAL_DRAIN_TIMEOUT);
        }
        if child_status.is_none() && failure.is_none() && started.elapsed() >= WORKER_BUILD_TIMEOUT {
            failure = Some("worker build timed out".to_string());
            kill_group(pgid);
            group_killed = true;
            final_deadline = Some(Instant::now() + FINAL_DRAIN_TIMEOUT);
        }

        if child_status.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    child_status = Some(status);
                    if failure.is_none() {
                        platform::signal_process_group(pgid, libc::SIGTERM);
                        post_exit_deadline = Some(Instant::now() + POST_EXIT_DRAIN_TIMEOUT);
                    } else {
                        kill_group(pgid);
                        group_killed = true;
                        final_deadline = Some(Instant::now() + FINAL_DRAIN_TIMEOUT);
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    failure.get_or_insert(format!("wait for worker tool: {error}"));
                    kill_group(pgid);
                    group_killed = true;
                    final_deadline = Some(Instant::now() + FINAL_DRAIN_TIMEOUT);
                }
            }
        }

        if let Some(deadline) = post_exit_deadline {
            if Instant::now() >= deadline {
                kill_group(pgid);
                group_killed = true;
                post_exit_deadline = None;
                final_deadline = Some(Instant::now() + FINAL_DRAIN_TIMEOUT);
            }
        }
        if let Some(deadline) = final_deadline {
            if Instant::now() >= deadline {
                stop.store(true, Ordering::Release);
                break;
            }
        }

        match events_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(PumpEvent::Data(stream, bytes)) => {
                if failure.is_some() {
                    continue;
                }
                output_total = output_total.saturating_add(bytes.len() as u64);
                if output_total > WORKER_MAX_OUTPUT_BYTES {
                    failure = Some("worker combined output limit exceeded".to_string());
                    kill_group(pgid);
                    group_killed = true;
                    final_deadline = Some(Instant::now() + FINAL_DRAIN_TIMEOUT);
                    continue;
                }
                let message = match stream {
                    StreamKind::Stdout => WorkerMessage::stdout(request_id, session_id, bytes),
                    StreamKind::Stderr => WorkerMessage::stderr(request_id, session_id, bytes),
                };
                if let Err(error) = sink.send(message) {
                    failure.get_or_insert(error);
                    kill_group(pgid);
                    group_killed = true;
                    final_deadline = Some(Instant::now() + FINAL_DRAIN_TIMEOUT);
                }
            }
            Ok(PumpEvent::End(stream)) => match stream {
                StreamKind::Stdout => stdout_done = true,
                StreamKind::Stderr => stderr_done = true,
            },
            Ok(PumpEvent::Failed(stream, error)) => {
                failure.get_or_insert(format!("read worker {}: {error}", stream.label()));
                kill_group(pgid);
                group_killed = true;
                final_deadline = Some(Instant::now() + FINAL_DRAIN_TIMEOUT);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                failure.get_or_insert("worker output pump channel disconnected".to_string());
                kill_group(pgid);
                group_killed = true;
                final_deadline = Some(Instant::now() + FINAL_DRAIN_TIMEOUT);
            }
        }
    }

    if !group_killed {
        kill_group(pgid);
    }
    stop.store(true, Ordering::Release);
    let status = match child_status {
        Some(status) => status,
        None => child.wait().map_err(|error| format!("reap worker tool: {error}"))?,
    };
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(status.code().unwrap_or(-1))
}

fn spawn_pump<R: Read + Send + 'static>(
    mut reader: R, stream: StreamKind, sender: mpsc::Sender<PumpEvent>, stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = [0u8; OUTPUT_BUFFER_BYTES];
        let mut total = 0u64;
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            match reader.read(&mut buffer) {
                Ok(0) => {
                    let _ = sender.send(PumpEvent::End(stream));
                    return;
                }
                Ok(count) => {
                    total = total.saturating_add(count as u64);
                    if total > WORKER_MAX_OUTPUT_BYTES {
                        let _ = sender.send(PumpEvent::Failed(stream, "worker output limit exceeded".to_string()));
                        return;
                    }
                    if sender.send(PumpEvent::Data(stream, buffer[..count].to_vec())).is_err() {
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(5)),
                Err(error) => {
                    let _ = sender.send(PumpEvent::Failed(stream, error.to_string()));
                    return;
                }
            }
        }
    })
}

fn validate_executable(path: &str) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| format!("inspect worker executable: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err("worker executable is not a regular file".to_string());
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err("worker executable is not executable".to_string());
    }
    Ok(())
}

fn kill_group(pgid: libc::pid_t) {
    platform::signal_process_group(pgid, libc::SIGTERM);
    platform::signal_process_group(pgid, libc::SIGKILL);
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

impl StreamKind {
    fn label(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

enum PumpEvent {
    Data(StreamKind, Vec<u8>),
    End(StreamKind),
    Failed(StreamKind, String),
}

#[cfg(test)]
#[path = "process_ut.rs"]
mod tests;
