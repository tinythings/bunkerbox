//! A bounded binary protocol for a host-side worker reached over SSH.
//!
//! This protocol is deliberately independent from `vscomm`.  A frame is:
//!
//! ```text
//! magic[4] version[u16] kind[u8] payload_length[u32] payload[payload_length]
//! ```
//!
//! All integer fields are little-endian.  The payload length is checked before
//! allocating a payload buffer, and every length and count inside a payload is
//! checked before it can drive an allocation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Read, Write};

#[cfg(feature = "async")]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const WORKER_PROTOCOL_MAGIC: [u8; 4] = *b"BBWK";
pub const WORKER_MAGIC: [u8; 4] = WORKER_PROTOCOL_MAGIC;
pub const WORKER_PROTOCOL_VERSION: u16 = 1;
pub const WORKER_VERSION: u16 = WORKER_PROTOCOL_VERSION;
pub const WORKER_ARTIFACT_PROTOCOL_VERSION: u16 = 2;
/// Adds trusted target-PATH command resolution without changing V1/V2 fields.
pub const WORKER_COMMAND_PROTOCOL_VERSION: u16 = 3;
/// Adds validated workspace-relative symlink entries to upload manifests.
pub const WORKER_SYMLINK_PROTOCOL_VERSION: u16 = 4;
pub const WORKER_FRAME_HEADER_LEN: usize = 4 + 2 + 1 + 4;
pub const WORKER_FRAME_HEADER_SIZE: usize = WORKER_FRAME_HEADER_LEN;
pub const WORKER_ID_LEN: usize = 16;
pub const WORKER_DIGEST_LEN: usize = 32;

/// Maximum bytes in one worker payload.  The declared length is rejected
/// before a buffer of this size is allocated.
pub const MAX_WORKER_FRAME_PAYLOAD: usize = 1024 * 1024;
pub const MAX_WORKER_PAYLOAD: usize = MAX_WORKER_FRAME_PAYLOAD;
pub const MAX_WORKER_FRAME_BYTES: usize = WORKER_FRAME_HEADER_LEN + MAX_WORKER_FRAME_PAYLOAD;
pub const MAX_WORKER_FRAME_LENGTH: usize = MAX_WORKER_FRAME_BYTES;
pub const MAX_WORKER_STRING_BYTES: usize = 4 * 1024;
pub const MAX_WORKER_PATH_BYTES: usize = 4 * 1024;
pub const MAX_WORKER_ENTRY_PATH_BYTES: usize = MAX_WORKER_PATH_BYTES;
pub const MAX_WORKER_PATH_COMPONENT_BYTES: usize = 255;
pub const MAX_WORKER_PATH_DEPTH: usize = 64;
pub const MAX_WORKER_CWD_BYTES: usize = MAX_WORKER_PATH_BYTES;
pub const MAX_WORKER_TOOL_BYTES: usize = 256;
pub const MAX_WORKER_EXECUTABLE_PATH_BYTES: usize = 4 * 1024;
pub const MAX_WORKER_EXECUTABLE_BYTES: usize = MAX_WORKER_EXECUTABLE_PATH_BYTES;
pub const MAX_WORKER_ARG_COUNT: usize = 256;
pub const MAX_WORKER_ARGUMENT_COUNT: usize = MAX_WORKER_ARG_COUNT;
pub const MAX_WORKER_ARG_BYTES: usize = 4 * 1024;
pub const MAX_WORKER_ARG_TOTAL_BYTES: usize = 256 * 1024;
pub const MAX_WORKER_ENV_COUNT: usize = 128;
pub const MAX_WORKER_ENVIRONMENT_COUNT: usize = MAX_WORKER_ENV_COUNT;
pub const MAX_WORKER_ENV_KEY_BYTES: usize = 256;
pub const MAX_WORKER_ENV_VALUE_BYTES: usize = 4 * 1024;
pub const MAX_WORKER_ENV_TOTAL_BYTES: usize = 256 * 1024;
pub const MAX_WORKER_UPLOAD_ENTRIES: usize = 4096;
pub const MAX_WORKER_ENTRY_COUNT: usize = MAX_WORKER_UPLOAD_ENTRIES;
pub const MAX_WORKER_UPLOAD_ENTRY_COUNT: usize = MAX_WORKER_UPLOAD_ENTRIES;
pub const MAX_WORKER_FILE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_WORKER_TOTAL_UPLOAD_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_WORKER_MANIFEST_BYTES: usize = 512 * 1024;
pub const MAX_WORKER_CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_WORKER_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_WORKER_ERROR_BYTES: usize = 4 * 1024;
pub const MAX_WORKER_ARTIFACT_ENTRIES: usize = 256;
pub const MAX_WORKER_ARTIFACT_PATH_BYTES: usize = MAX_WORKER_PATH_BYTES;
pub const MAX_WORKER_ARTIFACT_FILE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_WORKER_ARTIFACT_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerProtocolError {
    Io(String),
    Invalid(String),
}

pub type WorkerResult<T> = Result<T, WorkerProtocolError>;

impl WorkerProtocolError {
    pub fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid(_))
    }

    pub fn contains(&self, needle: &str) -> bool {
        match self {
            Self::Io(message) | Self::Invalid(message) => message.contains(needle),
        }
    }
}

impl fmt::Display for WorkerProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(formatter, "worker I/O error: {message}"),
            Self::Invalid(message) => write!(formatter, "invalid worker protocol: {message}"),
        }
    }
}

impl std::error::Error for WorkerProtocolError {}

impl From<io::Error> for WorkerProtocolError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

macro_rules! worker_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name(pub [u8; WORKER_ID_LEN]);

        impl $name {
            pub const fn new(bytes: [u8; WORKER_ID_LEN]) -> Self {
                Self(bytes)
            }

            pub const fn as_bytes(&self) -> &[u8; WORKER_ID_LEN] {
                &self.0
            }

            pub const fn into_bytes(self) -> [u8; WORKER_ID_LEN] {
                self.0
            }
        }

        impl From<[u8; WORKER_ID_LEN]> for $name {
            fn from(bytes: [u8; WORKER_ID_LEN]) -> Self {
                Self(bytes)
            }
        }
    };
}

worker_id!(WorkerRequestId);
worker_id!(WorkerSessionId);
worker_id!(WorkerUploadId);
worker_id!(WorkerArtifactSetId);

pub type RequestId = WorkerRequestId;
pub type SessionId = WorkerSessionId;
pub type UploadId = WorkerUploadId;
pub type UploadToken = WorkerUploadId;
pub type WorkerUploadToken = WorkerUploadId;
pub type WorkerDigest = [u8; WORKER_DIGEST_LEN];

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerFrameKind {
    Hello = 1,
    UploadBegin = 2,
    UploadEntry = 3,
    UploadFileChunk = 4,
    UploadComplete = 5,
    Build = 6,
    Cleanup = 7,
    SyncProgress = 8,
    Stdout = 9,
    Stderr = 10,
    Completed = 11,
    Error = 12,
    ArtifactManifest = 13,
    FetchArtifact = 14,
    ArtifactChunk = 15,
    ArtifactComplete = 16,
}

impl WorkerFrameKind {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Hello),
            2 => Some(Self::UploadBegin),
            3 => Some(Self::UploadEntry),
            4 => Some(Self::UploadFileChunk),
            5 => Some(Self::UploadComplete),
            6 => Some(Self::Build),
            7 => Some(Self::Cleanup),
            8 => Some(Self::SyncProgress),
            9 => Some(Self::Stdout),
            10 => Some(Self::Stderr),
            11 => Some(Self::Completed),
            12 => Some(Self::Error),
            13 => Some(Self::ArtifactManifest),
            14 => Some(Self::FetchArtifact),
            15 => Some(Self::ArtifactChunk),
            16 => Some(Self::ArtifactComplete),
            _ => None,
        }
    }
}

impl TryFrom<u8> for WorkerFrameKind {
    type Error = WorkerProtocolError;

    fn try_from(value: u8) -> WorkerResult<Self> {
        Self::from_u8(value).ok_or_else(|| invalid(format!("unknown worker frame kind: {value}")))
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerOperation {
    Protocol = 0,
    Upload = 1,
    Build = 2,
    Cleanup = 3,
    Sync = 4,
    Artifact = 5,
}

impl WorkerOperation {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(value: u8) -> WorkerResult<Self> {
        match value {
            0 => Ok(Self::Protocol),
            1 => Ok(Self::Upload),
            2 => Ok(Self::Build),
            3 => Ok(Self::Cleanup),
            4 => Ok(Self::Sync),
            5 => Ok(Self::Artifact),
            _ => Err(invalid(format!("unknown worker operation: {value}"))),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerErrorKind {
    WorkerProtocol = 1,
    Upload = 2,
    Build = 3,
    Cleanup = 4,
    Sync = 5,
    Artifact = 6,
}

pub type WorkerErrorClass = WorkerErrorKind;

impl WorkerErrorKind {
    #[allow(non_upper_case_globals)]
    pub const Protocol: Self = Self::WorkerProtocol;

    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(value: u8) -> WorkerResult<Self> {
        match value {
            1 => Ok(Self::WorkerProtocol),
            2 => Ok(Self::Upload),
            3 => Ok(Self::Build),
            4 => Ok(Self::Cleanup),
            5 => Ok(Self::Sync),
            6 => Ok(Self::Artifact),
            _ => Err(invalid(format!("unknown worker error kind: {value}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkerRelativePath(String);

impl WorkerRelativePath {
    pub fn new(value: impl Into<String>) -> WorkerResult<Self> {
        let value = value.into();
        validate_relative_path("worker relative path", &value, true)?;
        Ok(Self(value))
    }

    fn for_entry(value: impl Into<String>) -> WorkerResult<Self> {
        let value = value.into();
        validate_relative_path("worker entry path", &value, false)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl AsRef<str> for WorkerRelativePath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<String> for WorkerRelativePath {
    type Error = WorkerProtocolError;

    fn try_from(value: String) -> WorkerResult<Self> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkerTool(String);

impl WorkerTool {
    pub fn new(value: impl Into<String>) -> WorkerResult<Self> {
        let value = value.into();
        validate_tool(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for WorkerTool {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<String> for WorkerTool {
    type Error = WorkerProtocolError;

    fn try_from(value: String) -> WorkerResult<Self> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkerExecutablePath(String);

impl WorkerExecutablePath {
    pub fn new(value: impl Into<String>) -> WorkerResult<Self> {
        let value = value.into();
        validate_executable_path(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for WorkerExecutablePath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<String> for WorkerExecutablePath {
    type Error = WorkerProtocolError;

    fn try_from(value: String) -> WorkerResult<Self> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkerArtifactPath(String);

impl WorkerArtifactPath {
    pub fn new(value: impl Into<String>) -> WorkerResult<Self> {
        let value = value.into();
        validate_relative_path("worker artifact path", &value, false)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for WorkerArtifactPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<String> for WorkerArtifactPath {
    type Error = WorkerProtocolError;

    fn try_from(value: String) -> WorkerResult<Self> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerArtifactEntry {
    pub path: WorkerArtifactPath,
    pub mode: u32,
    pub size: u64,
    pub digest: WorkerDigest,
}

impl WorkerArtifactEntry {
    pub fn new(path: impl Into<String>, mode: u32, size: u64, digest: WorkerDigest) -> WorkerResult<Self> {
        let entry = Self { path: WorkerArtifactPath::new(path)?, mode, size, digest };
        entry.validate()
    }

    pub fn validate(&self) -> WorkerResult<Self> {
        if self.mode & !0o777 != 0 {
            return Err(invalid(format!("worker artifact mode has unsupported bits: {:o}", self.mode)));
        }
        if self.size > MAX_WORKER_ARTIFACT_FILE_BYTES {
            return Err(invalid(format!("worker artifact exceeds maximum file size {MAX_WORKER_ARTIFACT_FILE_BYTES}")));
        }
        Ok(self.clone())
    }

    pub fn path(&self) -> &WorkerArtifactPath {
        &self.path
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn digest(&self) -> &WorkerDigest {
        &self.digest
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerEntryKind {
    Directory = 1,
    File = 2,
    Symlink = 3,
}

impl WorkerEntryKind {
    pub fn from_u8(value: u8) -> WorkerResult<Self> {
        match value {
            1 => Ok(Self::Directory),
            2 => Ok(Self::File),
            3 => Ok(Self::Symlink),
            _ => Err(invalid(format!("unknown worker entry kind: {value}"))),
        }
    }

    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerUploadEntry {
    pub path: WorkerRelativePath,
    pub kind: WorkerEntryKind,
    pub mode: u32,
    pub size: u64,
    pub digest: Option<WorkerDigest>,
    pub symlink_target: Option<String>,
}

impl WorkerUploadEntry {
    pub fn new(path: impl Into<String>, kind: WorkerEntryKind, mode: u32, size: u64, digest: Option<WorkerDigest>) -> WorkerResult<Self> {
        Self::new_with_target(path, kind, mode, size, digest, None)
    }

    pub fn new_with_target(
        path: impl Into<String>, kind: WorkerEntryKind, mode: u32, size: u64, digest: Option<WorkerDigest>, symlink_target: Option<String>,
    ) -> WorkerResult<Self> {
        let entry = Self { path: WorkerRelativePath::for_entry(path)?, kind, mode, size, digest, symlink_target };
        entry.validate()?;
        Ok(entry)
    }

    pub fn directory(path: impl Into<String>, mode: u32) -> WorkerResult<Self> {
        Self::new(path, WorkerEntryKind::Directory, mode, 0, None)
    }

    pub fn file(path: impl Into<String>, mode: u32, size: u64, digest: WorkerDigest) -> WorkerResult<Self> {
        Self::new(path, WorkerEntryKind::File, mode, size, Some(digest))
    }

    pub fn symlink(path: impl Into<String>, mode: u32, target: impl Into<String>) -> WorkerResult<Self> {
        Self::new_with_target(path, WorkerEntryKind::Symlink, mode, 0, None, Some(target.into()))
    }

    pub fn path(&self) -> &WorkerRelativePath {
        &self.path
    }

    pub fn kind(&self) -> WorkerEntryKind {
        self.kind
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn digest(&self) -> Option<&WorkerDigest> {
        self.digest.as_ref()
    }

    pub fn symlink_target(&self) -> Option<&str> {
        self.symlink_target.as_deref()
    }

    pub fn validate(&self) -> WorkerResult<()> {
        validate_relative_path("worker entry path", self.path.as_str(), false)?;
        if self.mode & !0o7777 != 0 {
            return Err(invalid(format!("worker entry mode has unsupported bits: {:o}", self.mode)));
        }

        match self.kind {
            WorkerEntryKind::Directory => {
                if self.size != 0 {
                    return Err(invalid("worker directory entry must have zero size"));
                }
                if self.digest.is_some() {
                    return Err(invalid("worker directory entry must not have a digest"));
                }
                if self.symlink_target.is_some() {
                    return Err(invalid("worker directory entry must not have a symlink target"));
                }
            }
            WorkerEntryKind::File => {
                if self.size > MAX_WORKER_FILE_BYTES {
                    return Err(invalid(format!("worker file exceeds maximum size {MAX_WORKER_FILE_BYTES}")));
                }
                if self.digest.is_none() {
                    return Err(invalid("worker file entry is missing a digest"));
                }
                if self.symlink_target.is_some() {
                    return Err(invalid("worker file entry must not have a symlink target"));
                }
            }
            WorkerEntryKind::Symlink => {
                if self.size != 0 {
                    return Err(invalid("worker symlink entry must have zero size"));
                }
                if self.digest.is_some() {
                    return Err(invalid("worker symlink entry must not have a digest"));
                }
                let target = self.symlink_target.as_deref().ok_or_else(|| invalid("worker symlink entry is missing a target"))?;
                validate_symlink_target(self.path.as_str(), target)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerBuild {
    pub tool: WorkerTool,
    pub trusted_executable: WorkerExecutablePath,
    pub target_command: Option<WorkerTool>,
    pub argv: Vec<String>,
    pub cwd: WorkerRelativePath,
    pub guest_env: Vec<(String, String)>,
    pub target_env: Vec<(String, String)>,
    pub upload_token: WorkerUploadId,
    pub artifact_paths: Vec<WorkerArtifactPath>,
    pub artifact_max_file_bytes: u64,
    pub artifact_max_total_bytes: u64,
}

impl WorkerBuild {
    pub fn new(
        tool: impl Into<String>, trusted_executable: impl Into<String>, argv: Vec<String>, cwd: impl Into<String>, guest_env: Vec<(String, String)>,
        target_env: Vec<(String, String)>, upload_token: WorkerUploadId,
    ) -> WorkerResult<Self> {
        let build = Self {
            tool: WorkerTool::new(tool)?,
            trusted_executable: WorkerExecutablePath::new(trusted_executable)?,
            target_command: None,
            argv,
            cwd: WorkerRelativePath::new(cwd)?,
            guest_env,
            target_env,
            upload_token,
            artifact_paths: Vec::new(),
            artifact_max_file_bytes: MAX_WORKER_ARTIFACT_FILE_BYTES,
            artifact_max_total_bytes: MAX_WORKER_ARTIFACT_TOTAL_BYTES,
        };
        build.validate()?;
        Ok(build)
    }

    pub fn new_command(
        tool: impl Into<String>, command: impl Into<String>, argv: Vec<String>, cwd: impl Into<String>, guest_env: Vec<(String, String)>,
        target_env: Vec<(String, String)>, upload_token: WorkerUploadId,
    ) -> WorkerResult<Self> {
        let build = Self {
            tool: WorkerTool::new(tool)?,
            trusted_executable: WorkerExecutablePath::new("/usr/bin/bunkerbox-command")?,
            target_command: Some(WorkerTool::new(command)?),
            argv,
            cwd: WorkerRelativePath::new(cwd)?,
            guest_env,
            target_env,
            upload_token,
            artifact_paths: Vec::new(),
            artifact_max_file_bytes: MAX_WORKER_ARTIFACT_FILE_BYTES,
            artifact_max_total_bytes: MAX_WORKER_ARTIFACT_TOTAL_BYTES,
        };
        build.validate()?;
        Ok(build)
    }

    pub fn validate(&self) -> WorkerResult<()> {
        validate_tool(self.tool.as_str())?;
        validate_executable_path(self.trusted_executable.as_str())?;
        validate_relative_path("worker cwd", self.cwd.as_str(), true)?;
        validate_argv(&self.argv)?;
        validate_environment("worker guest environment", &self.guest_env)?;
        validate_environment("worker target environment", &self.target_env)?;
        validate_count(self.artifact_paths.len(), MAX_WORKER_ARTIFACT_ENTRIES, "worker artifact paths")?;
        for path in &self.artifact_paths {
            validate_relative_path("worker artifact path", path.as_str(), false)?;
        }
        validate_artifact_limits(self.artifact_max_file_bytes, self.artifact_max_total_bytes)?;
        let mut paths = BTreeSet::new();
        for path in &self.artifact_paths {
            if !paths.insert(path.as_str()) {
                return Err(invalid(format!("duplicate worker artifact path: {}", path.as_str())));
            }
        }
        Ok(())
    }

    pub fn tool(&self) -> &WorkerTool {
        &self.tool
    }

    pub fn trusted_executable(&self) -> &WorkerExecutablePath {
        &self.trusted_executable
    }

    pub fn trusted_executable_path(&self) -> &str {
        self.trusted_executable.as_str()
    }

    pub fn target_command(&self) -> Option<&WorkerTool> {
        self.target_command.as_ref()
    }

    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    pub fn cwd(&self) -> &WorkerRelativePath {
        &self.cwd
    }

    pub fn guest_env(&self) -> &[(String, String)] {
        &self.guest_env
    }

    pub fn target_env(&self) -> &[(String, String)] {
        &self.target_env
    }

    pub fn upload_token(&self) -> WorkerUploadId {
        self.upload_token
    }

    pub fn artifact_paths(&self) -> &[WorkerArtifactPath] {
        &self.artifact_paths
    }

    pub fn artifact_max_file_bytes(&self) -> u64 {
        self.artifact_max_file_bytes
    }

    pub fn artifact_max_total_bytes(&self) -> u64 {
        self.artifact_max_total_bytes
    }

    pub fn with_artifacts(mut self, paths: Vec<WorkerArtifactPath>, max_file_bytes: u64, max_total_bytes: u64) -> WorkerResult<Self> {
        self.artifact_paths = paths;
        self.artifact_max_file_bytes = max_file_bytes;
        self.artifact_max_total_bytes = max_total_bytes;
        self.validate()?;
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerMessage {
    Hello {
        request_id: WorkerRequestId,
        session_id: WorkerSessionId,
        version: u16,
        response: bool,
    },
    UploadBegin {
        request_id: WorkerRequestId,
        session_id: WorkerSessionId,
        upload_id: WorkerUploadId,
        entries: Vec<WorkerUploadEntry>,
    },
    UploadEntry {
        request_id: WorkerRequestId,
        session_id: WorkerSessionId,
        upload_id: WorkerUploadId,
        entry_index: u32,
        entry: WorkerUploadEntry,
    },
    UploadFileChunk {
        request_id: WorkerRequestId,
        session_id: WorkerSessionId,
        upload_id: WorkerUploadId,
        path: WorkerRelativePath,
        offset: u64,
        data: Vec<u8>,
    },
    UploadComplete {
        request_id: WorkerRequestId,
        session_id: WorkerSessionId,
        upload_id: WorkerUploadId,
    },
    Build {
        request_id: WorkerRequestId,
        session_id: WorkerSessionId,
        build: WorkerBuild,
    },
    Cleanup {
        request_id: WorkerRequestId,
        session_id: WorkerSessionId,
        upload_token: WorkerUploadId,
    },
    SyncProgress {
        request_id: WorkerRequestId,
        session_id: WorkerSessionId,
        upload_id: WorkerUploadId,
        completed_bytes: u64,
        total_bytes: Option<u64>,
    },
    Stdout {
        request_id: WorkerRequestId,
        session_id: WorkerSessionId,
        data: Vec<u8>,
    },
    Stderr {
        request_id: WorkerRequestId,
        session_id: WorkerSessionId,
        data: Vec<u8>,
    },
    Completed {
        request_id: WorkerRequestId,
        session_id: WorkerSessionId,
        operation: WorkerOperation,
        exit_code: i32,
    },
    Error {
        request_id: WorkerRequestId,
        session_id: WorkerSessionId,
        operation: WorkerOperation,
        kind: WorkerErrorKind,
        message: String,
    },
    ArtifactManifest {
        request_id: WorkerRequestId,
        session_id: WorkerSessionId,
        artifact_set_id: WorkerArtifactSetId,
        entries: Vec<WorkerArtifactEntry>,
        total_bytes: u64,
    },
    FetchArtifact {
        request_id: WorkerRequestId,
        session_id: WorkerSessionId,
        artifact_set_id: WorkerArtifactSetId,
        entry_index: u32,
    },
    ArtifactChunk {
        request_id: WorkerRequestId,
        session_id: WorkerSessionId,
        artifact_set_id: WorkerArtifactSetId,
        entry_index: u32,
        offset: u64,
        data: Vec<u8>,
    },
    ArtifactComplete {
        request_id: WorkerRequestId,
        session_id: WorkerSessionId,
        artifact_set_id: WorkerArtifactSetId,
        entry_index: u32,
    },
}

impl WorkerMessage {
    pub fn hello(request_id: WorkerRequestId, session_id: WorkerSessionId, response: bool) -> Self {
        Self::hello_for_version(request_id, session_id, response, WORKER_PROTOCOL_VERSION)
    }

    pub fn hello_for_version(request_id: WorkerRequestId, session_id: WorkerSessionId, response: bool, version: u16) -> Self {
        Self::Hello { request_id, session_id, version, response }
    }

    pub fn build(request_id: WorkerRequestId, session_id: WorkerSessionId, build: WorkerBuild) -> Self {
        Self::Build { request_id, session_id, build }
    }

    pub fn stdout(request_id: WorkerRequestId, session_id: WorkerSessionId, data: Vec<u8>) -> Self {
        Self::Stdout { request_id, session_id, data }
    }

    pub fn stderr(request_id: WorkerRequestId, session_id: WorkerSessionId, data: Vec<u8>) -> Self {
        Self::Stderr { request_id, session_id, data }
    }

    pub fn completed(request_id: WorkerRequestId, session_id: WorkerSessionId, operation: WorkerOperation, exit_code: i32) -> Self {
        Self::Completed { request_id, session_id, operation, exit_code }
    }

    pub fn error(
        request_id: WorkerRequestId, session_id: WorkerSessionId, operation: WorkerOperation, kind: WorkerErrorKind, message: impl Into<String>,
    ) -> Self {
        Self::Error { request_id, session_id, operation, kind, message: message.into() }
    }

    pub const fn kind(&self) -> WorkerFrameKind {
        match self {
            Self::Hello { .. } => WorkerFrameKind::Hello,
            Self::UploadBegin { .. } => WorkerFrameKind::UploadBegin,
            Self::UploadEntry { .. } => WorkerFrameKind::UploadEntry,
            Self::UploadFileChunk { .. } => WorkerFrameKind::UploadFileChunk,
            Self::UploadComplete { .. } => WorkerFrameKind::UploadComplete,
            Self::Build { .. } => WorkerFrameKind::Build,
            Self::Cleanup { .. } => WorkerFrameKind::Cleanup,
            Self::SyncProgress { .. } => WorkerFrameKind::SyncProgress,
            Self::Stdout { .. } => WorkerFrameKind::Stdout,
            Self::Stderr { .. } => WorkerFrameKind::Stderr,
            Self::Completed { .. } => WorkerFrameKind::Completed,
            Self::Error { .. } => WorkerFrameKind::Error,
            Self::ArtifactManifest { .. } => WorkerFrameKind::ArtifactManifest,
            Self::FetchArtifact { .. } => WorkerFrameKind::FetchArtifact,
            Self::ArtifactChunk { .. } => WorkerFrameKind::ArtifactChunk,
            Self::ArtifactComplete { .. } => WorkerFrameKind::ArtifactComplete,
        }
    }

    pub fn request_id(&self) -> WorkerRequestId {
        match self {
            Self::Hello { request_id, .. }
            | Self::UploadBegin { request_id, .. }
            | Self::UploadEntry { request_id, .. }
            | Self::UploadFileChunk { request_id, .. }
            | Self::UploadComplete { request_id, .. }
            | Self::Build { request_id, .. }
            | Self::Cleanup { request_id, .. }
            | Self::SyncProgress { request_id, .. }
            | Self::Stdout { request_id, .. }
            | Self::Stderr { request_id, .. }
            | Self::Completed { request_id, .. }
            | Self::Error { request_id, .. }
            | Self::ArtifactManifest { request_id, .. }
            | Self::FetchArtifact { request_id, .. }
            | Self::ArtifactChunk { request_id, .. }
            | Self::ArtifactComplete { request_id, .. } => *request_id,
        }
    }

    pub fn session_id(&self) -> WorkerSessionId {
        match self {
            Self::Hello { session_id, .. }
            | Self::UploadBegin { session_id, .. }
            | Self::UploadEntry { session_id, .. }
            | Self::UploadFileChunk { session_id, .. }
            | Self::UploadComplete { session_id, .. }
            | Self::Build { session_id, .. }
            | Self::Cleanup { session_id, .. }
            | Self::SyncProgress { session_id, .. }
            | Self::Stdout { session_id, .. }
            | Self::Stderr { session_id, .. }
            | Self::Completed { session_id, .. }
            | Self::Error { session_id, .. }
            | Self::ArtifactManifest { session_id, .. }
            | Self::FetchArtifact { session_id, .. }
            | Self::ArtifactChunk { session_id, .. }
            | Self::ArtifactComplete { session_id, .. } => *session_id,
        }
    }

    pub fn upload_id(&self) -> Option<WorkerUploadId> {
        match self {
            Self::UploadBegin { upload_id, .. }
            | Self::UploadEntry { upload_id, .. }
            | Self::UploadFileChunk { upload_id, .. }
            | Self::UploadComplete { upload_id, .. }
            | Self::SyncProgress { upload_id, .. } => Some(*upload_id),
            Self::Build { build, .. } => Some(build.upload_token),
            Self::Cleanup { upload_token, .. } => Some(*upload_token),
            Self::Hello { .. }
            | Self::Stdout { .. }
            | Self::Stderr { .. }
            | Self::Completed { .. }
            | Self::Error { .. }
            | Self::ArtifactManifest { .. }
            | Self::FetchArtifact { .. }
            | Self::ArtifactChunk { .. }
            | Self::ArtifactComplete { .. } => None,
        }
    }

    pub fn validate(&self) -> WorkerResult<()> {
        match self {
            Self::Hello { version, .. } => {
                if !is_supported_version(*version) {
                    return Err(invalid(format!("unsupported worker hello version: {version}")));
                }
            }
            Self::UploadBegin { entries, .. } => {
                validate_upload_manifest(entries)?;
            }
            Self::UploadEntry { entry_index, entry, .. } => {
                validate_entry_index(*entry_index)?;
                entry.validate()?;
            }
            Self::UploadFileChunk { path, offset, data, .. } => validate_chunk(path, *offset, data)?,
            Self::UploadComplete { .. } => {}
            Self::Build { build, .. } => build.validate()?,
            Self::Cleanup { .. } => {}
            Self::SyncProgress { completed_bytes, total_bytes, .. } => validate_progress(*completed_bytes, *total_bytes)?,
            Self::Stdout { data, .. } | Self::Stderr { data, .. } => {
                if data.len() > MAX_WORKER_OUTPUT_BYTES {
                    return Err(invalid(format!("worker output exceeds maximum length {MAX_WORKER_OUTPUT_BYTES}")));
                }
            }
            Self::Completed { operation, .. } => {
                if *operation == WorkerOperation::Protocol {
                    return Err(invalid("worker completion cannot use protocol operation"));
                }
            }
            Self::Error { message, .. } => validate_error_message(message)?,
            Self::ArtifactManifest { artifact_set_id, entries, total_bytes, .. } => {
                validate_nonzero_id(artifact_set_id.0, "worker artifact set")?;
                validate_artifact_manifest(entries, *total_bytes)?;
            }
            Self::FetchArtifact { artifact_set_id, entry_index, .. } => {
                validate_nonzero_id(artifact_set_id.0, "worker artifact set")?;
                validate_artifact_index(*entry_index)?;
            }
            Self::ArtifactChunk { artifact_set_id, entry_index, offset, data, .. } => {
                validate_nonzero_id(artifact_set_id.0, "worker artifact set")?;
                validate_artifact_index(*entry_index)?;
                validate_chunk_offset(*offset, data)?;
            }
            Self::ArtifactComplete { artifact_set_id, entry_index, .. } => {
                validate_nonzero_id(artifact_set_id.0, "worker artifact set")?;
                validate_artifact_index(*entry_index)?;
            }
        }
        Ok(())
    }

    fn requires_artifact_version(&self) -> bool {
        match self {
            Self::Build { build, .. } => !build.artifact_paths.is_empty(),
            Self::ArtifactManifest { .. } | Self::FetchArtifact { .. } | Self::ArtifactChunk { .. } | Self::ArtifactComplete { .. } => true,
            _ => false,
        }
    }

    fn requires_symlink_version(&self) -> bool {
        match self {
            Self::UploadBegin { entries, .. } => entries.iter().any(|entry| entry.kind() == WorkerEntryKind::Symlink),
            Self::UploadEntry { entry, .. } => entry.kind() == WorkerEntryKind::Symlink,
            _ => false,
        }
    }

    pub fn encode(&self) -> WorkerResult<Vec<u8>> {
        self.encode_version(WORKER_PROTOCOL_VERSION)
    }

    pub fn encode_version(&self, version: u16) -> WorkerResult<Vec<u8>> {
        validate_version(version)?;
        if version == WORKER_PROTOCOL_VERSION && self.requires_artifact_version() {
            return Err(invalid("worker artifact message requires artifact-capable protocol version"));
        }
        if version < WORKER_COMMAND_PROTOCOL_VERSION && matches!(self, Self::Build { build, .. } if build.target_command.is_some()) {
            return Err(invalid("worker command identity requires command-capable protocol version"));
        }
        if version < WORKER_SYMLINK_PROTOCOL_VERSION && self.requires_symlink_version() {
            return Err(invalid("worker symlink entries require symlink-capable protocol version"));
        }
        self.validate()?;
        let mut payload = WireWriter::new();
        encode_payload(self, &mut payload, version)?;
        let payload = payload.finish()?;

        let declared = u32::try_from(payload.len()).map_err(|_| invalid("worker payload length does not fit in u32"))?;
        let mut frame = Vec::with_capacity(WORKER_FRAME_HEADER_LEN + payload.len());
        frame.extend_from_slice(&WORKER_PROTOCOL_MAGIC);
        frame.extend_from_slice(&version.to_le_bytes());
        frame.push(self.kind().as_u8());
        frame.extend_from_slice(&declared.to_le_bytes());
        frame.extend_from_slice(&payload);
        Ok(frame)
    }

    pub fn decode(frame: &[u8]) -> WorkerResult<Self> {
        let (version, message) = Self::decode_versioned(frame)?;
        if version != WORKER_PROTOCOL_VERSION {
            return Err(invalid(format!("unsupported worker protocol version: {version}")));
        }
        Ok(message)
    }

    pub fn decode_versioned(frame: &[u8]) -> WorkerResult<(u16, Self)> {
        let (version, kind, payload) = split_frame_versioned(frame)?;
        let message = decode_payload(kind, payload, version)?;
        Ok((version, message))
    }

    #[cfg(feature = "async")]
    pub async fn read_async<R: AsyncRead + Unpin>(reader: &mut R) -> WorkerResult<Self> {
        let (version, message) = Self::read_async_versioned(reader).await?;
        require_default_version(version)?;
        Ok(message)
    }

    #[cfg(feature = "async")]
    pub async fn read_async_versioned<R: AsyncRead + Unpin>(reader: &mut R) -> WorkerResult<(u16, Self)> {
        let mut header = [0u8; WORKER_FRAME_HEADER_LEN];
        reader.read_exact(&mut header).await.map_err(|error| match error.kind() {
            io::ErrorKind::UnexpectedEof => invalid("truncated worker frame header"),
            _ => WorkerProtocolError::Io(error.to_string()),
        })?;
        let (version, kind, payload_len) = decode_header(&header)?;
        let mut payload = vec![0u8; payload_len];
        reader.read_exact(&mut payload).await.map_err(|error| match error.kind() {
            io::ErrorKind::UnexpectedEof => invalid("truncated worker frame payload"),
            _ => WorkerProtocolError::Io(error.to_string()),
        })?;
        Ok((version, decode_payload(kind, &payload, version)?))
    }

    pub fn read_blocking<R: Read>(reader: &mut R) -> WorkerResult<Self> {
        let (version, message) = Self::read_blocking_versioned(reader)?;
        require_default_version(version)?;
        Ok(message)
    }

    pub fn read_blocking_versioned<R: Read>(reader: &mut R) -> WorkerResult<(u16, Self)> {
        let mut header = [0u8; WORKER_FRAME_HEADER_LEN];
        reader.read_exact(&mut header).map_err(|error| match error.kind() {
            io::ErrorKind::UnexpectedEof => invalid("truncated worker frame header"),
            _ => WorkerProtocolError::Io(error.to_string()),
        })?;
        let (version, kind, payload_len) = decode_header(&header)?;
        let mut payload = vec![0u8; payload_len];
        reader.read_exact(&mut payload).map_err(|error| match error.kind() {
            io::ErrorKind::UnexpectedEof => invalid("truncated worker frame payload"),
            _ => WorkerProtocolError::Io(error.to_string()),
        })?;
        Ok((version, decode_payload(kind, &payload, version)?))
    }

    pub fn read_blocking_optional<R: Read>(reader: &mut R) -> WorkerResult<Option<Self>> {
        let message = Self::read_blocking_optional_versioned(reader)?;
        message
            .map(|(version, message)| {
                require_default_version(version)?;
                Ok(message)
            })
            .transpose()
    }

    pub fn read_blocking_optional_versioned<R: Read>(reader: &mut R) -> WorkerResult<Option<(u16, Self)>> {
        let mut first = [0u8; 1];
        match reader.read_exact(&mut first) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(WorkerProtocolError::Io(error.to_string())),
        }
        let mut header = [0u8; WORKER_FRAME_HEADER_LEN];
        header[0] = first[0];
        reader.read_exact(&mut header[1..]).map_err(|error| match error.kind() {
            io::ErrorKind::UnexpectedEof => invalid("truncated worker frame header"),
            _ => WorkerProtocolError::Io(error.to_string()),
        })?;
        let (version, kind, payload_len) = decode_header(&header)?;
        let mut payload = vec![0u8; payload_len];
        reader.read_exact(&mut payload).map_err(|error| match error.kind() {
            io::ErrorKind::UnexpectedEof => invalid("truncated worker frame payload"),
            _ => WorkerProtocolError::Io(error.to_string()),
        })?;
        decode_payload(kind, &payload, version).map(|message| Some((version, message)))
    }

    #[cfg(feature = "async")]
    pub async fn write_async<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> WorkerResult<()> {
        self.write_async_version(writer, WORKER_PROTOCOL_VERSION).await
    }

    #[cfg(feature = "async")]
    pub async fn write_async_version<W: AsyncWrite + Unpin>(&self, writer: &mut W, version: u16) -> WorkerResult<()> {
        let frame = self.encode_version(version)?;
        writer.write_all(&frame).await.map_err(WorkerProtocolError::from)?;
        writer.flush().await.map_err(WorkerProtocolError::from)
    }

    pub fn write_blocking<W: Write>(&self, writer: &mut W) -> WorkerResult<()> {
        self.write_blocking_version(writer, WORKER_PROTOCOL_VERSION)
    }

    pub fn write_blocking_version<W: Write>(&self, writer: &mut W, version: u16) -> WorkerResult<()> {
        let frame = self.encode_version(version)?;
        writer.write_all(&frame).map_err(WorkerProtocolError::from)?;
        writer.flush().map_err(WorkerProtocolError::from)
    }
}

#[cfg(feature = "async")]
pub async fn read_worker_message<R: AsyncRead + Unpin>(reader: &mut R) -> WorkerResult<WorkerMessage> {
    WorkerMessage::read_async(reader).await
}

#[cfg(feature = "async")]
pub async fn write_worker_message<W: AsyncWrite + Unpin>(writer: &mut W, message: &WorkerMessage) -> WorkerResult<()> {
    message.write_async(writer).await
}

#[cfg(feature = "async")]
pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> WorkerResult<WorkerMessage> {
    read_worker_message(reader).await
}

#[cfg(feature = "async")]
pub async fn read_message_versioned<R: AsyncRead + Unpin>(reader: &mut R) -> WorkerResult<(u16, WorkerMessage)> {
    WorkerMessage::read_async_versioned(reader).await
}

#[cfg(feature = "async")]
pub async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, message: &WorkerMessage) -> WorkerResult<()> {
    write_worker_message(writer, message).await
}

#[cfg(feature = "async")]
pub async fn write_message_versioned<W: AsyncWrite + Unpin>(writer: &mut W, message: &WorkerMessage, version: u16) -> WorkerResult<()> {
    message.write_async_version(writer, version).await
}

pub fn encode_worker_message(message: &WorkerMessage) -> WorkerResult<Vec<u8>> {
    message.encode()
}

pub fn decode_worker_message(frame: &[u8]) -> WorkerResult<WorkerMessage> {
    WorkerMessage::decode(frame)
}

pub fn validate_worker_relative_path(value: &str) -> WorkerResult<()> {
    validate_relative_path("worker relative path", value, false)
}

pub fn validate_worker_tool(value: &str) -> WorkerResult<()> {
    validate_tool(value)
}

pub fn validate_worker_executable_path(value: &str) -> WorkerResult<()> {
    validate_executable_path(value)
}

pub fn validate_upload_manifest(entries: &[WorkerUploadEntry]) -> WorkerResult<u64> {
    validate_count(entries.len(), MAX_WORKER_UPLOAD_ENTRIES, "worker upload entries")?;
    let mut previous: Option<&str> = None;
    let mut total_bytes = 0u64;
    let mut manifest_bytes = 4usize;
    for entry in entries {
        entry.validate()?;
        if let Some(previous) = previous {
            if previous >= entry.path.as_str() {
                return Err(invalid("worker upload manifest must be strictly sorted by path"));
            }
        }
        previous = Some(entry.path.as_str());
        let encoded_entry_bytes = 4usize
            .checked_add(entry.path.as_str().len())
            .and_then(|bytes| bytes.checked_add(1 + 4 + 8 + 1))
            .and_then(|bytes| bytes.checked_add(if entry.digest.is_some() { WORKER_DIGEST_LEN } else { 0 }))
            .and_then(|bytes| bytes.checked_add(1 + entry.symlink_target().map_or(0, |target| 4 + target.len())))
            .ok_or_else(|| invalid("worker manifest length overflow"))?;
        manifest_bytes = manifest_bytes.checked_add(encoded_entry_bytes).ok_or_else(|| invalid("worker manifest length overflow"))?;
        if manifest_bytes > MAX_WORKER_MANIFEST_BYTES {
            return Err(invalid(format!("worker manifest exceeds maximum length {MAX_WORKER_MANIFEST_BYTES}")));
        }
        total_bytes = total_bytes.checked_add(entry.size).ok_or_else(|| invalid("worker upload byte count overflow"))?;
        if total_bytes > MAX_WORKER_TOTAL_UPLOAD_BYTES {
            return Err(invalid(format!("worker upload exceeds maximum size {MAX_WORKER_TOTAL_UPLOAD_BYTES}")));
        }
    }
    validate_upload_symlinks(entries)?;
    Ok(total_bytes)
}

fn encode_payload(message: &WorkerMessage, writer: &mut WireWriter, version: u16) -> WorkerResult<()> {
    match message {
        WorkerMessage::Hello { request_id, session_id, version, response } => {
            encode_correlation(writer, *request_id, *session_id)?;
            writer.u16(*version)?;
            writer.boolean(*response)?;
        }
        WorkerMessage::UploadBegin { request_id, session_id, upload_id, entries } => {
            encode_correlation(writer, *request_id, *session_id)?;
            writer.id(upload_id.0)?;
            writer.count(entries.len(), MAX_WORKER_UPLOAD_ENTRIES, "worker upload entries")?;
            for entry in entries {
                encode_entry(writer, entry, version)?;
            }
        }
        WorkerMessage::UploadEntry { request_id, session_id, upload_id, entry_index, entry } => {
            encode_correlation(writer, *request_id, *session_id)?;
            writer.id(upload_id.0)?;
            writer.u32(*entry_index)?;
            encode_entry(writer, entry, version)?;
        }
        WorkerMessage::UploadFileChunk { request_id, session_id, upload_id, path, offset, data } => {
            encode_correlation(writer, *request_id, *session_id)?;
            writer.id(upload_id.0)?;
            writer.string(path.as_str(), MAX_WORKER_PATH_BYTES, "worker chunk path")?;
            writer.u64(*offset)?;
            writer.blob(data, MAX_WORKER_CHUNK_BYTES, "worker file chunk")?;
        }
        WorkerMessage::UploadComplete { request_id, session_id, upload_id } => {
            encode_correlation(writer, *request_id, *session_id)?;
            writer.id(upload_id.0)?;
        }
        WorkerMessage::Build { request_id, session_id, build } => {
            encode_correlation(writer, *request_id, *session_id)?;
            encode_build(writer, build, version)?;
        }
        WorkerMessage::Cleanup { request_id, session_id, upload_token } => {
            encode_correlation(writer, *request_id, *session_id)?;
            writer.id(upload_token.0)?;
        }
        WorkerMessage::SyncProgress { request_id, session_id, upload_id, completed_bytes, total_bytes } => {
            encode_correlation(writer, *request_id, *session_id)?;
            writer.id(upload_id.0)?;
            writer.u64(*completed_bytes)?;
            match total_bytes {
                Some(total_bytes) => {
                    writer.boolean(true)?;
                    writer.u64(*total_bytes)?;
                }
                None => writer.boolean(false)?,
            }
        }
        WorkerMessage::Stdout { request_id, session_id, data } | WorkerMessage::Stderr { request_id, session_id, data } => {
            encode_correlation(writer, *request_id, *session_id)?;
            writer.blob(data, MAX_WORKER_OUTPUT_BYTES, "worker output")?;
        }
        WorkerMessage::Completed { request_id, session_id, operation, exit_code } => {
            encode_correlation(writer, *request_id, *session_id)?;
            writer.u8(operation.as_u8())?;
            writer.i32(*exit_code)?;
        }
        WorkerMessage::Error { request_id, session_id, operation, kind, message } => {
            encode_correlation(writer, *request_id, *session_id)?;
            writer.u8(operation.as_u8())?;
            writer.u8(kind.as_u8())?;
            writer.string(message, MAX_WORKER_ERROR_BYTES, "worker error")?;
        }
        WorkerMessage::ArtifactManifest { request_id, session_id, artifact_set_id, entries, total_bytes } => {
            encode_correlation(writer, *request_id, *session_id)?;
            writer.id(artifact_set_id.0)?;
            writer.count(entries.len(), MAX_WORKER_ARTIFACT_ENTRIES, "worker artifact entries")?;
            for entry in entries {
                encode_artifact_entry(writer, entry)?;
            }
            writer.u64(*total_bytes)?;
        }
        WorkerMessage::FetchArtifact { request_id, session_id, artifact_set_id, entry_index } => {
            encode_correlation(writer, *request_id, *session_id)?;
            writer.id(artifact_set_id.0)?;
            writer.u32(*entry_index)?;
        }
        WorkerMessage::ArtifactChunk { request_id, session_id, artifact_set_id, entry_index, offset, data } => {
            encode_correlation(writer, *request_id, *session_id)?;
            writer.id(artifact_set_id.0)?;
            writer.u32(*entry_index)?;
            writer.u64(*offset)?;
            writer.blob(data, MAX_WORKER_CHUNK_BYTES, "worker artifact chunk")?;
        }
        WorkerMessage::ArtifactComplete { request_id, session_id, artifact_set_id, entry_index } => {
            encode_correlation(writer, *request_id, *session_id)?;
            writer.id(artifact_set_id.0)?;
            writer.u32(*entry_index)?;
        }
    }
    Ok(())
}

fn decode_payload(kind: WorkerFrameKind, payload: &[u8], version: u16) -> WorkerResult<WorkerMessage> {
    let mut reader = WireReader::new(payload);
    let message = match kind {
        WorkerFrameKind::Hello => {
            let (request_id, session_id) = decode_correlation(&mut reader)?;
            let version = reader.u16()?;
            let response = reader.boolean("worker hello response")?;
            WorkerMessage::Hello { request_id, session_id, version, response }
        }
        WorkerFrameKind::UploadBegin => {
            let (request_id, session_id) = decode_correlation(&mut reader)?;
            let upload_id = WorkerUploadId(reader.array16()?);
            let count = reader.count(MAX_WORKER_UPLOAD_ENTRIES, "worker upload entries")?;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                entries.push(decode_entry(&mut reader, version)?);
            }
            WorkerMessage::UploadBegin { request_id, session_id, upload_id, entries }
        }
        WorkerFrameKind::UploadEntry => {
            let (request_id, session_id) = decode_correlation(&mut reader)?;
            let upload_id = WorkerUploadId(reader.array16()?);
            let entry_index = reader.u32()?;
            let entry = decode_entry(&mut reader, version)?;
            WorkerMessage::UploadEntry { request_id, session_id, upload_id, entry_index, entry }
        }
        WorkerFrameKind::UploadFileChunk => {
            let (request_id, session_id) = decode_correlation(&mut reader)?;
            let upload_id = WorkerUploadId(reader.array16()?);
            let path = WorkerRelativePath::for_entry(reader.string(MAX_WORKER_PATH_BYTES, "worker chunk path")?)?;
            let offset = reader.u64()?;
            let data = reader.blob(MAX_WORKER_CHUNK_BYTES, "worker file chunk")?;
            WorkerMessage::UploadFileChunk { request_id, session_id, upload_id, path, offset, data }
        }
        WorkerFrameKind::UploadComplete => {
            let (request_id, session_id) = decode_correlation(&mut reader)?;
            let upload_id = WorkerUploadId(reader.array16()?);
            WorkerMessage::UploadComplete { request_id, session_id, upload_id }
        }
        WorkerFrameKind::Build => {
            let (request_id, session_id) = decode_correlation(&mut reader)?;
            WorkerMessage::Build { request_id, session_id, build: decode_build(&mut reader, version)? }
        }
        WorkerFrameKind::Cleanup => {
            let (request_id, session_id) = decode_correlation(&mut reader)?;
            let upload_token = WorkerUploadId(reader.array16()?);
            WorkerMessage::Cleanup { request_id, session_id, upload_token }
        }
        WorkerFrameKind::SyncProgress => {
            let (request_id, session_id) = decode_correlation(&mut reader)?;
            let upload_id = WorkerUploadId(reader.array16()?);
            let completed_bytes = reader.u64()?;
            let total_bytes = if reader.boolean("worker progress total flag")? { Some(reader.u64()?) } else { None };
            WorkerMessage::SyncProgress { request_id, session_id, upload_id, completed_bytes, total_bytes }
        }
        WorkerFrameKind::Stdout => {
            let (request_id, session_id) = decode_correlation(&mut reader)?;
            WorkerMessage::Stdout { request_id, session_id, data: reader.blob(MAX_WORKER_OUTPUT_BYTES, "worker stdout")? }
        }
        WorkerFrameKind::Stderr => {
            let (request_id, session_id) = decode_correlation(&mut reader)?;
            WorkerMessage::Stderr { request_id, session_id, data: reader.blob(MAX_WORKER_OUTPUT_BYTES, "worker stderr")? }
        }
        WorkerFrameKind::Completed => {
            let (request_id, session_id) = decode_correlation(&mut reader)?;
            let operation = WorkerOperation::from_u8(reader.u8()?)?;
            let exit_code = reader.i32()?;
            WorkerMessage::Completed { request_id, session_id, operation, exit_code }
        }
        WorkerFrameKind::Error => {
            let (request_id, session_id) = decode_correlation(&mut reader)?;
            let operation = WorkerOperation::from_u8(reader.u8()?)?;
            let kind = WorkerErrorKind::from_u8(reader.u8()?)?;
            let message = reader.string(MAX_WORKER_ERROR_BYTES, "worker error")?;
            WorkerMessage::Error { request_id, session_id, operation, kind, message }
        }
        WorkerFrameKind::ArtifactManifest => {
            let (request_id, session_id) = decode_correlation(&mut reader)?;
            let artifact_set_id = WorkerArtifactSetId(reader.array16()?);
            let count = reader.count(MAX_WORKER_ARTIFACT_ENTRIES, "worker artifact entries")?;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                entries.push(decode_artifact_entry(&mut reader)?);
            }
            let total_bytes = reader.u64()?;
            WorkerMessage::ArtifactManifest { request_id, session_id, artifact_set_id, entries, total_bytes }
        }
        WorkerFrameKind::FetchArtifact => {
            let (request_id, session_id) = decode_correlation(&mut reader)?;
            let artifact_set_id = WorkerArtifactSetId(reader.array16()?);
            let entry_index = reader.u32()?;
            WorkerMessage::FetchArtifact { request_id, session_id, artifact_set_id, entry_index }
        }
        WorkerFrameKind::ArtifactChunk => {
            let (request_id, session_id) = decode_correlation(&mut reader)?;
            let artifact_set_id = WorkerArtifactSetId(reader.array16()?);
            let entry_index = reader.u32()?;
            let offset = reader.u64()?;
            let data = reader.blob(MAX_WORKER_CHUNK_BYTES, "worker artifact chunk")?;
            WorkerMessage::ArtifactChunk { request_id, session_id, artifact_set_id, entry_index, offset, data }
        }
        WorkerFrameKind::ArtifactComplete => {
            let (request_id, session_id) = decode_correlation(&mut reader)?;
            let artifact_set_id = WorkerArtifactSetId(reader.array16()?);
            let entry_index = reader.u32()?;
            WorkerMessage::ArtifactComplete { request_id, session_id, artifact_set_id, entry_index }
        }
    };
    reader.finish()?;
    message.validate()?;
    if version == WORKER_PROTOCOL_VERSION && message.requires_artifact_version() {
        return Err(invalid("worker artifact message requires artifact-capable protocol version"));
    }
    if let WorkerMessage::Hello { version: hello_version, .. } = &message {
        if *hello_version != version {
            return Err(invalid("worker Hello version does not match frame version"));
        }
    }
    Ok(message)
}

fn encode_artifact_entry(writer: &mut WireWriter, entry: &WorkerArtifactEntry) -> WorkerResult<()> {
    entry.validate()?;
    writer.string(entry.path.as_str(), MAX_WORKER_ARTIFACT_PATH_BYTES, "worker artifact path")?;
    writer.u32(entry.mode)?;
    writer.u64(entry.size)?;
    writer.bytes(&entry.digest)?;
    Ok(())
}

fn decode_artifact_entry(reader: &mut WireReader<'_>) -> WorkerResult<WorkerArtifactEntry> {
    WorkerArtifactEntry::new(reader.string(MAX_WORKER_ARTIFACT_PATH_BYTES, "worker artifact path")?, reader.u32()?, reader.u64()?, reader.array32()?)
}

fn encode_build(writer: &mut WireWriter, build: &WorkerBuild, version: u16) -> WorkerResult<()> {
    build.validate()?;
    writer.string(build.tool.as_str(), MAX_WORKER_TOOL_BYTES, "worker tool")?;
    if version >= WORKER_COMMAND_PROTOCOL_VERSION {
        writer.string(build.target_command.as_ref().map_or(build.tool.as_str(), WorkerTool::as_str), MAX_WORKER_TOOL_BYTES, "worker command")?;
    } else {
        writer.string(build.trusted_executable.as_str(), MAX_WORKER_EXECUTABLE_PATH_BYTES, "worker executable path")?;
    }
    writer.count(build.argv.len(), MAX_WORKER_ARG_COUNT, "worker argv")?;
    for argument in &build.argv {
        writer.string(argument, MAX_WORKER_ARG_BYTES, "worker argument")?;
    }
    writer.string(build.cwd.as_str(), MAX_WORKER_CWD_BYTES, "worker cwd")?;
    encode_environment(writer, &build.guest_env, "worker guest environment")?;
    encode_environment(writer, &build.target_env, "worker target environment")?;
    writer.id(build.upload_token.0)?;
    if version >= WORKER_ARTIFACT_PROTOCOL_VERSION {
        writer.count(build.artifact_paths.len(), MAX_WORKER_ARTIFACT_ENTRIES, "worker artifact paths")?;
        for path in &build.artifact_paths {
            writer.string(path.as_str(), MAX_WORKER_ARTIFACT_PATH_BYTES, "worker artifact path")?;
        }
        writer.u64(build.artifact_max_file_bytes)?;
        writer.u64(build.artifact_max_total_bytes)?;
    }
    Ok(())
}

fn decode_build(reader: &mut WireReader<'_>, version: u16) -> WorkerResult<WorkerBuild> {
    let tool = WorkerTool::new(reader.string(MAX_WORKER_TOOL_BYTES, "worker tool")?)?;
    let (trusted_executable, target_command) = if version >= WORKER_COMMAND_PROTOCOL_VERSION {
        let command = WorkerTool::new(reader.string(MAX_WORKER_TOOL_BYTES, "worker command")?)?;
        (WorkerExecutablePath::new("/usr/bin/bunkerbox-command")?, Some(command))
    } else {
        (WorkerExecutablePath::new(reader.string(MAX_WORKER_EXECUTABLE_PATH_BYTES, "worker executable path")?)?, None)
    };
    let argument_count = reader.count(MAX_WORKER_ARG_COUNT, "worker argv")?;
    let mut argv = Vec::with_capacity(argument_count);
    for _ in 0..argument_count {
        argv.push(reader.string(MAX_WORKER_ARG_BYTES, "worker argument")?);
    }
    let cwd = WorkerRelativePath::new(reader.string(MAX_WORKER_CWD_BYTES, "worker cwd")?)?;
    let guest_env = decode_environment(reader, "worker guest environment")?;
    let target_env = decode_environment(reader, "worker target environment")?;
    let upload_token = WorkerUploadId(reader.array16()?);
    let (artifact_paths, artifact_max_file_bytes, artifact_max_total_bytes) = if version >= WORKER_ARTIFACT_PROTOCOL_VERSION {
        let count = reader.count(MAX_WORKER_ARTIFACT_ENTRIES, "worker artifact paths")?;
        let mut paths = Vec::with_capacity(count);
        for _ in 0..count {
            paths.push(WorkerArtifactPath::new(reader.string(MAX_WORKER_ARTIFACT_PATH_BYTES, "worker artifact path")?)?);
        }
        (paths, reader.u64()?, reader.u64()?)
    } else {
        (Vec::new(), MAX_WORKER_ARTIFACT_FILE_BYTES, MAX_WORKER_ARTIFACT_TOTAL_BYTES)
    };
    let build = WorkerBuild {
        tool,
        trusted_executable,
        target_command,
        argv,
        cwd,
        guest_env,
        target_env,
        upload_token,
        artifact_paths,
        artifact_max_file_bytes,
        artifact_max_total_bytes,
    };
    build.validate()?;
    Ok(build)
}

fn encode_environment(writer: &mut WireWriter, environment: &[(String, String)], field: &str) -> WorkerResult<()> {
    validate_environment(field, environment)?;
    writer.count(environment.len(), MAX_WORKER_ENV_COUNT, field)?;
    for (key, value) in environment {
        writer.string(key, MAX_WORKER_ENV_KEY_BYTES, "worker environment key")?;
        writer.string(value, MAX_WORKER_ENV_VALUE_BYTES, "worker environment value")?;
    }
    Ok(())
}

fn decode_environment(reader: &mut WireReader<'_>, field: &str) -> WorkerResult<Vec<(String, String)>> {
    let count = reader.count(MAX_WORKER_ENV_COUNT, field)?;
    let mut environment = Vec::with_capacity(count);
    for _ in 0..count {
        let key = reader.string(MAX_WORKER_ENV_KEY_BYTES, "worker environment key")?;
        let value = reader.string(MAX_WORKER_ENV_VALUE_BYTES, "worker environment value")?;
        environment.push((key, value));
    }
    validate_environment(field, &environment)?;
    Ok(environment)
}

fn encode_entry(writer: &mut WireWriter, entry: &WorkerUploadEntry, version: u16) -> WorkerResult<()> {
    entry.validate()?;
    writer.string(entry.path.as_str(), MAX_WORKER_PATH_BYTES, "worker entry path")?;
    writer.u8(entry.kind.as_u8())?;
    writer.u32(entry.mode)?;
    writer.u64(entry.size)?;
    match entry.digest {
        Some(digest) => {
            writer.boolean(true)?;
            writer.bytes(&digest)?;
        }
        None => writer.boolean(false)?,
    }
    if version >= WORKER_SYMLINK_PROTOCOL_VERSION {
        match entry.symlink_target() {
            Some(target) => {
                writer.boolean(true)?;
                writer.string(target, MAX_WORKER_PATH_BYTES, "worker symlink target")?;
            }
            None => writer.boolean(false)?,
        }
    }
    Ok(())
}

fn decode_entry(reader: &mut WireReader<'_>, version: u16) -> WorkerResult<WorkerUploadEntry> {
    let path = reader.string(MAX_WORKER_PATH_BYTES, "worker entry path")?;
    let kind = WorkerEntryKind::from_u8(reader.u8()?)?;
    let mode = reader.u32()?;
    let size = reader.u64()?;
    let digest = match reader.boolean("worker entry digest flag")? {
        true => Some(reader.array32()?),
        false => None,
    };
    let symlink_target = if version >= WORKER_SYMLINK_PROTOCOL_VERSION {
        match reader.boolean("worker symlink target flag")? {
            true => Some(reader.string(MAX_WORKER_PATH_BYTES, "worker symlink target")?),
            false => None,
        }
    } else {
        None
    };
    WorkerUploadEntry::new_with_target(path, kind, mode, size, digest, symlink_target)
}

fn encode_correlation(writer: &mut WireWriter, request_id: WorkerRequestId, session_id: WorkerSessionId) -> WorkerResult<()> {
    writer.id(request_id.0)?;
    writer.id(session_id.0)
}

fn decode_correlation(reader: &mut WireReader<'_>) -> WorkerResult<(WorkerRequestId, WorkerSessionId)> {
    Ok((WorkerRequestId(reader.array16()?), WorkerSessionId(reader.array16()?)))
}

fn validate_entry_index(index: u32) -> WorkerResult<()> {
    if usize::try_from(index).map_or(true, |index| index >= MAX_WORKER_UPLOAD_ENTRIES) {
        return Err(invalid(format!("worker upload entry index exceeds maximum {MAX_WORKER_UPLOAD_ENTRIES}")));
    }
    Ok(())
}

fn validate_chunk(path: &WorkerRelativePath, offset: u64, data: &[u8]) -> WorkerResult<()> {
    validate_relative_path("worker chunk path", path.as_str(), false)?;
    if data.is_empty() {
        return Err(invalid("worker file chunk must not be empty"));
    }
    if data.len() > MAX_WORKER_CHUNK_BYTES {
        return Err(invalid(format!("worker file chunk exceeds maximum length {MAX_WORKER_CHUNK_BYTES}")));
    }
    let end = offset
        .checked_add(u64::try_from(data.len()).map_err(|_| invalid("worker file chunk length does not fit in u64"))?)
        .ok_or_else(|| invalid("worker file chunk offset overflow"))?;
    if end > MAX_WORKER_FILE_BYTES {
        return Err(invalid(format!("worker file chunk exceeds maximum file size {MAX_WORKER_FILE_BYTES}")));
    }
    Ok(())
}

fn validate_progress(completed_bytes: u64, total_bytes: Option<u64>) -> WorkerResult<()> {
    if completed_bytes > MAX_WORKER_TOTAL_UPLOAD_BYTES {
        return Err(invalid("worker progress exceeds the maximum upload size"));
    }
    if let Some(total_bytes) = total_bytes {
        if total_bytes > MAX_WORKER_TOTAL_UPLOAD_BYTES {
            return Err(invalid("worker progress total exceeds the maximum upload size"));
        }
        if completed_bytes > total_bytes {
            return Err(invalid("worker progress exceeds its total"));
        }
    }
    Ok(())
}

fn validate_argv(argv: &[String]) -> WorkerResult<()> {
    validate_count(argv.len(), MAX_WORKER_ARG_COUNT, "worker argv")?;
    let mut total_bytes = 0usize;
    for argument in argv {
        validate_text("worker argument", argument, MAX_WORKER_ARG_BYTES)?;
        total_bytes = total_bytes.checked_add(argument.len()).ok_or_else(|| invalid("worker argv length overflow"))?;
        if total_bytes > MAX_WORKER_ARG_TOTAL_BYTES {
            return Err(invalid(format!("worker argv exceeds maximum length {MAX_WORKER_ARG_TOTAL_BYTES}")));
        }
    }
    Ok(())
}

fn validate_environment(field: &str, environment: &[(String, String)]) -> WorkerResult<()> {
    validate_count(environment.len(), MAX_WORKER_ENV_COUNT, field)?;
    let mut names = BTreeSet::new();
    let mut total_bytes = 0usize;
    for (key, value) in environment {
        validate_environment_key(key)?;
        validate_environment_value(value)?;
        total_bytes = total_bytes
            .checked_add(key.len())
            .and_then(|bytes| bytes.checked_add(value.len()))
            .ok_or_else(|| invalid(format!("{field} length overflow")))?;
        if total_bytes > MAX_WORKER_ENV_TOTAL_BYTES {
            return Err(invalid(format!("{field} exceeds maximum length {MAX_WORKER_ENV_TOTAL_BYTES}")));
        }
        if !names.insert(key.as_str()) {
            return Err(invalid(format!("duplicate worker environment key: {key}")));
        }
    }
    Ok(())
}

fn validate_environment_key(key: &str) -> WorkerResult<()> {
    validate_text("worker environment key", key, MAX_WORKER_ENV_KEY_BYTES)?;
    let mut bytes = key.bytes();
    let Some(first) = bytes.next() else {
        return Err(invalid("worker environment key is empty"));
    };
    if !(first == b'_' || first.is_ascii_alphabetic()) || !bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric()) {
        return Err(invalid("worker environment key is not a valid variable name"));
    }
    Ok(())
}

fn validate_environment_value(value: &str) -> WorkerResult<()> {
    validate_text("worker environment value", value, MAX_WORKER_ENV_VALUE_BYTES)?;
    if value.chars().any(char::is_control) {
        return Err(invalid("worker environment value contains control data"));
    }
    Ok(())
}

fn validate_error_message(message: &str) -> WorkerResult<()> {
    validate_text("worker error", message, MAX_WORKER_ERROR_BYTES)
}

fn validate_tool(value: &str) -> WorkerResult<()> {
    validate_text("worker tool", value, MAX_WORKER_TOOL_BYTES)?;
    if value.is_empty()
        || value == "."
        || value == ".."
        || !value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'+'))
    {
        return Err(invalid("worker tool must be a single safe executable identity"));
    }
    Ok(())
}

fn validate_executable_path(value: &str) -> WorkerResult<()> {
    validate_text("worker executable path", value, MAX_WORKER_EXECUTABLE_PATH_BYTES)?;
    if !value.starts_with('/') || value == "/" || value.starts_with("//") {
        return Err(invalid("worker executable path must be a normalized absolute path"));
    }
    if value.chars().any(|character| character.is_whitespace() || character.is_control()) {
        return Err(invalid("worker executable path must not contain whitespace or control data"));
    }
    if value.bytes().any(|byte| {
        byte < 0x20
            || byte == 0x7f
            || matches!(byte, b'\\' | b';' | b'|' | b'&' | b'$' | b'`' | b'<' | b'>' | b'\'' | b'"' | b'(' | b')' | b'[' | b']' | b'{' | b'}')
    }) {
        return Err(invalid("worker executable path contains unsafe identity data"));
    }
    for (depth, component) in value.split('/').skip(1).enumerate() {
        if depth >= MAX_WORKER_PATH_DEPTH
            || component.len() > MAX_WORKER_PATH_COMPONENT_BYTES
            || component.is_empty()
            || component == "."
            || component == ".."
            || component.contains(':')
        {
            return Err(invalid("worker executable path is not normalized"));
        }
    }
    Ok(())
}

fn validate_relative_path(field: &str, value: &str, allow_empty: bool) -> WorkerResult<()> {
    validate_text(field, value, MAX_WORKER_PATH_BYTES)?;
    if value.is_empty() {
        if allow_empty {
            return Ok(());
        }
        return Err(invalid(format!("{field} must not be empty")));
    }
    if value.starts_with('/') || value.starts_with('\\') || value.contains('\\') || value.bytes().any(|byte| byte == b':') {
        return Err(invalid(format!("{field} must be a normalized relative path")));
    }
    for (depth, component) in value.split('/').enumerate() {
        if depth >= MAX_WORKER_PATH_DEPTH
            || component.len() > MAX_WORKER_PATH_COMPONENT_BYTES
            || component.is_empty()
            || component == "."
            || component == ".."
            || component.chars().any(char::is_control)
        {
            return Err(invalid(format!("{field} must be a normalized relative path")));
        }
    }
    Ok(())
}

fn validate_symlink_target(path: &str, target: &str) -> WorkerResult<()> {
    normalize_symlink_target(path, target).map(|_| ())
}

fn normalize_symlink_target(path: &str, target: &str) -> WorkerResult<String> {
    validate_text("worker symlink target", target, MAX_WORKER_PATH_BYTES)?;
    if target.is_empty() || target.starts_with('/') || target.starts_with('\\') || target.contains('\\') {
        return Err(invalid("worker symlink target must be non-empty and relative"));
    }
    let mut components = path.rsplit_once('/').map_or_else(Vec::new, |(parent, _)| parent.split('/').collect::<Vec<_>>());
    let target_components = target.split('/').collect::<Vec<_>>();
    if target_components.len() > MAX_WORKER_PATH_DEPTH {
        return Err(invalid("worker symlink target exceeds maximum depth"));
    }
    for (index, component) in target_components.iter().enumerate() {
        if component.is_empty() {
            if index + 1 == target_components.len() {
                continue;
            }
            return Err(invalid("worker symlink target contains an empty component"));
        }
        match *component {
            "." => {}
            ".." => {
                components.pop().ok_or_else(|| invalid("worker symlink target escapes workspace"))?;
            }
            value => {
                if value.len() > MAX_WORKER_PATH_COMPONENT_BYTES || value.chars().any(char::is_control) || value.contains(':') {
                    return Err(invalid("worker symlink target contains an invalid component"));
                }
                components.push(value);
            }
        }
    }
    Ok(components.join("/"))
}

fn validate_upload_symlinks(entries: &[WorkerUploadEntry]) -> WorkerResult<()> {
    let by_path = entries.iter().map(|entry| (entry.path().as_str(), entry)).collect::<BTreeMap<_, _>>();
    for entry in entries.iter().filter(|entry| entry.kind() == WorkerEntryKind::Symlink) {
        let target = entry.symlink_target().ok_or_else(|| invalid("worker symlink entry is missing a target"))?;
        let mut current = normalize_symlink_target(entry.path().as_str(), target)?;
        let mut visited = BTreeSet::new();
        loop {
            if current.is_empty() {
                break;
            }
            let target_entry = by_path.get(current.as_str()).ok_or_else(|| invalid("worker symlink target is missing from upload manifest"))?;
            if target_entry.kind() != WorkerEntryKind::Symlink {
                break;
            }
            if !visited.insert(current.clone()) {
                return Err(invalid("worker symlink loop detected"));
            }
            let nested_target = target_entry.symlink_target().ok_or_else(|| invalid("worker symlink entry is missing a target"))?;
            current = normalize_symlink_target(target_entry.path().as_str(), nested_target)?;
        }
    }
    Ok(())
}

fn validate_text(field: &str, value: &str, maximum: usize) -> WorkerResult<()> {
    if value.len() > maximum {
        return Err(invalid(format!("{field} exceeds maximum length {maximum}")));
    }
    if value.as_bytes().contains(&0) {
        return Err(invalid(format!("{field} contains a NUL byte")));
    }
    Ok(())
}

fn validate_count(count: usize, maximum: usize, field: &str) -> WorkerResult<()> {
    if count > maximum {
        return Err(invalid(format!("{field} exceeds maximum count {maximum}")));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> WorkerProtocolError {
    WorkerProtocolError::Invalid(message.into())
}

fn is_supported_version(version: u16) -> bool {
    matches!(version, WORKER_PROTOCOL_VERSION | WORKER_ARTIFACT_PROTOCOL_VERSION | WORKER_COMMAND_PROTOCOL_VERSION | WORKER_SYMLINK_PROTOCOL_VERSION)
}

fn validate_version(version: u16) -> WorkerResult<()> {
    if is_supported_version(version) {
        Ok(())
    } else {
        Err(invalid(format!("unsupported worker protocol version: {version}")))
    }
}

fn require_default_version(version: u16) -> WorkerResult<()> {
    if version == WORKER_PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(invalid(format!("unsupported worker protocol version: {version}")))
    }
}

fn validate_nonzero_id(bytes: [u8; WORKER_ID_LEN], label: &str) -> WorkerResult<()> {
    if bytes == [0; WORKER_ID_LEN] {
        return Err(invalid(format!("{label} ID must be nonzero")));
    }
    Ok(())
}

fn validate_artifact_limits(max_file_bytes: u64, max_total_bytes: u64) -> WorkerResult<()> {
    if max_file_bytes == 0 || max_file_bytes > MAX_WORKER_ARTIFACT_FILE_BYTES {
        return Err(invalid("worker artifact per-file limit is invalid"));
    }
    if max_total_bytes == 0 || max_total_bytes > MAX_WORKER_ARTIFACT_TOTAL_BYTES {
        return Err(invalid("worker artifact total limit is invalid"));
    }
    Ok(())
}

fn validate_artifact_index(index: u32) -> WorkerResult<()> {
    if usize::try_from(index).map_or(true, |index| index >= MAX_WORKER_ARTIFACT_ENTRIES) {
        return Err(invalid(format!("worker artifact index exceeds maximum {MAX_WORKER_ARTIFACT_ENTRIES}")));
    }
    Ok(())
}

fn validate_chunk_offset(offset: u64, data: &[u8]) -> WorkerResult<()> {
    if data.is_empty() {
        return Err(invalid("worker artifact chunk must not be empty"));
    }
    if data.len() > MAX_WORKER_CHUNK_BYTES {
        return Err(invalid(format!("worker artifact chunk exceeds maximum length {MAX_WORKER_CHUNK_BYTES}")));
    }
    let end = offset.checked_add(data.len() as u64).ok_or_else(|| invalid("worker artifact chunk offset overflow"))?;
    if end > MAX_WORKER_ARTIFACT_FILE_BYTES {
        return Err(invalid("worker artifact chunk exceeds maximum file size"));
    }
    Ok(())
}

fn validate_artifact_manifest(entries: &[WorkerArtifactEntry], total_bytes: u64) -> WorkerResult<()> {
    validate_count(entries.len(), MAX_WORKER_ARTIFACT_ENTRIES, "worker artifact entries")?;
    let mut paths = BTreeSet::new();
    let mut total = 0u64;
    let mut manifest_bytes = 4usize;
    for entry in entries {
        entry.validate()?;
        if !paths.insert(entry.path.as_str()) {
            return Err(invalid(format!("duplicate worker artifact path: {}", entry.path.as_str())));
        }
        total = total.checked_add(entry.size).ok_or_else(|| invalid("worker artifact total size overflow"))?;
        if total > MAX_WORKER_ARTIFACT_TOTAL_BYTES || total > total_bytes {
            return Err(invalid("worker artifact total exceeds its limit"));
        }
        manifest_bytes = manifest_bytes
            .checked_add(4 + entry.path.as_str().len() + 4 + 8 + WORKER_DIGEST_LEN)
            .ok_or_else(|| invalid("worker artifact manifest length overflow"))?;
        if manifest_bytes > MAX_WORKER_MANIFEST_BYTES {
            return Err(invalid("worker artifact manifest exceeds maximum length"));
        }
    }
    if total != total_bytes {
        return Err(invalid("worker artifact manifest total does not match entries"));
    }
    Ok(())
}

fn split_frame_versioned(frame: &[u8]) -> WorkerResult<(u16, WorkerFrameKind, &[u8])> {
    if frame.len() < WORKER_FRAME_HEADER_LEN {
        return Err(invalid("truncated worker frame header"));
    }
    let (version, kind, payload_len) = decode_header(&frame[..WORKER_FRAME_HEADER_LEN])?;
    let expected = WORKER_FRAME_HEADER_LEN.checked_add(payload_len).ok_or_else(|| invalid("worker frame length overflow"))?;
    if frame.len() < expected {
        return Err(invalid("truncated worker frame payload"));
    }
    if frame.len() > expected {
        return Err(invalid("extra bytes after worker frame"));
    }
    Ok((version, kind, &frame[WORKER_FRAME_HEADER_LEN..expected]))
}

fn decode_header(header: &[u8]) -> WorkerResult<(u16, WorkerFrameKind, usize)> {
    if header.len() != WORKER_FRAME_HEADER_LEN {
        return Err(invalid("invalid worker frame header length"));
    }
    if header[..4] != WORKER_PROTOCOL_MAGIC {
        return Err(invalid("invalid worker frame magic"));
    }
    let version = u16::from_le_bytes([header[4], header[5]]);
    validate_version(version)?;
    let kind = WorkerFrameKind::try_from(header[6])?;
    let payload_len = u32::from_le_bytes([header[7], header[8], header[9], header[10]]) as usize;
    if payload_len > MAX_WORKER_FRAME_PAYLOAD {
        return Err(invalid(format!("worker payload exceeds maximum length {MAX_WORKER_FRAME_PAYLOAD}")));
    }
    Ok((version, kind, payload_len))
}

struct WireWriter {
    bytes: Vec<u8>,
}

impl WireWriter {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn finish(self) -> WorkerResult<Vec<u8>> {
        if self.bytes.len() > MAX_WORKER_FRAME_PAYLOAD {
            return Err(invalid(format!("worker payload exceeds maximum length {MAX_WORKER_FRAME_PAYLOAD}")));
        }
        Ok(self.bytes)
    }

    fn bytes(&mut self, value: &[u8]) -> WorkerResult<()> {
        let new_length = self.bytes.len().checked_add(value.len()).ok_or_else(|| invalid("worker payload length overflow"))?;
        if new_length > MAX_WORKER_FRAME_PAYLOAD {
            return Err(invalid(format!("worker payload exceeds maximum length {MAX_WORKER_FRAME_PAYLOAD}")));
        }
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn id(&mut self, value: [u8; WORKER_ID_LEN]) -> WorkerResult<()> {
        self.bytes(&value)
    }

    fn u8(&mut self, value: u8) -> WorkerResult<()> {
        self.bytes(&[value])
    }

    fn boolean(&mut self, value: bool) -> WorkerResult<()> {
        self.u8(u8::from(value))
    }

    fn u16(&mut self, value: u16) -> WorkerResult<()> {
        self.bytes(&value.to_le_bytes())
    }

    fn u32(&mut self, value: u32) -> WorkerResult<()> {
        self.bytes(&value.to_le_bytes())
    }

    fn u64(&mut self, value: u64) -> WorkerResult<()> {
        self.bytes(&value.to_le_bytes())
    }

    fn i32(&mut self, value: i32) -> WorkerResult<()> {
        self.bytes(&value.to_le_bytes())
    }

    fn count(&mut self, count: usize, maximum: usize, field: &str) -> WorkerResult<()> {
        validate_count(count, maximum, field)?;
        self.u32(u32::try_from(count).map_err(|_| invalid(format!("{field} count does not fit in u32")))?)
    }

    fn string(&mut self, value: &str, maximum: usize, field: &str) -> WorkerResult<()> {
        validate_text(field, value, maximum)?;
        let length = u32::try_from(value.len()).map_err(|_| invalid(format!("{field} length does not fit in u32")))?;
        self.u32(length)?;
        self.bytes(value.as_bytes())
    }

    fn blob(&mut self, value: &[u8], maximum: usize, field: &str) -> WorkerResult<()> {
        if value.len() > maximum {
            return Err(invalid(format!("{field} exceeds maximum length {maximum}")));
        }
        let length = u32::try_from(value.len()).map_err(|_| invalid(format!("{field} length does not fit in u32")))?;
        self.u32(length)?;
        self.bytes(value)
    }
}

struct WireReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> WireReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, length: usize) -> WorkerResult<&'a [u8]> {
        let end = self.offset.checked_add(length).ok_or_else(|| invalid("worker payload length overflow"))?;
        if end > self.bytes.len() {
            return Err(invalid("truncated worker payload"));
        }
        let value = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> WorkerResult<u8> {
        Ok(self.take(1)?[0])
    }

    fn boolean(&mut self, field: &str) -> WorkerResult<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(invalid(format!("invalid {field} flag: {value}"))),
        }
    }

    fn u16(&mut self) -> WorkerResult<u16> {
        let value = self.take(2)?;
        Ok(u16::from_le_bytes([value[0], value[1]]))
    }

    fn u32(&mut self) -> WorkerResult<u32> {
        let value = self.take(4)?;
        Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
    }

    fn u64(&mut self) -> WorkerResult<u64> {
        let value = self.take(8)?;
        Ok(u64::from_le_bytes(value.try_into().map_err(|_| invalid("invalid worker integer"))?))
    }

    fn i32(&mut self) -> WorkerResult<i32> {
        let value = self.take(4)?;
        Ok(i32::from_le_bytes(value.try_into().map_err(|_| invalid("invalid worker integer"))?))
    }

    fn array16(&mut self) -> WorkerResult<[u8; WORKER_ID_LEN]> {
        self.take(WORKER_ID_LEN)?.try_into().map_err(|_| invalid("invalid worker identifier"))
    }

    fn array32(&mut self) -> WorkerResult<[u8; WORKER_DIGEST_LEN]> {
        self.take(WORKER_DIGEST_LEN)?.try_into().map_err(|_| invalid("invalid worker digest"))
    }

    fn count(&mut self, maximum: usize, field: &str) -> WorkerResult<usize> {
        let count = self.u32()? as usize;
        validate_count(count, maximum, field)?;
        Ok(count)
    }

    fn string(&mut self, maximum: usize, field: &str) -> WorkerResult<String> {
        let length = self.u32()? as usize;
        if length > maximum {
            return Err(invalid(format!("{field} exceeds maximum length {maximum}")));
        }
        let value = std::str::from_utf8(self.take(length)?).map_err(|_| invalid(format!("{field} is not valid UTF-8")))?;
        validate_text(field, value, maximum)?;
        Ok(value.to_owned())
    }

    fn blob(&mut self, maximum: usize, field: &str) -> WorkerResult<Vec<u8>> {
        let length = self.u32()? as usize;
        if length > maximum {
            return Err(invalid(format!("{field} exceeds maximum length {maximum}")));
        }
        Ok(self.take(length)?.to_vec())
    }

    fn finish(self) -> WorkerResult<()> {
        if self.offset != self.bytes.len() {
            return Err(invalid("extra bytes in worker payload"));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "lib_ut.rs"]
mod worker_protocol_tests;
