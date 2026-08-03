use crate::cfg::ProjectConfig;
use crate::remote::WorkspaceSessionId;
use crate::workspace::WorkspaceHandle;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const MAX_SNAPSHOT_ENTRIES: usize = 10_000;
pub const MAX_SNAPSHOT_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_SNAPSHOT_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_SNAPSHOT_PATH_BYTES: usize = 4 * 1024;
pub const MAX_SNAPSHOT_COMPONENT_BYTES: usize = 255;
pub const MAX_SNAPSHOT_DEPTH: usize = 64;
pub const MAX_SNAPSHOT_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
pub const SNAPSHOT_COPY_BUFFER_BYTES: usize = 64 * 1024;

static NEXT_STAGING_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SnapshotId([u8; 32]);

impl SnapshotId {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRelativePath(String);

impl SnapshotRelativePath {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_relative_path(&value, MAX_SNAPSHOT_PATH_BYTES, MAX_SNAPSHOT_COMPONENT_BYTES, MAX_SNAPSHOT_DEPTH)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotEntryKind {
    Directory,
    RegularFile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotEntry {
    path: SnapshotRelativePath,
    kind: SnapshotEntryKind,
    mode: u16,
    size: u64,
    content_digest: Option<[u8; 32]>,
}

impl SnapshotEntry {
    pub fn path(&self) -> &SnapshotRelativePath {
        &self.path
    }

    pub fn kind(&self) -> SnapshotEntryKind {
        self.kind
    }

    pub fn mode(&self) -> u16 {
        self.mode
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn content_digest(&self) -> Option<&[u8; 32]> {
        self.content_digest.as_ref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotLimits {
    pub max_entries: usize,
    pub max_total_bytes: u64,
    pub max_file_bytes: u64,
    pub max_path_bytes: usize,
    pub max_component_bytes: usize,
    pub max_depth: usize,
    pub max_manifest_bytes: usize,
    pub max_duration: Duration,
}

impl Default for SnapshotLimits {
    fn default() -> Self {
        Self {
            max_entries: MAX_SNAPSHOT_ENTRIES,
            max_total_bytes: MAX_SNAPSHOT_TOTAL_BYTES,
            max_file_bytes: MAX_SNAPSHOT_FILE_BYTES,
            max_path_bytes: MAX_SNAPSHOT_PATH_BYTES,
            max_component_bytes: MAX_SNAPSHOT_COMPONENT_BYTES,
            max_depth: MAX_SNAPSHOT_DEPTH,
            max_manifest_bytes: MAX_SNAPSHOT_MANIFEST_BYTES,
            max_duration: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotExclusionPolicy {
    basename_prunes: BTreeSet<String>,
    anchored_prunes: BTreeSet<String>,
}

impl SnapshotExclusionPolicy {
    pub fn from_config(config: &ProjectConfig, runtime_exclude: Option<&[String]>) -> Result<Self, String> {
        Self::from_patterns(config.effective_exclude(runtime_exclude))
    }

    pub fn from_patterns(patterns: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut policy = Self { basename_prunes: BTreeSet::new(), anchored_prunes: BTreeSet::new() };
        for name in [".git", ".bunker", ".bunkerbox", ".env", ".envrc", ".ssh"] {
            policy.basename_prunes.insert(name.to_string());
        }
        for pattern in patterns {
            policy.add_pattern(&pattern)?;
        }
        Ok(policy)
    }

    pub fn excludes(&self, path: &str) -> bool {
        let components = path.split('/');
        if components.clone().any(|component| self.basename_prunes.contains(component)) {
            return true;
        }
        self.anchored_prunes.iter().any(|prefix| path == prefix || path.starts_with(&format!("{prefix}/")))
    }

    fn add_pattern(&mut self, raw: &str) -> Result<(), String> {
        let pattern = raw.trim().trim_end_matches('/');
        if pattern.is_empty() || pattern.starts_with('/') || pattern.contains('\\') {
            return Err(format!("invalid snapshot exclusion: {raw}"));
        }
        let components = pattern.split('/').collect::<Vec<_>>();
        if components.iter().any(|component| component.is_empty() || *component == "." || *component == "..") {
            return Err(format!("invalid snapshot exclusion: {raw}"));
        }
        for component in &components {
            if component.len() > MAX_SNAPSHOT_COMPONENT_BYTES || component.as_bytes().contains(&0) {
                return Err(format!("snapshot exclusion component is too long: {raw}"));
            }
        }
        if components.len() == 1 {
            self.basename_prunes.insert(components[0].to_string());
        } else {
            let normalized = components.join("/");
            if normalized.len() > MAX_SNAPSHOT_PATH_BYTES {
                return Err(format!("snapshot exclusion is too long: {raw}"));
            }
            self.anchored_prunes.insert(normalized);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SnapshotHandle {
    session_id: WorkspaceSessionId,
    snapshot_id: SnapshotId,
}

impl SnapshotHandle {
    pub fn session_id(&self) -> WorkspaceSessionId {
        self.session_id
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    handle: SnapshotHandle,
    entries: Vec<SnapshotEntry>,
    total_file_bytes: u64,
}

impl WorkspaceSnapshot {
    pub fn handle(&self) -> &SnapshotHandle {
        &self.handle
    }

    pub fn entries(&self) -> &[SnapshotEntry] {
        &self.entries
    }

    pub fn total_file_bytes(&self) -> u64 {
        self.total_file_bytes
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotStore {
    root: PathBuf,
}

impl SnapshotStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn resolve(&self, handle: &SnapshotHandle) -> Result<WorkspaceSnapshot, String> {
        let manifest_path = self.manifest_path(handle);
        let metadata = fs::symlink_metadata(&manifest_path).map_err(|error| format!("snapshot is unavailable: {error}"))?;
        if !metadata.file_type().is_file() {
            return Err("snapshot manifest is not a regular file".to_string());
        }
        if metadata.len() > MAX_SNAPSHOT_MANIFEST_BYTES as u64 {
            return Err("stored snapshot manifest exceeds limit".to_string());
        }
        let stored: StoredSnapshot = serde_json::from_slice(&fs::read(&manifest_path).map_err(|error| format!("read snapshot manifest: {error}"))?)
            .map_err(|error| format!("decode snapshot manifest: {error}"))?;
        if stored.session_id != handle.session_id.0 {
            return Err("snapshot session mismatch".to_string());
        }
        let (entries, total_file_bytes) = stored.into_entries()?;
        let snapshot_id = snapshot_id(&entries);
        if snapshot_id != handle.snapshot_id {
            return Err("snapshot manifest identity mismatch".to_string());
        }
        Ok(WorkspaceSnapshot { handle: handle.clone(), entries, total_file_bytes })
    }

    pub fn remove(&self, handle: &SnapshotHandle) -> Result<(), String> {
        let path = self.snapshot_path(handle);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(&path).map_err(|error| format!("remove snapshot: {error}")),
            Ok(_) => Err("snapshot publication is not a directory".to_string()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("inspect snapshot for removal: {error}")),
        }
    }

    pub fn materialize(&self, handle: &SnapshotHandle, destination: &Path) -> Result<MaterializedWorkspace, String> {
        let snapshot = self.resolve(handle)?;
        if fs::symlink_metadata(destination).is_ok() {
            return Err("materialization destination already exists".to_string());
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("create materialization parent: {error}"))?;
        }
        fs::create_dir(destination).map_err(|error| format!("create materialization destination: {error}"))?;
        let mut cleanup = MaterializationGuard { path: destination.to_path_buf(), committed: false };
        set_mode(destination, 0o700)?;
        let source_root = open_directory(&self.files_path(handle))?;
        let destination_root = open_directory(destination)?;

        for entry in snapshot.entries() {
            match entry.kind {
                SnapshotEntryKind::Directory => ensure_destination_directory(&destination_root, entry.path.as_str(), entry.mode)?,
                SnapshotEntryKind::RegularFile => {
                    let source = open_relative_file(&source_root, entry.path.as_str(), libc::O_RDONLY)?;
                    let destination_file = create_relative_file(&destination_root, entry.path.as_str(), entry.mode)?;
                    copy_materialized_file(
                        &source,
                        &destination_file,
                        entry.size,
                        entry.content_digest.ok_or_else(|| "regular file has no digest".to_string())?,
                        entry.path.as_str(),
                    )?;
                }
            }
        }

        cleanup.committed = true;
        Ok(MaterializedWorkspace { root: destination.to_path_buf() })
    }

    fn manifest_path(&self, handle: &SnapshotHandle) -> PathBuf {
        self.snapshot_path(handle).join("manifest.json")
    }

    fn snapshot_path(&self, handle: &SnapshotHandle) -> PathBuf {
        self.root.join(hex(&handle.session_id.0)).join(hex(&handle.snapshot_id.0))
    }

    fn files_path(&self, handle: &SnapshotHandle) -> PathBuf {
        self.snapshot_path(handle).join("files")
    }

    pub(crate) fn root_for_cleanup(&self) -> PathBuf {
        self.root.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedWorkspace {
    root: PathBuf,
}

impl MaterializedWorkspace {
    pub fn root(&self) -> &Path {
        &self.root
    }
}

struct MaterializationGuard {
    path: PathBuf,
    committed: bool,
}

impl Drop for MaterializationGuard {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub struct SnapshotBuilder {
    store: SnapshotStore,
    limits: SnapshotLimits,
    exclusions: SnapshotExclusionPolicy,
}

impl SnapshotBuilder {
    pub fn new(store: SnapshotStore, limits: SnapshotLimits, exclusions: SnapshotExclusionPolicy) -> Self {
        Self { store, limits, exclusions }
    }

    pub fn build(&self, workspace: &WorkspaceHandle, session_id: WorkspaceSessionId) -> Result<WorkspaceSnapshot, String> {
        self.build_root(workspace.path(), session_id)
    }

    pub(crate) fn build_root(&self, workspace_root: &Path, session_id: WorkspaceSessionId) -> Result<WorkspaceSnapshot, String> {
        validate_limits(&self.limits)?;
        if session_id.0 == [0; 16] {
            return Err("snapshot requires an authoritative nonzero workspace session".to_string());
        }
        let started = Instant::now();
        let canonical_root = fs::canonicalize(workspace_root).map_err(|error| format!("resolve snapshot workspace: {error}"))?;
        let root = open_directory(&canonical_root)?;
        let root_stat = stat_fd(root.as_raw_fd())?;
        let root_device = root_stat.st_dev;
        let store_root = prepare_store_root(&self.store.root)?;
        let staging_root = store_root.join(".staging");
        fs::create_dir_all(&staging_root).map_err(|error| format!("create snapshot staging root: {error}"))?;
        set_mode(&staging_root, 0o700)?;
        let stage = staging_root.join(format!("{}-{}", hex(&session_id.0), NEXT_STAGING_ID.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&stage).map_err(|error| format!("create snapshot staging directory: {error}"))?;
        set_mode(&stage, 0o700)?;
        let mut cleanup = StagingGuard { path: stage.clone(), committed: false };
        let files_root = stage.join("files");
        fs::create_dir(&files_root).map_err(|error| format!("create snapshot content directory: {error}"))?;

        let mut state = WalkState {
            entries: Vec::new(),
            total_file_bytes: 0,
            started,
            root_device,
            stage_files: files_root,
            next_buffer: vec![0; SNAPSHOT_COPY_BUFFER_BYTES],
        };
        walk_directory(root.as_raw_fd(), "", 0, &self.limits, &self.exclusions, &mut state)?;
        state.entries.sort_by(|left, right| left.path.as_str().cmp(right.path.as_str()));
        let id = snapshot_id(&state.entries);
        let stored = StoredSnapshot::from_entries(session_id, &state.entries, state.total_file_bytes);
        let manifest = serde_json::to_vec(&stored).map_err(|error| format!("encode snapshot manifest: {error}"))?;
        if manifest.len() > self.limits.max_manifest_bytes {
            return Err(format!("snapshot manifest exceeds maximum size {}", self.limits.max_manifest_bytes));
        }
        fs::write(stage.join("manifest.json"), manifest).map_err(|error| format!("write snapshot manifest: {error}"))?;

        let final_session = store_root.join(hex(&session_id.0));
        fs::create_dir_all(&final_session).map_err(|error| format!("create snapshot session directory: {error}"))?;
        set_mode(&final_session, 0o700)?;
        let final_path = final_session.join(hex(&id.0));
        if !final_path.exists() {
            fs::rename(&stage, &final_path).map_err(|error| format!("publish snapshot: {error}"))?;
        } else {
            if !fs::symlink_metadata(&final_path).map_err(|error| format!("inspect existing snapshot: {error}"))?.file_type().is_dir() {
                return Err("existing snapshot publication is not a directory".to_string());
            }
            fs::remove_dir_all(&stage).map_err(|error| format!("discard duplicate snapshot staging: {error}"))?;
        }
        cleanup.committed = true;
        Ok(WorkspaceSnapshot {
            handle: SnapshotHandle { session_id, snapshot_id: id },
            entries: state.entries,
            total_file_bytes: state.total_file_bytes,
        })
    }
}

struct StagingGuard {
    path: PathBuf,
    committed: bool,
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct WalkState {
    entries: Vec<SnapshotEntry>,
    total_file_bytes: u64,
    started: Instant,
    root_device: libc::dev_t,
    stage_files: PathBuf,
    next_buffer: Vec<u8>,
}

fn walk_directory(
    directory_fd: RawFd, parent: &str, depth: usize, limits: &SnapshotLimits, exclusions: &SnapshotExclusionPolicy, state: &mut WalkState,
) -> Result<(), String> {
    check_deadline(state.started, limits)?;
    if depth > limits.max_depth {
        return Err(format!("snapshot exceeds maximum depth {}", limits.max_depth));
    }
    for name in read_directory_names(directory_fd)? {
        check_deadline(state.started, limits)?;
        let component = name.to_str().ok_or_else(|| "snapshot contains a non-UTF-8 path component".to_string())?;
        validate_component(component, limits.max_component_bytes)?;
        let relative = if parent.is_empty() { component.to_string() } else { format!("{parent}/{component}") };
        validate_relative_path(&relative, limits.max_path_bytes, limits.max_component_bytes, limits.max_depth)?;
        if exclusions.excludes(&relative) {
            continue;
        }
        let child_stat = stat_at(directory_fd, &name)?;
        if child_stat.st_dev != state.root_device {
            return Err(format!("snapshot entry crosses filesystem boundary: {relative}"));
        }
        let entry_kind = child_kind(&child_stat, &relative)?;
        match entry_kind {
            SnapshotEntryKind::Directory => {
                add_entry_limit(state.entries.len(), limits)?;
                let child = open_child_directory(directory_fd, &name, &relative)?;
                let mode = normalized_mode(child_stat.st_mode);
                state.entries.push(SnapshotEntry {
                    path: SnapshotRelativePath::new(relative.clone())?,
                    kind: SnapshotEntryKind::Directory,
                    mode,
                    size: 0,
                    content_digest: None,
                });
                walk_directory(child.as_raw_fd(), &relative, depth + 1, limits, exclusions, state)?;
            }
            SnapshotEntryKind::RegularFile => {
                add_entry_limit(state.entries.len(), limits)?;
                if child_stat.st_nlink > 1 {
                    return Err(format!("snapshot rejects hard-linked file: {relative}"));
                }
                let size = checked_file_size(child_stat.st_size, limits.max_file_bytes, &relative)?;
                let new_total = state.total_file_bytes.checked_add(size).ok_or_else(|| "snapshot total size overflow".to_string())?;
                if new_total > limits.max_total_bytes {
                    return Err(format!("snapshot exceeds maximum total size {}", limits.max_total_bytes));
                }
                let (digest, bytes_read) = copy_and_hash_file(directory_fd, &name, &relative, size, &child_stat, limits, state)?;
                if bytes_read != size {
                    return Err(format!("file changed while snapshotting: {relative}"));
                }
                state.total_file_bytes = new_total;
                state.entries.push(SnapshotEntry {
                    path: SnapshotRelativePath::new(relative)?,
                    kind: SnapshotEntryKind::RegularFile,
                    mode: normalized_mode(child_stat.st_mode),
                    size,
                    content_digest: Some(digest),
                });
            }
        }
    }
    Ok(())
}

fn copy_and_hash_file(
    parent_fd: RawFd, name: &OsStr, relative: &str, expected_size: u64, expected_stat: &libc::stat, limits: &SnapshotLimits, state: &mut WalkState,
) -> Result<([u8; 32], u64), String> {
    let file = open_child_file(parent_fd, name, relative)?;
    let opened_stat = stat_fd(file.as_raw_fd())?;
    compare_file_stat(expected_stat, &opened_stat, relative)?;
    let destination = state.stage_files.join(relative);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create staged parent for {relative}: {error}"))?;
    }
    let mut staged = File::create(&destination).map_err(|error| format!("create staged file {relative}: {error}"))?;
    let mut hasher = Sha256::new();
    let mut read_bytes = 0u64;
    loop {
        check_deadline(state.started, limits)?;
        let count = (&file).read(&mut state.next_buffer).map_err(|error| format!("read snapshot file {relative}: {error}"))?;
        if count == 0 {
            break;
        }
        read_bytes = read_bytes.checked_add(count as u64).ok_or_else(|| format!("snapshot file size overflow: {relative}"))?;
        if read_bytes > expected_size || read_bytes > limits.max_file_bytes {
            return Err(format!("file changed beyond snapshot limit: {relative}"));
        }
        hasher.update(&state.next_buffer[..count]);
        staged.write_all(&state.next_buffer[..count]).map_err(|error| format!("stage snapshot file {relative}: {error}"))?;
    }
    staged.sync_all().map_err(|error| format!("flush staged file {relative}: {error}"))?;
    let final_stat = stat_fd(file.as_raw_fd())?;
    compare_file_stat(expected_stat, &final_stat, relative)?;
    if read_bytes != expected_size {
        return Err(format!("file changed while snapshotting: {relative}"));
    }
    set_mode(&destination, normalized_mode(expected_stat.st_mode))?;
    Ok((hasher.finalize().into(), read_bytes))
}

fn check_deadline(started: Instant, limits: &SnapshotLimits) -> Result<(), String> {
    if started.elapsed() >= limits.max_duration {
        return Err("snapshot creation deadline exceeded".to_string());
    }
    Ok(())
}

fn add_entry_limit(count: usize, limits: &SnapshotLimits) -> Result<(), String> {
    if count >= limits.max_entries {
        return Err(format!("snapshot exceeds maximum entry count {}", limits.max_entries));
    }
    Ok(())
}

fn validate_limits(limits: &SnapshotLimits) -> Result<(), String> {
    if limits.max_entries == 0
        || limits.max_total_bytes == 0
        || limits.max_file_bytes == 0
        || limits.max_path_bytes == 0
        || limits.max_path_bytes > MAX_SNAPSHOT_PATH_BYTES
        || limits.max_component_bytes == 0
        || limits.max_component_bytes > MAX_SNAPSHOT_COMPONENT_BYTES
        || limits.max_depth == 0
        || limits.max_manifest_bytes == 0
        || limits.max_duration.is_zero()
    {
        return Err("invalid snapshot limits".to_string());
    }
    Ok(())
}

fn checked_file_size(size: libc::off_t, max: u64, path: &str) -> Result<u64, String> {
    if size < 0 {
        return Err(format!("snapshot file has invalid size: {path}"));
    }
    let size = size as u64;
    if size > max {
        return Err(format!("snapshot file exceeds maximum size {max}: {path}"));
    }
    Ok(size)
}

fn validate_relative_path(value: &str, max_path: usize, max_component: usize, max_depth: usize) -> Result<(), String> {
    if value.is_empty() || value.starts_with('/') || value.contains('\\') || value.len() > max_path {
        return Err(format!("invalid snapshot relative path: {value}"));
    }
    let components = value.split('/').collect::<Vec<_>>();
    if components.len() > max_depth || components.iter().any(|part| part.is_empty() || *part == "." || *part == "..") {
        return Err(format!("invalid snapshot relative path: {value}"));
    }
    components.iter().try_for_each(|part| validate_component(part, max_component))
}

fn validate_component(value: &str, max: usize) -> Result<(), String> {
    if value.is_empty() || value == "." || value == ".." || value.len() > max || value.as_bytes().contains(&0) {
        return Err(format!("invalid snapshot path component: {value}"));
    }
    Ok(())
}

fn child_kind(stat: &libc::stat, path: &str) -> Result<SnapshotEntryKind, String> {
    match stat.st_mode & libc::S_IFMT {
        libc::S_IFDIR => Ok(SnapshotEntryKind::Directory),
        libc::S_IFREG => Ok(SnapshotEntryKind::RegularFile),
        libc::S_IFLNK => Err(format!("snapshot rejects symlink: {path}")),
        _ => Err(format!("snapshot rejects special file: {path}")),
    }
}

fn normalized_mode(mode: libc::mode_t) -> u16 {
    (mode & 0o777) as u16
}

fn compare_file_stat(expected: &libc::stat, actual: &libc::stat, path: &str) -> Result<(), String> {
    if expected.st_dev != actual.st_dev
        || expected.st_ino != actual.st_ino
        || expected.st_mode & libc::S_IFMT != actual.st_mode & libc::S_IFMT
        || expected.st_size != actual.st_size
        || expected.st_nlink != actual.st_nlink
        || expected.st_mode & 0o777 != actual.st_mode & 0o777
    {
        return Err(format!("file changed while snapshotting: {path}"));
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u16) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode as u32)).map_err(|error| format!("set staged file mode: {error}"))
}

fn prepare_store_root(root: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(root).map_err(|error| format!("create snapshot store: {error}"))?;
    fs::canonicalize(root).map_err(|error| format!("resolve snapshot store: {error}"))
}

fn open_directory(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| format!("open snapshot workspace: {error}"))
}

fn open_child_directory(parent: RawFd, name: &OsStr, path: &str) -> Result<File, String> {
    open_at(parent, name, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .map_err(|error| format!("open snapshot directory {path}: {error}"))
}

fn open_child_file(parent: RawFd, name: &OsStr, path: &str) -> Result<File, String> {
    open_at(parent, name, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC).map_err(|error| format!("open snapshot file {path}: {error}"))
}

fn open_at(parent: RawFd, name: &OsStr, flags: i32) -> io::Result<File> {
    let name = CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in snapshot path"))?;
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_at_mode(parent: RawFd, name: &OsStr, flags: i32, mode: u32) -> io::Result<File> {
    let name = CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in snapshot path"))?;
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags, mode) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn ensure_destination_directory(root: &File, relative: &str, mode: u16) -> Result<(), String> {
    let mut current = root.try_clone().map_err(|error| format!("clone materialization root: {error}"))?;
    for component in relative.split('/') {
        let name = OsStr::new(component);
        let name = CString::new(name.as_bytes()).map_err(|_| "NUL in materialization path".to_string())?;
        let result = unsafe { libc::mkdirat(current.as_raw_fd(), name.as_ptr(), 0o700) };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(format!("create materialized directory {relative}: {error}"));
            }
        }
        current = open_at(current.as_raw_fd(), OsStr::new(component), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .map_err(|error| format!("open materialized directory {relative}: {error}"))?;
    }
    if unsafe { libc::fchmod(current.as_raw_fd(), mode as libc::mode_t) } != 0 {
        return Err(format!("set materialized directory mode {relative}: {}", io::Error::last_os_error()));
    }
    Ok(())
}

fn create_relative_file(root: &File, relative: &str, mode: u16) -> Result<File, String> {
    let mut components = relative.split('/').collect::<Vec<_>>();
    let file_name = components.pop().ok_or_else(|| "empty materialization path".to_string())?;
    let mut parent = root.try_clone().map_err(|error| format!("clone materialization root: {error}"))?;
    for component in components {
        parent = open_at(parent.as_raw_fd(), OsStr::new(component), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .map_err(|error| format!("open materialized parent {relative}: {error}"))?;
    }
    open_at_mode(
        parent.as_raw_fd(),
        OsStr::new(file_name),
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        (mode & 0o777) as u32,
    )
    .map_err(|error| format!("create materialized file {relative}: {error}"))
}

fn open_relative_file(root: &File, relative: &str, flags: i32) -> Result<File, String> {
    let mut components = relative.split('/').collect::<Vec<_>>();
    let file_name = components.pop().ok_or_else(|| "empty snapshot content path".to_string())?;
    let mut parent = root.try_clone().map_err(|error| format!("clone snapshot content root: {error}"))?;
    for component in components {
        parent = open_at(parent.as_raw_fd(), OsStr::new(component), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .map_err(|error| format!("open snapshot content parent {relative}: {error}"))?;
    }
    open_at(parent.as_raw_fd(), OsStr::new(file_name), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .map_err(|error| format!("open snapshot content {relative}: {error}"))
}

fn copy_materialized_file(source: &File, destination: &File, expected_size: u64, expected_digest: [u8; 32], path: &str) -> Result<(), String> {
    let mut source = source.try_clone().map_err(|error| format!("clone snapshot content {path}: {error}"))?;
    let mut destination = destination.try_clone().map_err(|error| format!("clone materialized file {path}: {error}"))?;
    let mut buffer = vec![0u8; SNAPSHOT_COPY_BUFFER_BYTES];
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    loop {
        let count = source.read(&mut buffer).map_err(|error| format!("read snapshot content {path}: {error}"))?;
        if count == 0 {
            break;
        }
        copied = copied.checked_add(count as u64).ok_or_else(|| format!("materialized size overflow: {path}"))?;
        if copied > expected_size {
            return Err(format!("snapshot content is larger than manifest: {path}"));
        }
        hasher.update(&buffer[..count]);
        destination.write_all(&buffer[..count]).map_err(|error| format!("write materialized file {path}: {error}"))?;
    }
    if copied != expected_size || hasher.finalize().as_slice() != expected_digest {
        return Err(format!("snapshot content digest mismatch: {path}"));
    }
    destination.sync_all().map_err(|error| format!("flush materialized file {path}: {error}"))?;
    Ok(())
}

fn stat_at(parent: RawFd, name: &OsStr) -> Result<libc::stat, String> {
    let name = CString::new(name.as_bytes()).map_err(|_| "NUL in snapshot path".to_string())?;
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstatat(parent, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(format!("stat snapshot entry: {}", io::Error::last_os_error()));
    }
    Ok(stat)
}

fn stat_fd(fd: RawFd) -> Result<libc::stat, String> {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(format!("stat snapshot descriptor: {}", io::Error::last_os_error()));
    }
    Ok(stat)
}

fn read_directory_names(fd: RawFd) -> Result<Vec<OsString>, String> {
    let duplicate = unsafe { libc::dup(fd) };
    if duplicate < 0 {
        return Err(format!("duplicate snapshot directory: {}", io::Error::last_os_error()));
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(format!("open snapshot directory stream: {}", io::Error::last_os_error()));
    }
    let stream = DirectoryStream(stream);
    let mut names = Vec::new();
    loop {
        set_errno(0);
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let errno = get_errno();
            if errno != 0 {
                return Err(format!("read snapshot directory: {errno}"));
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(OsString::from_vec(name.to_vec()));
        }
    }
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(names)
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0) };
    }
}

fn set_errno(value: i32) {
    unsafe { *libc::__errno_location() = value };
}

fn get_errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

#[derive(Serialize, Deserialize)]
struct StoredSnapshot {
    session_id: [u8; 16],
    entries: Vec<StoredEntry>,
    total_file_bytes: u64,
}

#[derive(Serialize, Deserialize)]
struct StoredEntry {
    path: String,
    kind: SnapshotEntryKind,
    mode: u16,
    size: u64,
    content_digest: Option<[u8; 32]>,
}

impl StoredSnapshot {
    fn from_entries(session_id: WorkspaceSessionId, entries: &[SnapshotEntry], total_file_bytes: u64) -> Self {
        Self {
            session_id: session_id.0,
            entries: entries
                .iter()
                .map(|entry| StoredEntry {
                    path: entry.path.as_str().to_string(),
                    kind: entry.kind,
                    mode: entry.mode,
                    size: entry.size,
                    content_digest: entry.content_digest,
                })
                .collect(),
            total_file_bytes,
        }
    }

    fn into_entries(self) -> Result<(Vec<SnapshotEntry>, u64), String> {
        if self.entries.len() > MAX_SNAPSHOT_ENTRIES {
            return Err("stored snapshot exceeds maximum entry count".to_string());
        }
        let mut total_file_bytes = 0u64;
        let mut previous_path = None;
        let entries = self
            .entries
            .into_iter()
            .map(|entry| {
                let path = SnapshotRelativePath::new(entry.path)?;
                if previous_path.as_deref().is_some_and(|previous: &str| previous >= path.as_str()) {
                    return Err("snapshot manifest entries are not strictly ordered".to_string());
                }
                previous_path = Some(path.as_str().to_string());
                if matches!(entry.kind, SnapshotEntryKind::Directory) && (entry.size != 0 || entry.content_digest.is_some()) {
                    return Err("invalid directory snapshot entry".to_string());
                }
                if matches!(entry.kind, SnapshotEntryKind::RegularFile) && entry.content_digest.is_none() {
                    return Err("invalid regular-file snapshot entry".to_string());
                }
                if matches!(entry.kind, SnapshotEntryKind::RegularFile) {
                    if entry.size > MAX_SNAPSHOT_FILE_BYTES {
                        return Err("stored snapshot file exceeds maximum size".to_string());
                    }
                    total_file_bytes = total_file_bytes.checked_add(entry.size).ok_or_else(|| "snapshot manifest size overflow".to_string())?;
                    if total_file_bytes > MAX_SNAPSHOT_TOTAL_BYTES {
                        return Err("stored snapshot exceeds maximum total size".to_string());
                    }
                }
                Ok(SnapshotEntry { path, kind: entry.kind, mode: entry.mode & 0o777, size: entry.size, content_digest: entry.content_digest })
            })
            .collect::<Result<Vec<_>, String>>()?;
        if total_file_bytes != self.total_file_bytes {
            return Err("snapshot manifest total size mismatch".to_string());
        }
        Ok((entries, total_file_bytes))
    }
}

fn snapshot_id(entries: &[SnapshotEntry]) -> SnapshotId {
    let mut canonical = Vec::new();
    for entry in entries {
        canonical.push(match entry.kind {
            SnapshotEntryKind::Directory => 0,
            SnapshotEntryKind::RegularFile => 1,
        });
        canonical.extend_from_slice(&(entry.path.as_str().len() as u32).to_le_bytes());
        canonical.extend_from_slice(entry.path.as_str().as_bytes());
        canonical.extend_from_slice(&entry.mode.to_le_bytes());
        canonical.extend_from_slice(&entry.size.to_le_bytes());
        if let Some(digest) = entry.content_digest {
            canonical.extend_from_slice(&digest);
        }
    }
    SnapshotId(Sha256::digest(canonical).into())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
#[path = "snapshot_ut.rs"]
mod tests;
