use crate::remote::RemoteExecutionControl;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub const MAX_ARTIFACT_ENTRIES: usize = 256;
pub const MAX_ARTIFACT_PATH_BYTES: usize = 4 * 1024;
pub const MAX_ARTIFACT_FILE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_ARTIFACT_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_ARTIFACT_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_MAX_ARTIFACT_FILE_BYTES: u64 = MAX_ARTIFACT_FILE_BYTES;
pub const DEFAULT_MAX_ARTIFACT_TOTAL_BYTES: u64 = MAX_ARTIFACT_TOTAL_BYTES;
pub const DEFAULT_MAX_ARTIFACT_ENTRIES: usize = MAX_ARTIFACT_ENTRIES;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const ARTIFACT_ROOT: &str = ".bunkerbox";
const ARTIFACT_DIRECTORY: &str = "artifacts";
const STAGING_DIRECTORY: &str = ".staging";
static NEXT_SPOOL_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtifactPolicy {
    paths: Vec<String>,
}

impl ArtifactPolicy {
    pub fn new(paths: Vec<String>) -> Result<Self, String> {
        if paths.len() > MAX_ARTIFACT_ENTRIES {
            return Err(format!("artifact policy exceeds maximum entry count {MAX_ARTIFACT_ENTRIES}"));
        }
        let mut seen = BTreeSet::new();
        for path in &paths {
            validate_artifact_path(path)?;
            if !seen.insert(path.clone()) {
                return Err(format!("duplicate artifact path: {path}"));
            }
        }
        Ok(Self { paths })
    }

    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    pub fn is_enabled(&self) -> bool {
        !self.paths.is_empty()
    }

    pub fn validate_limits(&self, limits: ArtifactLimits) -> Result<(), String> {
        if self.paths.len() > limits.max_entries {
            return Err(format!("artifact policy exceeds configured entry count {}", limits.max_entries));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArtifactLimits {
    pub timeout: Duration,
    pub max_entries: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for ArtifactLimits {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_ARTIFACT_TIMEOUT,
            max_entries: DEFAULT_MAX_ARTIFACT_ENTRIES,
            max_file_bytes: DEFAULT_MAX_ARTIFACT_FILE_BYTES,
            max_total_bytes: DEFAULT_MAX_ARTIFACT_TOTAL_BYTES,
        }
    }
}

impl ArtifactLimits {
    pub fn new(timeout: Duration, max_entries: usize, max_file_bytes: u64, max_total_bytes: u64) -> Result<Self, String> {
        if timeout.is_zero() {
            return Err("artifact timeout must be positive".to_string());
        }
        if max_entries == 0 || max_entries > MAX_ARTIFACT_ENTRIES {
            return Err(format!("artifact entry limit must be between 1 and {MAX_ARTIFACT_ENTRIES}"));
        }
        if max_file_bytes == 0 || max_file_bytes > MAX_ARTIFACT_FILE_BYTES {
            return Err(format!("artifact file limit must be between 1 and {MAX_ARTIFACT_FILE_BYTES}"));
        }
        if max_total_bytes == 0 || max_total_bytes > MAX_ARTIFACT_TOTAL_BYTES {
            return Err(format!("artifact total limit must be between 1 and {MAX_ARTIFACT_TOTAL_BYTES}"));
        }
        Ok(Self { timeout, max_entries, max_file_bytes, max_total_bytes })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactEntry {
    path: String,
    mode: u32,
    size: u64,
    digest: [u8; 32],
}

impl ArtifactEntry {
    pub fn new(path: impl Into<String>, mode: u32, size: u64, digest: [u8; 32]) -> Result<Self, String> {
        let path = path.into();
        validate_artifact_path(&path)?;
        if mode & !0o777 != 0 {
            return Err(format!("artifact mode has unsupported bits: {mode:o}"));
        }
        Ok(Self { path, mode, size, digest })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactManifest {
    entries: Vec<ArtifactEntry>,
    total_bytes: u64,
}

impl ArtifactManifest {
    pub fn new(entries: Vec<ArtifactEntry>, total_bytes: u64, policy: &ArtifactPolicy, limits: ArtifactLimits) -> Result<Self, String> {
        policy.validate_limits(limits)?;
        if entries.len() != policy.paths.len() {
            return Err(format!("artifact manifest has {} entries but {} are required", entries.len(), policy.paths.len()));
        }
        if entries.len() > limits.max_entries {
            return Err("artifact manifest exceeds configured entry count".to_string());
        }
        let mut total = 0u64;
        let mut seen = BTreeSet::new();
        for (entry, expected_path) in entries.iter().zip(policy.paths.iter()) {
            if entry.path != *expected_path {
                return Err(format!("artifact manifest path does not match policy: {}", entry.path));
            }
            if !seen.insert(entry.path.as_str()) {
                return Err(format!("artifact manifest contains duplicate path: {}", entry.path));
            }
            if entry.size > limits.max_file_bytes {
                return Err(format!("artifact exceeds configured per-file limit: {}", entry.path));
            }
            total = total.checked_add(entry.size).ok_or_else(|| "artifact manifest total size overflow".to_string())?;
            if total > limits.max_total_bytes {
                return Err("artifact manifest exceeds configured total size".to_string());
            }
        }
        if total != total_bytes {
            return Err("artifact manifest total size does not match entries".to_string());
        }
        Ok(Self { entries, total_bytes })
    }

    pub fn entries(&self) -> &[ArtifactEntry] {
        &self.entries
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn entry(&self, index: usize) -> Option<&ArtifactEntry> {
        self.entries.get(index)
    }
}

pub struct LocalArtifactSpool {
    path: PathBuf,
    root: File,
    manifest: ArtifactManifest,
}

impl LocalArtifactSpool {
    pub fn capture(job_root: &Path, parent: &Path, policy: &ArtifactPolicy, limits: ArtifactLimits) -> Result<Self, String> {
        Self::capture_with_control(job_root, parent, policy, limits, &RemoteExecutionControl::new())
    }

    pub fn capture_with_control(
        job_root: &Path, parent: &Path, policy: &ArtifactPolicy, limits: ArtifactLimits, control: &RemoteExecutionControl,
    ) -> Result<Self, String> {
        policy.validate_limits(limits)?;
        let path = create_unique_directory(parent, "artifact-spool")?;
        let root = match open_directory(&path) {
            Ok(root) => root,
            Err(error) => {
                let _ = fs::remove_dir_all(&path);
                return Err(format!("open local artifact spool: {error}"));
            }
        };

        let result = (|| {
            let mut entries = Vec::with_capacity(policy.paths.len());
            let mut total = 0u64;
            for relative in policy.paths() {
                if control.is_cancelled() {
                    return Err("artifact capture cancelled".to_string());
                }
                let source = open_regular_file(job_root, relative)?;
                let metadata = source.metadata().map_err(|error| format!("stat artifact {relative}: {error}"))?;
                if metadata.nlink() != 1 {
                    return Err(format!("artifact is a hard-link alias: {relative}"));
                }
                let size = metadata.len();
                if size > limits.max_file_bytes {
                    return Err(format!("artifact exceeds configured per-file limit: {relative}"));
                }
                total = total.checked_add(size).ok_or_else(|| "artifact total size overflow".to_string())?;
                if total > limits.max_total_bytes {
                    return Err("artifacts exceed configured total size".to_string());
                }
                let mode = metadata.mode() & 0o777;
                let destination = create_relative_file(&root, relative, mode)?;
                let digest = copy_and_hash(&source, &destination, size, relative, control)?;
                let after = source.metadata().map_err(|error| format!("restat artifact {relative}: {error}"))?;
                if after.dev() != metadata.dev()
                    || after.ino() != metadata.ino()
                    || after.len() != metadata.len()
                    || after.mode() & 0o777 != metadata.mode() & 0o777
                    || after.nlink() != metadata.nlink()
                {
                    return Err(format!("artifact changed during capture: {relative}"));
                }
                entries.push(ArtifactEntry::new(relative.clone(), mode, size, digest)?);
            }
            ArtifactManifest::new(entries, total, policy, limits)
        })();

        match result {
            Ok(manifest) => Ok(Self { path, root, manifest }),
            Err(error) => {
                let _ = fs::remove_dir_all(&path);
                Err(error)
            }
        }
    }

    pub fn manifest(&self) -> &ArtifactManifest {
        &self.manifest
    }

    pub fn open_entry(&self, index: usize) -> Result<File, String> {
        let entry = self.manifest.entry(index).ok_or_else(|| "artifact index is out of range".to_string())?;
        let file = open_regular_file_from_fd(&self.root, &entry.path)?;
        let metadata = file.metadata().map_err(|error| format!("stat spooled artifact {}: {error}", entry.path))?;
        if metadata.nlink() != 1 || metadata.len() != entry.size || metadata.mode() & 0o777 != entry.mode {
            return Err(format!("spooled artifact metadata changed: {}", entry.path));
        }
        Ok(file)
    }
}

impl Drop for LocalArtifactSpool {
    fn drop(&mut self) {
        let _ = &self.root;
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub struct ArtifactPublication {
    artifacts: File,
    staging_root: File,
    staging: File,
    request: String,
    manifest: ArtifactManifest,
    written: BTreeSet<usize>,
    published: bool,
}

impl ArtifactPublication {
    pub fn new(workspace_root: &Path, request_id: [u8; 16], manifest: ArtifactManifest) -> Result<Self, String> {
        let workspace = open_directory(workspace_root).map_err(|error| format!("open artifact workspace: {error}"))?;
        let bunkerbox = ensure_directory_at(&workspace, ARTIFACT_ROOT)?;
        let artifacts = ensure_directory_at(&bunkerbox, ARTIFACT_DIRECTORY)?;
        let staging_root = ensure_directory_at(&artifacts, STAGING_DIRECTORY)?;
        let request = hex_id(request_id);
        if entry_exists_at(&artifacts, &request).map_err(|error| format!("inspect artifact destination: {error}"))? {
            return Err(format!("artifact destination already exists for request {request}"));
        }
        let staging = create_directory_at(&staging_root, &request, 0o700).map_err(|error| format!("create artifact staging directory: {error}"))?;
        Ok(Self { artifacts, staging_root, staging, request, manifest, written: BTreeSet::new(), published: false })
    }

    pub fn begin(&self, index: usize) -> Result<ArtifactWriter, String> {
        if self.written.contains(&index) {
            return Err(format!("artifact entry was already written: {index}"));
        }
        let entry = self.manifest.entry(index).ok_or_else(|| "artifact index is out of range".to_string())?.clone();
        let file = create_relative_file(&self.staging, &entry.path, 0o600)
            .map_err(|error| format!("create artifact staging file {}: {error}", entry.path))?;
        Ok(ArtifactWriter { index, entry, file, written: 0, hasher: Sha256::new() })
    }

    pub fn complete(&mut self, writer: ArtifactWriter) -> Result<(), String> {
        let index = writer.index;
        writer.finish()?;
        if !self.written.insert(index) {
            return Err(format!("artifact entry was completed twice: {index}"));
        }
        Ok(())
    }

    pub fn copy_from_reader<R: Read>(&mut self, index: usize, reader: &mut R) -> Result<(), String> {
        self.copy_from_reader_with_control(index, reader, &RemoteExecutionControl::new())
    }

    pub fn copy_from_reader_with_control<R: Read>(&mut self, index: usize, reader: &mut R, control: &RemoteExecutionControl) -> Result<(), String> {
        let mut writer = self.begin(index)?;
        let mut buffer = [0u8; COPY_BUFFER_BYTES];
        loop {
            if control.is_cancelled() {
                return Err("artifact publication copy cancelled".to_string());
            }
            let count = reader.read(&mut buffer).map_err(|error| format!("read artifact spool: {error}"))?;
            if count == 0 {
                break;
            }
            writer.write_chunk(&buffer[..count])?;
        }
        self.complete(writer)
    }

    pub fn publish(mut self) -> Result<(), String> {
        if self.written.len() != self.manifest.entries.len() {
            return Err("artifact publication is missing entries".to_string());
        }
        rename_noreplace(&self.staging_root, &self.request, &self.artifacts, &self.request).map_err(|error| format!("publish artifacts: {error}"))?;
        self.artifacts.sync_all().map_err(|error| format!("flush artifact destination: {error}"))?;
        self.published = true;
        Ok(())
    }
}

impl Drop for ArtifactPublication {
    fn drop(&mut self) {
        if !self.published {
            cleanup_publication(&self.staging_root, &self.staging, &self.request, &self.manifest);
        }
    }
}

pub struct ArtifactWriter {
    index: usize,
    entry: ArtifactEntry,
    file: File,
    written: u64,
    hasher: Sha256,
}

impl ArtifactWriter {
    pub fn expected_offset(&self) -> Result<u64, String> {
        self.file.metadata().map(|metadata| metadata.len()).map_err(|error| format!("stat artifact staging file: {error}"))
    }

    pub fn write_chunk(&mut self, bytes: &[u8]) -> Result<(), String> {
        let end = self.written.checked_add(bytes.len() as u64).ok_or_else(|| "artifact size overflow".to_string())?;
        if end > self.entry.size {
            return Err(format!("artifact exceeds manifest size: {}", self.entry.path));
        }
        self.file.write_all(bytes).map_err(|error| format!("write artifact staging file: {error}"))?;
        self.hasher.update(bytes);
        self.written = end;
        Ok(())
    }

    fn finish(mut self) -> Result<(), String> {
        self.file.flush().map_err(|error| format!("flush artifact staging file: {error}"))?;
        self.file.sync_all().map_err(|error| format!("sync artifact staging file: {error}"))?;
        let metadata = self.file.metadata().map_err(|error| format!("stat artifact staging file: {error}"))?;
        if self.written != self.entry.size || metadata.len() != self.entry.size {
            return Err(format!("artifact size does not match manifest: {}", self.entry.path));
        }
        if metadata.nlink() != 1 {
            return Err(format!("artifact staging file has an invalid link count: {}", self.entry.path));
        }
        if self.hasher.finalize().as_slice() != self.entry.digest {
            return Err(format!("artifact digest does not match manifest: {}", self.entry.path));
        }
        if unsafe { libc::fchmod(self.file.as_raw_fd(), self.entry.mode as libc::mode_t) } != 0 {
            return Err(format!("set artifact mode {}: {}", self.entry.path, io::Error::last_os_error()));
        }
        Ok(())
    }
}

pub fn validate_artifact_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.len() > MAX_ARTIFACT_PATH_BYTES || path.as_bytes().contains(&0) {
        return Err("artifact path is empty, too long, or contains NUL".to_string());
    }
    if path.starts_with('/') || path.starts_with('\\') || path.contains('\\') {
        return Err(format!("artifact path must be relative: {path}"));
    }
    if path.bytes().any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']' | b'{' | b'}')) {
        return Err(format!("artifact path must not contain glob syntax: {path}"));
    }
    if path.split('/').any(|component| component.is_empty() || component == "." || component == "..") {
        return Err(format!("artifact path is not normalized: {path}"));
    }
    Ok(())
}

fn artifact_components(path: &str) -> Result<Vec<&str>, String> {
    validate_artifact_path(path)?;
    Ok(path.split('/').collect())
}

fn create_unique_directory(parent: &Path, label: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(parent).map_err(|error| format!("create {label} parent: {error}"))?;
    for _ in 0..32 {
        let name = format!(".bunkerbox-{label}-{}-{}", std::process::id(), NEXT_SPOOL_ID.fetch_add(1, Ordering::Relaxed));
        let path = parent.join(name);
        match fs::create_dir(&path) {
            Ok(()) => {
                set_private_mode(&path)?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create {label}: {error}")),
        }
    }
    Err(format!("could not reserve a private {label}"))
}

fn ensure_directory_at(parent: &File, name: &str) -> Result<File, String> {
    let name = component_name(name).map_err(|error| format!("invalid artifact directory component: {error}"))?;
    let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } == 0;
    if !created {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(format!("create artifact directory: {error}"));
        }
    }
    let name = name.to_str().map_err(|_| "artifact directory component is not UTF-8".to_string())?;
    let directory = open_directory_at(parent, name).map_err(|error| format!("open artifact directory: {error}"))?;
    if created {
        set_private_mode_fd(&directory)?;
    }
    Ok(directory)
}

fn create_directory_at(parent: &File, name: &str, mode: u32) -> io::Result<File> {
    let name = component_name(name)?;
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let name = name.to_str().map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "artifact directory component is not UTF-8"))?;
    let directory = match open_directory_at(parent, name) {
        Ok(directory) => directory,
        Err(error) => {
            let _ = unlink_at(parent, name, libc::AT_REMOVEDIR);
            return Err(error);
        }
    };
    if let Err(error) = set_private_mode_fd_io(&directory, mode) {
        let _ = unlink_at(parent, name, libc::AT_REMOVEDIR);
        return Err(error);
    }
    Ok(directory)
}

fn entry_exists_at(parent: &File, name: &str) -> io::Result<bool> {
    let name = component_name(name)?;
    let mut metadata = unsafe { std::mem::zeroed::<libc::stat>() };
    let result = unsafe { libc::fstatat(parent.as_raw_fd(), name.as_ptr(), &mut metadata, libc::AT_SYMLINK_NOFOLLOW) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound {
        Ok(false)
    } else {
        Err(error)
    }
}

fn rename_noreplace(from_parent: &File, from_name: &str, to_parent: &File, to_name: &str) -> io::Result<()> {
    let from_name = component_name(from_name)?;
    let to_name = component_name(to_name)?;
    #[cfg(target_os = "linux")]
    {
        let result =
            unsafe { libc::renameat2(from_parent.as_raw_fd(), from_name.as_ptr(), to_parent.as_raw_fd(), to_name.as_ptr(), libc::RENAME_NOREPLACE) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (from_parent, from_name, to_parent, to_name);
        Err(io::Error::new(io::ErrorKind::Unsupported, "artifact publication requires renameat2"))
    }
}

fn cleanup_publication(staging_root: &File, staging: &File, request: &str, manifest: &ArtifactManifest) {
    let mut directories = Vec::new();
    for entry in &manifest.entries {
        let Ok(components) = artifact_components(&entry.path) else { continue };
        let _ = unlink_relative(staging, &components, 0);
        for count in 1..components.len() {
            directories.push(components[..count].join("/"));
        }
    }
    directories.sort_by_key(|path| std::cmp::Reverse(path.split('/').count()));
    directories.dedup();
    for directory in directories {
        if let Ok(components) = artifact_components(&directory) {
            let _ = unlink_relative(staging, &components, libc::AT_REMOVEDIR);
        }
    }
    let _ = unlink_at(staging_root, request, libc::AT_REMOVEDIR);
}

fn unlink_relative(root: &File, components: &[&str], flags: i32) -> io::Result<()> {
    let (name, parents) = components.split_last().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "artifact path is empty"))?;
    let mut parent = root.try_clone()?;
    for component in parents {
        parent = open_directory_at(&parent, component)?;
    }
    unlink_at(&parent, name, flags)
}

fn unlink_at(parent: &File, name: &str, flags: i32) -> io::Result<()> {
    let name = component_name(name)?;
    let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn component_name(name: &str) -> io::Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid path component"));
    }
    CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path component"))
}

fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)
}

fn open_directory_at(parent: &File, name: &str) -> io::Result<File> {
    open_at(parent, name, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
}

fn open_file_at(parent: &File, name: &str) -> io::Result<File> {
    open_at(parent, name, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
}

fn open_at(parent: &File, name: &str, flags: i32) -> io::Result<File> {
    let name = CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in artifact path"))?;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn create_file_at(parent: &File, name: &str, mode: u32) -> io::Result<File> {
    let name = CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in artifact path"))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::mode_t,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_relative_directory(root: &Path, components: &[&str]) -> Result<File, String> {
    let mut current = open_directory(root).map_err(|error| format!("open artifact root: {error}"))?;
    for component in components {
        current = open_directory_at(&current, component).map_err(|error| format!("open artifact directory {component}: {error}"))?;
    }
    Ok(current)
}

fn open_regular_file(root: &Path, relative: &str) -> Result<File, String> {
    let components = artifact_components(relative)?;
    let (name, parents) = components.split_last().ok_or_else(|| "artifact path is empty".to_string())?;
    let parent = open_relative_directory(root, parents)?;
    let file = open_file_at(&parent, name).map_err(|error| format!("open artifact {relative}: {error}"))?;
    let metadata = file.metadata().map_err(|error| format!("stat artifact {relative}: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err(format!("artifact is not a regular file: {relative}"));
    }
    Ok(file)
}

fn open_regular_file_from_fd(root: &File, relative: &str) -> Result<File, String> {
    let components = artifact_components(relative)?;
    let (name, parents) = components.split_last().ok_or_else(|| "artifact path is empty".to_string())?;
    let mut parent = root.try_clone().map_err(|error| format!("clone artifact spool root: {error}"))?;
    for component in parents {
        parent = open_directory_at(&parent, component).map_err(|error| format!("open spooled artifact directory: {error}"))?;
    }
    let file = open_file_at(&parent, name).map_err(|error| format!("open spooled artifact: {error}"))?;
    let metadata = file.metadata().map_err(|error| format!("stat spooled artifact: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err(format!("spooled artifact is not a regular file: {relative}"));
    }
    Ok(file)
}

fn create_relative_file(root: &File, relative: &str, mode: u32) -> Result<File, String> {
    let components = artifact_components(relative)?;
    let (name, parents) = components.split_last().ok_or_else(|| "artifact path is empty".to_string())?;
    let mut parent = root.try_clone().map_err(|error| format!("clone artifact spool root: {error}"))?;
    for component in parents {
        match unsafe { libc::mkdirat(parent.as_raw_fd(), CString::new(*component).unwrap().as_ptr(), 0o700) } {
            0 => {}
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::AlreadyExists => {}
            _ => return Err(format!("create artifact spool directory: {}", io::Error::last_os_error())),
        }
        parent = open_directory_at(&parent, component).map_err(|error| format!("open artifact spool directory: {error}"))?;
    }
    let file = create_file_at(&parent, name, mode & 0o777).map_err(|error| format!("create artifact spool file {relative}: {error}"))?;
    if unsafe { libc::fchmod(file.as_raw_fd(), (mode & 0o777) as libc::mode_t) } != 0 {
        return Err(format!("set artifact spool file mode: {}", io::Error::last_os_error()));
    }
    Ok(file)
}

fn copy_and_hash(source: &File, destination: &File, expected_size: u64, path: &str, control: &RemoteExecutionControl) -> Result<[u8; 32], String> {
    let mut source = source.try_clone().map_err(|error| format!("clone artifact source {path}: {error}"))?;
    let mut destination = destination.try_clone().map_err(|error| format!("clone artifact spool file {path}: {error}"))?;
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = [0u8; COPY_BUFFER_BYTES];
    loop {
        if control.is_cancelled() {
            return Err(format!("artifact capture cancelled: {path}"));
        }
        let count = source.read(&mut buffer).map_err(|error| format!("read artifact {path}: {error}"))?;
        if count == 0 {
            break;
        }
        copied = copied.checked_add(count as u64).ok_or_else(|| "artifact size overflow".to_string())?;
        if copied > expected_size {
            return Err(format!("artifact grew during capture: {path}"));
        }
        hasher.update(&buffer[..count]);
        destination.write_all(&buffer[..count]).map_err(|error| format!("write artifact spool {path}: {error}"))?;
    }
    if copied != expected_size {
        return Err(format!("artifact size changed during capture: {path}"));
    }
    destination.sync_all().map_err(|error| format!("sync artifact spool {path}: {error}"))?;
    Ok(hasher.finalize().into())
}

fn set_private_mode(path: &Path) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| format!("set private artifact mode: {error}"))
}

fn set_private_mode_fd(file: &File) -> Result<(), String> {
    set_private_mode_fd_io(file, 0o700).map_err(|error| format!("set private artifact mode: {error}"))
}

fn set_private_mode_fd_io(file: &File, mode: u32) -> io::Result<()> {
    if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn hex_id(bytes: [u8; 16]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
#[path = "artifact_ut.rs"]
mod tests;
