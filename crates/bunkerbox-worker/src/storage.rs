use crate::platform;
use bunkerbox_worker_protocol::{
    validate_upload_manifest, WorkerArtifactEntry, WorkerArtifactPath, WorkerArtifactSetId, WorkerDigest, WorkerEntryKind, WorkerProtocolError,
    WorkerRelativePath, WorkerSessionId, WorkerUploadEntry, WorkerUploadId, MAX_WORKER_ARTIFACT_FILE_BYTES, MAX_WORKER_ARTIFACT_TOTAL_BYTES,
    MAX_WORKER_MANIFEST_BYTES,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};

const STATE_DIRECTORY: &str = ".bunkerbox-worker";
const SESSIONS_DIRECTORY: &str = "sessions";
const UPLOADS_DIRECTORY: &str = "uploads";
const JOBS_DIRECTORY: &str = "jobs";
const MANIFEST_FILE: &str = "manifest";
const COMPLETE_FILE: &str = "complete";
const LOCK_FILE: &str = "lock";
const FILES_DIRECTORY: &str = "files";
const MANIFEST_MAGIC: [u8; 4] = *b"BBWM";
const MANIFEST_VERSION: u16 = 1;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_STALE_SESSIONS: usize = 64;
const MAX_STALE_UPLOADS_PER_SESSION: usize = 256;

static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

pub struct UploadStore {
    sessions: File,
    jobs: File,
}

impl UploadStore {
    pub fn new(root: &File) -> Result<Self, String> {
        let state = private_directory(root, STATE_DIRECTORY)?;
        let sessions = private_directory(&state, SESSIONS_DIRECTORY)?;
        let jobs = private_directory(&state, JOBS_DIRECTORY)?;
        let store = Self { sessions, jobs };
        store.cleanup_stale().map_err(|error| format!("clean stale worker state: {error}"))?;
        Ok(store)
    }

    pub fn begin(
        &self, session_id: WorkerSessionId, upload_id: WorkerUploadId, entries: Vec<WorkerUploadEntry>,
    ) -> Result<UploadTransaction, String> {
        require_nonzero_id(session_id.0, "worker session")?;
        require_nonzero_id(upload_id.0, "worker upload")?;
        validate_upload_manifest(&entries).map_err(protocol_error)?;
        validate_manifest_structure(&entries)?;

        let session = private_directory(&self.sessions, &hex_id(session_id.0))?;
        let uploads = private_directory(&session, UPLOADS_DIRECTORY)?;
        let token_name = hex_id(upload_id.0);
        platform::create_dir_at(&uploads, &token_name, 0o700).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                format!("worker upload token is already reserved: {token_name}")
            } else {
                format!("reserve worker upload token: {error}")
            }
        })?;
        let token = match platform::open_dir_at(&uploads, &token_name) {
            Ok(token) => token,
            Err(error) => {
                let _ = platform::remove_tree_at(&uploads, &token_name);
                return Err(format!("open reserved worker upload: {error}"));
            }
        };
        if let Err(error) = platform::validate_private_directory(&token, "worker upload token") {
            let _ = platform::remove_tree_at(&uploads, &token_name);
            return Err(error);
        }
        let lock = match platform::create_file_at(&token, LOCK_FILE, 0o600) {
            Ok(lock) => lock,
            Err(error) => {
                let _ = platform::remove_tree_at(&uploads, &token_name);
                return Err(format!("create worker upload lock: {error}"));
            }
        };
        let locked = match platform::lock_exclusive(&lock) {
            Ok(locked) => locked,
            Err(error) => {
                let _ = platform::remove_tree_at(&uploads, &token_name);
                return Err(format!("lock worker upload: {error}"));
            }
        };
        if !locked {
            let _ = platform::remove_tree_at(&uploads, &token_name);
            return Err("worker upload token is active".to_string());
        }
        let files = match private_directory(&token, FILES_DIRECTORY) {
            Ok(files) => files,
            Err(error) => {
                let _ = platform::remove_tree_at(&uploads, &token_name);
                return Err(error);
            }
        };

        let mut transaction =
            UploadTransaction { uploads, token_name, token, lock, files, session_id, upload_id, entries, states: BTreeMap::new(), committed: false };
        transaction.initialize()?;
        Ok(transaction)
    }

    pub fn open_completed(&self, session_id: WorkerSessionId, upload_id: WorkerUploadId) -> Result<StoredUpload, String> {
        require_nonzero_id(session_id.0, "worker session")?;
        require_nonzero_id(upload_id.0, "worker upload")?;
        let session = open_existing_private_directory(&self.sessions, &hex_id(session_id.0), "worker session")?;
        let uploads = open_existing_private_directory(&session, UPLOADS_DIRECTORY, "worker uploads")?;
        let token_name = hex_id(upload_id.0);
        let token = open_existing_private_directory(&uploads, &token_name, "worker upload token")?;
        let lock = platform::open_lock_at(&token, LOCK_FILE).map_err(|error| format!("open worker upload lock: {error}"))?;
        if !platform::lock_exclusive(&lock).map_err(|error| format!("lock worker upload: {error}"))? {
            return Err("worker upload token is active".to_string());
        }
        let complete = platform::open_file_at(&token, COMPLETE_FILE).map_err(|_| "worker upload is not complete".to_string())?;
        let marker = read_bounded(complete, 1)?;
        if !marker.is_empty() {
            return Err("worker upload completion marker is invalid".to_string());
        }
        let manifest_file = platform::open_file_at(&token, MANIFEST_FILE).map_err(|error| format!("open worker upload manifest: {error}"))?;
        let manifest = StoredManifest::read(manifest_file)?;
        if manifest.session_id != session_id || manifest.upload_id != upload_id {
            return Err("worker upload identity mismatch".to_string());
        }
        let files = open_existing_private_directory(&token, FILES_DIRECTORY, "worker upload files")?;
        Ok(StoredUpload { token, lock, files, entries: manifest.entries })
    }

    pub fn cleanup(&self, session_id: WorkerSessionId, upload_id: WorkerUploadId) -> Result<(), String> {
        require_nonzero_id(session_id.0, "worker session")?;
        require_nonzero_id(upload_id.0, "worker upload")?;
        let session = open_existing_private_directory(&self.sessions, &hex_id(session_id.0), "worker session")?;
        let uploads = open_existing_private_directory(&session, UPLOADS_DIRECTORY, "worker uploads")?;
        let token_name = hex_id(upload_id.0);
        let token = open_existing_private_directory(&uploads, &token_name, "worker upload token")?;
        let lock = platform::open_lock_at(&token, LOCK_FILE).map_err(|error| format!("open worker upload lock: {error}"))?;
        if !platform::lock_exclusive(&lock).map_err(|error| format!("lock worker upload: {error}"))? {
            return Err("worker upload token is active".to_string());
        }
        platform::remove_tree_at(&uploads, &token_name).map_err(|error| format!("remove worker upload: {error}"))
    }

    pub fn jobs_directory(&self) -> Result<File, String> {
        self.jobs.try_clone().map_err(|error| format!("clone worker jobs directory: {error}"))
    }

    fn cleanup_stale(&self) -> io::Result<()> {
        let sessions = platform::list_names(&self.sessions)?;
        for session_name in sessions.into_iter().take(MAX_STALE_SESSIONS) {
            if !is_hex_id(&session_name) {
                continue;
            }
            let Ok(session) = platform::open_dir_at(&self.sessions, &session_name) else { continue };
            let Ok(uploads) = platform::open_dir_at(&session, UPLOADS_DIRECTORY) else { continue };
            for token_name in platform::list_names(&uploads)?.into_iter().take(MAX_STALE_UPLOADS_PER_SESSION) {
                if !is_hex_id(&token_name) {
                    continue;
                }
                let Ok(token) = platform::open_dir_at(&uploads, &token_name) else { continue };
                let Ok(lock) = platform::open_lock_at(&token, LOCK_FILE) else { continue };
                if !platform::lock_exclusive(&lock)? {
                    continue;
                }
                if platform::open_file_at(&token, COMPLETE_FILE).is_err() {
                    let _ = platform::remove_tree_at(&uploads, &token_name);
                }
            }
        }
        Ok(())
    }
}

pub struct UploadTransaction {
    uploads: File,
    token_name: String,
    token: File,
    lock: File,
    files: File,
    session_id: WorkerSessionId,
    upload_id: WorkerUploadId,
    entries: Vec<WorkerUploadEntry>,
    states: BTreeMap<String, PendingFile>,
    committed: bool,
}

impl UploadTransaction {
    fn initialize(&mut self) -> Result<(), String> {
        let manifest = StoredManifest { session_id: self.session_id, upload_id: self.upload_id, entries: self.entries.clone() };
        let encoded = manifest.encode()?;
        let mut manifest_file =
            platform::create_file_at(&self.token, MANIFEST_FILE, 0o600).map_err(|error| format!("create worker upload manifest: {error}"))?;
        manifest_file.write_all(&encoded).map_err(|error| format!("write worker upload manifest: {error}"))?;
        platform::sync_fd(&manifest_file).map_err(|error| format!("flush worker upload manifest: {error}"))?;

        for entry in &self.entries {
            match entry.kind() {
                WorkerEntryKind::Directory => ensure_directory(&self.files, entry.path().as_str(), entry.mode())?,
                WorkerEntryKind::File => {
                    let file = create_relative_file(&self.files, entry.path().as_str(), entry.mode())?;
                    self.states.insert(
                        entry.path().as_str().to_string(),
                        PendingFile {
                            file,
                            size: entry.size(),
                            digest: *entry.digest().ok_or_else(|| "worker regular file has no digest".to_string())?,
                            received: 0,
                            hasher: Sha256::new(),
                        },
                    );
                }
            }
        }
        platform::sync_fd(&self.files).map_err(|error| format!("flush worker upload files: {error}"))?;
        Ok(())
    }

    pub fn accept_chunk(&mut self, path: &WorkerRelativePath, offset: u64, data: &[u8]) -> Result<(), String> {
        let state = self.states.get_mut(path.as_str()).ok_or_else(|| format!("worker chunk path is not a declared file: {}", path.as_str()))?;
        if offset != state.received {
            return Err(format!("worker chunk offset is out of order for {}", path.as_str()));
        }
        let end = offset.checked_add(data.len() as u64).ok_or_else(|| "worker chunk offset overflow".to_string())?;
        if end > state.size {
            return Err(format!("worker chunk exceeds declared file size: {}", path.as_str()));
        }
        state.file.write_all(data).map_err(|error| format!("write worker upload file {}: {error}", path.as_str()))?;
        state.hasher.update(data);
        state.received = end;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<(), String> {
        for entry in &self.entries {
            if entry.kind() != WorkerEntryKind::File {
                continue;
            }
            let state =
                self.states.get_mut(entry.path().as_str()).ok_or_else(|| format!("worker file state is missing: {}", entry.path().as_str()))?;
            if state.received != state.size {
                return Err(format!("worker upload file is incomplete: {}", entry.path().as_str()));
            }
            if state.hasher.clone().finalize().as_slice() != state.digest {
                return Err(format!("worker upload file digest mismatch: {}", entry.path().as_str()));
            }
            platform::sync_fd(&state.file).map_err(|error| format!("flush worker upload file {}: {error}", entry.path().as_str()))?;
            let metadata =
                platform::stat_fd(state.file.as_raw_fd()).map_err(|error| format!("stat worker upload file {}: {error}", entry.path().as_str()))?;
            validate_regular_file(&metadata, state.size, entry.mode(), entry.path().as_str())?;
        }
        let marker = platform::create_file_at(&self.token, COMPLETE_FILE, 0o600).map_err(|error| format!("publish worker upload: {error}"))?;
        platform::sync_fd(&marker).map_err(|error| format!("flush worker upload marker: {error}"))?;
        platform::sync_fd(&self.token).map_err(|error| format!("flush worker upload directory: {error}"))?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for UploadTransaction {
    fn drop(&mut self) {
        if !self.committed {
            let _ = platform::remove_tree_at(&self.uploads, &self.token_name);
        }
        let _ = &self.lock;
    }
}

pub struct StoredUpload {
    token: File,
    lock: File,
    files: File,
    entries: Vec<WorkerUploadEntry>,
}

impl StoredUpload {
    pub fn materialize(&self, destination: &File) -> Result<(), String> {
        let _ = (&self.token, &self.lock);
        for entry in &self.entries {
            match entry.kind() {
                WorkerEntryKind::Directory => ensure_directory(destination, entry.path().as_str(), entry.mode())?,
                WorkerEntryKind::File => {
                    let source = open_relative_file(&self.files, entry.path().as_str())?;
                    let source_metadata = platform::stat_fd(source.as_raw_fd()).map_err(|error| format!("stat stored worker file: {error}"))?;
                    validate_regular_file(&source_metadata, entry.size(), entry.mode(), entry.path().as_str())?;
                    let target = create_relative_file(destination, entry.path().as_str(), entry.mode())?;
                    copy_and_verify(&source, &target, entry)?;
                }
            }
        }
        platform::sync_fd(destination).map_err(|error| format!("flush worker job workspace: {error}"))?;
        Ok(())
    }
}

pub struct ArtifactSpool {
    parent: File,
    name: String,
    root: File,
    lock: File,
    files: File,
    artifact_set_id: WorkerArtifactSetId,
    entries: Vec<WorkerArtifactEntry>,
    total_bytes: u64,
}

impl ArtifactSpool {
    pub fn capture(parent: &File, job_root: &File, paths: &[WorkerArtifactPath], max_file_bytes: u64, max_total_bytes: u64) -> Result<Self, String> {
        if max_file_bytes == 0 || max_file_bytes > MAX_WORKER_ARTIFACT_FILE_BYTES {
            return Err("worker artifact per-file limit is invalid".to_string());
        }
        if max_total_bytes == 0 || max_total_bytes > MAX_WORKER_ARTIFACT_TOTAL_BYTES {
            return Err("worker artifact total limit is invalid".to_string());
        }
        let (name, lock_name, lock, root) = reserve_artifact_root(parent)?;
        let files = match private_directory(&root, FILES_DIRECTORY) {
            Ok(files) => files,
            Err(error) => {
                let _ = platform::remove_tree_at(parent, &name);
                let _ = platform::unlink_at(parent, &lock_name, 0);
                return Err(error);
            }
        };

        let result = (|| {
            let mut entries = Vec::with_capacity(paths.len());
            let mut total_bytes = 0u64;
            for path in paths {
                let source = open_relative_file(job_root, path.as_str())?;
                let before = platform::stat_fd(source.as_raw_fd()).map_err(|error| format!("stat worker artifact {}: {error}", path.as_str()))?;
                validate_artifact_source(&before, path.as_str())?;
                let size = u64::try_from(before.st_size).map_err(|_| format!("worker artifact size is invalid: {}", path.as_str()))?;
                if size > max_file_bytes {
                    return Err(format!("worker artifact exceeds per-file limit: {}", path.as_str()));
                }
                total_bytes = total_bytes.checked_add(size).ok_or_else(|| "worker artifact total size overflow".to_string())?;
                if total_bytes > max_total_bytes {
                    return Err("worker artifacts exceed total size limit".to_string());
                }
                let mode = (before.st_mode as u32) & 0o777;
                if let Some((parents, _)) = path.as_str().rsplit_once('/') {
                    ensure_directory(&files, parents, 0o700)?;
                }
                let destination = create_relative_file(&files, path.as_str(), mode)?;
                let digest = copy_artifact(&source, &destination, size, path.as_str())?;
                let after = platform::stat_fd(source.as_raw_fd()).map_err(|error| format!("restat worker artifact {}: {error}", path.as_str()))?;
                if after.st_dev != before.st_dev
                    || after.st_ino != before.st_ino
                    || after.st_size != before.st_size
                    || after.st_mode & 0o777 != before.st_mode & 0o777
                    || after.st_nlink != before.st_nlink
                {
                    return Err(format!("worker artifact changed during capture: {}", path.as_str()));
                }
                entries.push(WorkerArtifactEntry::new(path.as_str().to_string(), mode, size, digest).map_err(protocol_error)?);
            }
            let artifact_set_id = artifact_set_id(&name, &entries, total_bytes);
            Ok((artifact_set_id, entries, total_bytes))
        })();

        match result {
            Ok((artifact_set_id, entries, total_bytes)) => Ok(Self {
                parent: parent.try_clone().map_err(|error| format!("clone worker jobs directory: {error}"))?,
                name,
                root,
                lock,
                files,
                artifact_set_id,
                entries,
                total_bytes,
            }),
            Err(error) => {
                let _ = platform::remove_tree_at(parent, &name);
                let _ = platform::unlink_at(parent, &lock_name, 0);
                Err(error)
            }
        }
    }

    pub fn artifact_set_id(&self) -> WorkerArtifactSetId {
        self.artifact_set_id
    }

    pub fn entries(&self) -> &[WorkerArtifactEntry] {
        &self.entries
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn open_entry(&self, index: usize) -> Result<File, String> {
        let entry = self.entries.get(index).ok_or_else(|| "worker artifact index is out of range".to_string())?;
        let file = open_relative_file(&self.files, entry.path().as_str())?;
        let metadata = platform::stat_fd(file.as_raw_fd()).map_err(|error| format!("stat worker artifact spool: {error}"))?;
        validate_regular_file(&metadata, entry.size(), entry.mode(), entry.path().as_str())?;
        Ok(file)
    }
}

impl Drop for ArtifactSpool {
    fn drop(&mut self) {
        let _ = (&self.root, &self.lock, &self.files);
        let _ = platform::remove_tree_at(&self.parent, &self.name);
    }
}

struct PendingFile {
    file: File,
    size: u64,
    digest: WorkerDigest,
    received: u64,
    hasher: Sha256,
}

struct StoredManifest {
    session_id: WorkerSessionId,
    upload_id: WorkerUploadId,
    entries: Vec<WorkerUploadEntry>,
}

impl StoredManifest {
    fn encode(&self) -> Result<Vec<u8>, String> {
        validate_upload_manifest(&self.entries).map_err(protocol_error)?;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MANIFEST_MAGIC);
        bytes.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.session_id.0);
        bytes.extend_from_slice(&self.upload_id.0);
        put_u32(&mut bytes, self.entries.len())?;
        for entry in &self.entries {
            put_string(&mut bytes, entry.path().as_str())?;
            bytes.push(entry.kind().as_u8());
            bytes.extend_from_slice(&entry.mode().to_le_bytes());
            bytes.extend_from_slice(&entry.size().to_le_bytes());
            match entry.digest() {
                Some(digest) => {
                    bytes.push(1);
                    bytes.extend_from_slice(digest);
                }
                None => bytes.push(0),
            }
            if bytes.len() > MAX_WORKER_MANIFEST_BYTES {
                return Err("worker stored manifest exceeds maximum length".to_string());
            }
        }
        Ok(bytes)
    }

    fn read(file: File) -> Result<Self, String> {
        let bytes = read_bounded(file, MAX_WORKER_MANIFEST_BYTES)?;
        let mut reader = ManifestReader { bytes: &bytes, offset: 0 };
        if reader.take(4)? != MANIFEST_MAGIC {
            return Err("worker stored manifest has invalid magic".to_string());
        }
        if reader.u16()? != MANIFEST_VERSION {
            return Err("worker stored manifest has unsupported version".to_string());
        }
        let session_id = WorkerSessionId(reader.array16()?);
        let upload_id = WorkerUploadId(reader.array16()?);
        let count = reader.count()?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let path = reader.string()?;
            let kind = WorkerEntryKind::from_u8(reader.u8()?).map_err(protocol_error)?;
            let mode = reader.u32()?;
            let size = reader.u64()?;
            let digest = match reader.u8()? {
                0 => None,
                1 => Some(reader.array32()?),
                _ => return Err("worker stored manifest has invalid digest flag".to_string()),
            };
            entries.push(WorkerUploadEntry::new(path, kind, mode, size, digest).map_err(protocol_error)?);
        }
        reader.finish()?;
        validate_upload_manifest(&entries).map_err(protocol_error)?;
        validate_manifest_structure(&entries)?;
        Ok(Self { session_id, upload_id, entries })
    }
}

struct ManifestReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> ManifestReader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self.offset.checked_add(length).ok_or_else(|| "worker stored manifest length overflow".to_string())?;
        if end > self.bytes.len() {
            return Err("worker stored manifest is truncated".to_string());
        }
        let result = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(result)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().map_err(|_| "invalid worker manifest integer".to_string())?))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| "invalid worker manifest integer".to_string())?))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| "invalid worker manifest integer".to_string())?))
    }

    fn array16(&mut self) -> Result<[u8; 16], String> {
        self.take(16)?.try_into().map_err(|_| "invalid worker manifest identifier".to_string())
    }

    fn array32(&mut self) -> Result<[u8; 32], String> {
        self.take(32)?.try_into().map_err(|_| "invalid worker manifest digest".to_string())
    }

    fn count(&mut self) -> Result<usize, String> {
        let count = self.u32()? as usize;
        if count > bunkerbox_worker_protocol::MAX_WORKER_UPLOAD_ENTRIES {
            return Err("worker stored manifest has too many entries".to_string());
        }
        Ok(count)
    }

    fn string(&mut self) -> Result<String, String> {
        let length = self.u32()? as usize;
        if length > bunkerbox_worker_protocol::MAX_WORKER_PATH_BYTES {
            return Err("worker stored manifest path is too long".to_string());
        }
        String::from_utf8(self.take(length)?.to_vec()).map_err(|_| "worker stored manifest path is not UTF-8".to_string())
    }

    fn finish(self) -> Result<(), String> {
        if self.offset != self.bytes.len() {
            return Err("worker stored manifest has trailing bytes".to_string());
        }
        Ok(())
    }
}

fn private_directory(parent: &File, name: &str) -> Result<File, String> {
    match platform::create_dir_at(parent, name, 0o700) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("create private worker directory {name}: {error}")),
    }
    let directory = platform::open_dir_at(parent, name).map_err(|error| format!("open private worker directory {name}: {error}"))?;
    platform::validate_private_directory(&directory, &format!("worker directory {name}"))?;
    Ok(directory)
}

fn reserve_artifact_root(parent: &File) -> Result<(String, String, File, File), String> {
    for _ in 0..32 {
        let name = format!("artifact-{}-{}", unsafe { libc::getpid() }, next_job_id());
        let lock_name = format!("{name}.lock");
        let lock = match platform::create_file_at(parent, &lock_name, 0o600) {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create worker artifact lock: {error}")),
        };
        if !platform::lock_exclusive(&lock).map_err(|error| format!("lock worker artifact spool: {error}"))? {
            let _ = platform::unlink_at(parent, &lock_name, 0);
            continue;
        }
        if let Err(error) = platform::create_dir_at(parent, &name, 0o700) {
            let _ = platform::unlink_at(parent, &lock_name, 0);
            if error.kind() == io::ErrorKind::AlreadyExists {
                continue;
            }
            return Err(format!("create worker artifact spool: {error}"));
        }
        let root = match platform::open_dir_at(parent, &name) {
            Ok(root) => root,
            Err(error) => {
                let _ = platform::remove_tree_at(parent, &name);
                let _ = platform::unlink_at(parent, &lock_name, 0);
                return Err(format!("open worker artifact spool: {error}"));
            }
        };
        if let Err(error) = platform::validate_private_directory(&root, "worker artifact spool") {
            let _ = platform::remove_tree_at(parent, &name);
            let _ = platform::unlink_at(parent, &lock_name, 0);
            return Err(error);
        }
        return Ok((name, lock_name, lock, root));
    }
    Err("could not reserve a worker artifact spool".to_string())
}

fn validate_artifact_source(metadata: &libc::stat, path: &str) -> Result<(), String> {
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG || metadata.st_nlink != 1 {
        return Err(format!("worker artifact is not a private regular file: {path}"));
    }
    if metadata.st_size < 0 {
        return Err(format!("worker artifact has an invalid size: {path}"));
    }
    Ok(())
}

fn copy_artifact(source: &File, destination: &File, expected_size: u64, path: &str) -> Result<WorkerDigest, String> {
    let mut source = source.try_clone().map_err(|error| format!("clone worker artifact {path}: {error}"))?;
    let mut destination = destination.try_clone().map_err(|error| format!("clone worker artifact spool {path}: {error}"))?;
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    loop {
        let count = source.read(&mut buffer).map_err(|error| format!("read worker artifact {path}: {error}"))?;
        if count == 0 {
            break;
        }
        copied = copied.checked_add(count as u64).ok_or_else(|| "worker artifact size overflow".to_string())?;
        if copied > expected_size {
            return Err(format!("worker artifact grew during capture: {path}"));
        }
        hasher.update(&buffer[..count]);
        destination.write_all(&buffer[..count]).map_err(|error| format!("write worker artifact spool {path}: {error}"))?;
    }
    if copied != expected_size {
        return Err(format!("worker artifact size changed during capture: {path}"));
    }
    platform::sync_fd(&destination).map_err(|error| format!("flush worker artifact spool {path}: {error}"))?;
    Ok(hasher.finalize().into())
}

fn artifact_set_id(name: &str, entries: &[WorkerArtifactEntry], total_bytes: u64) -> WorkerArtifactSetId {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update(total_bytes.to_le_bytes());
    for entry in entries {
        hasher.update(entry.path().as_str().as_bytes());
        hasher.update(entry.size().to_le_bytes());
        hasher.update(entry.digest());
    }
    let digest = hasher.finalize();
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    if id == [0; 16] {
        id[0] = 1;
    }
    WorkerArtifactSetId(id)
}

fn open_existing_private_directory(parent: &File, name: &str, label: &str) -> Result<File, String> {
    let directory = platform::open_dir_at(parent, name).map_err(|error| format!("open {label}: {error}"))?;
    platform::validate_private_directory(&directory, label)?;
    Ok(directory)
}

fn ensure_directory(root: &File, relative: &str, mode: u32) -> Result<(), String> {
    let components = components(relative)?;
    let mut current = root.try_clone().map_err(|error| format!("clone worker directory: {error}"))?;
    for (index, component) in components.iter().enumerate() {
        let created = match platform::create_dir_at(&current, component, 0o700) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(format!("create worker directory {relative}: {error}")),
        };
        current = platform::open_dir_at(&current, component).map_err(|error| format!("open worker directory {relative}: {error}"))?;
        if created || index + 1 == components.len() {
            platform::chmod_fd(&current, mode & 0o777).map_err(|error| format!("set worker directory mode {relative}: {error}"))?;
        }
    }
    Ok(())
}

fn create_relative_file(root: &File, relative: &str, mode: u32) -> Result<File, String> {
    let components = components(relative)?;
    let (file_name, parents) = components.split_last().ok_or_else(|| "worker file path is empty".to_string())?;
    let parent = open_relative_directory_components(root, parents)?;
    let file = platform::create_file_at(&parent, file_name, mode & 0o777).map_err(|error| format!("create worker file {relative}: {error}"))?;
    platform::chmod_fd(&file, mode & 0o777).map_err(|error| format!("set worker file mode {relative}: {error}"))?;
    Ok(file)
}

fn open_relative_file(root: &File, relative: &str) -> Result<File, String> {
    let components = components(relative)?;
    let (file_name, parents) = components.split_last().ok_or_else(|| "worker file path is empty".to_string())?;
    let parent = open_relative_directory_components(root, parents)?;
    platform::open_file_at(&parent, file_name).map_err(|error| format!("open worker file {relative}: {error}"))
}

fn open_relative_directory_components(root: &File, components: &[&str]) -> Result<File, String> {
    let mut current = root.try_clone().map_err(|error| format!("clone worker directory: {error}"))?;
    for component in components {
        current = platform::open_dir_at(&current, component).map_err(|error| format!("open worker parent directory: {error}"))?;
    }
    Ok(current)
}

fn components(relative: &str) -> Result<Vec<&str>, String> {
    bunkerbox_worker_protocol::validate_worker_relative_path(relative).map_err(protocol_error)?;
    let components = relative.split('/').collect::<Vec<_>>();
    if components.iter().any(|component| component.is_empty() || *component == "." || *component == "..") {
        return Err("worker path contains an invalid component".to_string());
    }
    Ok(components)
}

fn validate_manifest_structure(entries: &[WorkerUploadEntry]) -> Result<(), String> {
    let mut kinds = BTreeMap::new();
    for entry in entries {
        if kinds.insert(entry.path().as_str(), entry.kind()).is_some() {
            return Err(format!("worker manifest has a duplicate path: {}", entry.path().as_str()));
        }
        let mut prefix = String::new();
        let parts = entry.path().as_str().split('/').collect::<Vec<_>>();
        for (index, component) in parts.iter().enumerate().take(parts.len().saturating_sub(1)) {
            if index > 0 {
                prefix.push('/');
            }
            prefix.push_str(component);
            if matches!(kinds.get(prefix.as_str()), Some(WorkerEntryKind::File)) {
                return Err(format!("worker manifest path collides with a file: {}", entry.path().as_str()));
            }
        }
    }
    for entry in entries {
        let parts = entry.path().as_str().split('/').collect::<Vec<_>>();
        let mut prefix = String::new();
        for component in parts.iter().take(parts.len().saturating_sub(1)) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if kinds.get(prefix.as_str()) != Some(&WorkerEntryKind::Directory) {
                return Err(format!("worker manifest is missing directory: {prefix}"));
            }
        }
    }
    Ok(())
}

fn copy_and_verify(source: &File, destination: &File, entry: &WorkerUploadEntry) -> Result<(), String> {
    let mut source = source.try_clone().map_err(|error| format!("clone stored worker file: {error}"))?;
    let mut destination = destination.try_clone().map_err(|error| format!("clone materialized worker file: {error}"))?;
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    loop {
        let count = source.read(&mut buffer).map_err(|error| format!("read stored worker file: {error}"))?;
        if count == 0 {
            break;
        }
        copied = copied.checked_add(count as u64).ok_or_else(|| "worker materialized size overflow".to_string())?;
        if copied > entry.size() {
            return Err(format!("stored worker file exceeds manifest: {}", entry.path().as_str()));
        }
        hasher.update(&buffer[..count]);
        destination.write_all(&buffer[..count]).map_err(|error| format!("write materialized worker file: {error}"))?;
    }
    let expected = entry.digest().ok_or_else(|| "worker materialized file has no digest".to_string())?;
    if copied != entry.size() || hasher.finalize().as_slice() != expected {
        return Err(format!("worker materialized file digest mismatch: {}", entry.path().as_str()));
    }
    platform::sync_fd(&destination).map_err(|error| format!("flush materialized worker file: {error}"))?;
    Ok(())
}

fn validate_regular_file(metadata: &libc::stat, size: u64, mode: u32, path: &str) -> Result<(), String> {
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG || metadata.st_nlink != 1 {
        return Err(format!("worker file is not a private regular file: {path}"));
    }
    if metadata.st_size < 0 || metadata.st_size as u64 != size {
        return Err(format!("worker file size mismatch: {path}"));
    }
    if metadata.st_mode & 0o777 != mode as libc::mode_t & 0o777 {
        return Err(format!("worker file mode mismatch: {path}"));
    }
    Ok(())
}

fn read_bounded(mut file: File, maximum: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut limited = (&mut file).take(maximum as u64 + 1);
    limited.read_to_end(&mut bytes).map_err(|error| format!("read worker state: {error}"))?;
    if bytes.len() > maximum {
        return Err("worker state exceeds its maximum length".to_string());
    }
    Ok(bytes)
}

fn put_u32(bytes: &mut Vec<u8>, value: usize) -> Result<(), String> {
    bytes.extend_from_slice(&u32::try_from(value).map_err(|_| "worker manifest count does not fit in u32".to_string())?.to_le_bytes());
    Ok(())
}

fn put_string(bytes: &mut Vec<u8>, value: &str) -> Result<(), String> {
    put_u32(bytes, value.len())?;
    bytes.extend_from_slice(value.as_bytes());
    Ok(())
}

fn require_nonzero_id(bytes: [u8; 16], label: &str) -> Result<(), String> {
    if bytes == [0; 16] {
        return Err(format!("{label} ID must be nonzero"));
    }
    Ok(())
}

fn hex_id(bytes: [u8; 16]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_hex_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn protocol_error(error: WorkerProtocolError) -> String {
    error.to_string()
}

pub(crate) fn open_relative_directory(root: &File, relative: &str) -> Result<File, String> {
    if relative.is_empty() {
        return root.try_clone().map_err(|error| format!("clone worker cwd: {error}"));
    }
    let components = components(relative)?;
    open_relative_directory_components(root, &components)
}

pub(crate) fn next_job_id() -> u64 {
    NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
#[path = "storage_ut.rs"]
mod tests;
