// Dead-code warnings are expected here: vscomm is shared between
// two binaries (bunkerbox and bunkerbox-vscomm) that use different items.
#![allow(dead_code)]
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::Path;

use crate::remote as remote_domain;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub mod buildsys;
pub const TOOLCHAIN_PORT: u32 = 9999;
// Keep UI traffic on a separate vsock endpoint from command execution.
pub const TUI_STATUS_PORT: u32 = 10000;
pub const VSCOMM_BIN_DIR: &str = "/usr/local/bunkerbox/bin";
/// Maximum payload accepted in one vsock frame.
pub const MAX_FRAME_PAYLOAD: usize = 1024 * 1024;
pub const REMOTE_PROTOCOL_VERSION: u16 = 1;
pub const MAX_REMOTE_STRING_BYTES: usize = 4 * 1024;
pub const MAX_REMOTE_TOOL_BYTES: usize = 256;
pub const MAX_REMOTE_ARG_COUNT: usize = 256;
pub const MAX_REMOTE_ARG_BYTES: usize = 4 * 1024;
pub const MAX_REMOTE_ENV_COUNT: usize = 64;
pub const MAX_REMOTE_ENV_KEY_BYTES: usize = 256;
pub const MAX_REMOTE_ENV_VALUE_BYTES: usize = 4 * 1024;
pub const MAX_REMOTE_ERROR_BYTES: usize = 4 * 1024;

#[repr(u16)]
#[derive(Clone, Copy)]
pub enum FrameType {
    ExecReq = 1,
    Stdout = 2,
    Stderr = 3,
    Exit = 4,
    Disconnect = 5,
    UiCommand = 10,
    RemoteRequest = 20,
    RemoteEvent = 21,
}

impl FrameType {
    pub fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::ExecReq),
            2 => Some(Self::Stdout),
            3 => Some(Self::Stderr),
            4 => Some(Self::Exit),
            5 => Some(Self::Disconnect),
            10 => Some(Self::UiCommand),
            20 => Some(Self::RemoteRequest),
            21 => Some(Self::RemoteEvent),
            _ => None,
        }
    }
}

pub struct ExecRequest {
    pub cwd: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestId(pub [u8; 16]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceSessionId(pub [u8; 16]);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRelativePath(String);

impl WorkspaceRelativePath {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_remote_string("remote cwd", &value, MAX_REMOTE_STRING_BYTES)?;
        if value.is_empty() {
            return Ok(Self(value));
        }

        let path = Path::new(&value);
        if path.is_absolute() || value.split('/').any(|component| component.is_empty() || component == "." || component == "..") {
            return Err("remote cwd must be a normalized relative path".to_string());
        }

        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTool(String);

impl RemoteTool {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_remote_string("remote tool", &value, MAX_REMOTE_TOOL_BYTES)?;
        if value.is_empty() {
            return Err("remote tool is empty".to_string());
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBuild {
    pub cwd: WorkspaceRelativePath,
    pub tool: RemoteTool,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl RemoteBuild {
    pub fn new(cwd: WorkspaceRelativePath, tool: RemoteTool, argv: Vec<String>, env: Vec<(String, String)>) -> Result<Self, String> {
        validate_remote_build_fields(&cwd, &tool, &argv, &env)?;
        Ok(Self { cwd, tool, argv, env })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteOperation {
    Sync(RemoteSync),
    Build(RemoteBuild),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteSync;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRequest {
    pub request_id: RequestId,
    pub workspace_session_id: WorkspaceSessionId,
    pub operation: RemoteOperation,
}

impl RemoteRequest {
    pub fn sync(request_id: RequestId, workspace_session_id: WorkspaceSessionId) -> Self {
        Self { request_id, workspace_session_id, operation: RemoteOperation::Sync(RemoteSync) }
    }

    pub fn build(request_id: RequestId, workspace_session_id: WorkspaceSessionId, build: RemoteBuild) -> Self {
        Self { request_id, workspace_session_id, operation: RemoteOperation::Build(build) }
    }

    pub fn to_frame(&self) -> Result<Frame, String> {
        let mut writer = WireWriter::new(*b"BBR1");
        writer.u16(REMOTE_PROTOCOL_VERSION);
        writer.u8(match &self.operation {
            RemoteOperation::Sync(_) => 1,
            RemoteOperation::Build(_) => 2,
        });
        writer.u8(0);
        writer.bytes(&self.request_id.0);
        writer.bytes(&self.workspace_session_id.0);

        if let RemoteOperation::Build(build) = &self.operation {
            encode_remote_build(&mut writer, build)?;
        }

        writer.into_frame(FrameType::RemoteRequest)
    }

    pub fn from_frame(frame: Frame) -> Result<Self, String> {
        if !matches!(frame.frame_type, FrameType::RemoteRequest) {
            return Err("expected RemoteRequest frame".to_string());
        }

        let mut reader = WireReader::new(&frame.payload);
        reader.magic(*b"BBR1")?;
        reader.version()?;
        let operation_kind = reader.u8()?;
        reader.zero_reserved()?;
        let request_id = RequestId(reader.array16()?);
        let workspace_session_id = WorkspaceSessionId(reader.array16()?);
        let operation = match operation_kind {
            1 => RemoteOperation::Sync(RemoteSync),
            2 => RemoteOperation::Build(decode_remote_build(&mut reader)?),
            value => return Err(format!("unknown remote operation: {value}")),
        };
        let request = Self { request_id, workspace_session_id, operation };
        reader.finish()?;
        Ok(request)
    }

    pub fn into_domain(self) -> Result<remote_domain::RemoteRequest, String> {
        let request_id = remote_domain::RequestId(self.request_id.0);
        let session_id = remote_domain::WorkspaceSessionId(self.workspace_session_id.0);
        match self.operation {
            RemoteOperation::Sync(_) => Ok(remote_domain::RemoteRequest::sync(request_id, session_id)),
            RemoteOperation::Build(build) => {
                let cwd = remote_domain::WorkspaceRelativePath::new(build.cwd.as_str())?;
                let tool = remote_domain::RemoteTool::new(build.tool.as_str())?;
                let build = remote_domain::RemoteBuild::new(cwd, tool, build.argv, build.env)?;
                Ok(remote_domain::RemoteRequest::build(request_id, session_id, build))
            }
        }
    }
}

fn encode_remote_build(writer: &mut WireWriter, build: &RemoteBuild) -> Result<(), String> {
    validate_remote_build_fields(&build.cwd, &build.tool, &build.argv, &build.env)?;
    writer.string(build.cwd.as_str(), MAX_REMOTE_STRING_BYTES, "remote cwd")?;
    writer.string(build.tool.as_str(), MAX_REMOTE_TOOL_BYTES, "remote tool")?;
    writer.count(build.argv.len(), MAX_REMOTE_ARG_COUNT, "remote argv")?;
    for arg in &build.argv {
        writer.string(arg, MAX_REMOTE_ARG_BYTES, "remote argument")?;
    }
    writer.count(build.env.len(), MAX_REMOTE_ENV_COUNT, "remote environment")?;
    for (key, value) in &build.env {
        writer.string(key, MAX_REMOTE_ENV_KEY_BYTES, "remote environment key")?;
        writer.string(value, MAX_REMOTE_ENV_VALUE_BYTES, "remote environment value")?;
    }
    Ok(())
}

fn decode_remote_build(reader: &mut WireReader<'_>) -> Result<RemoteBuild, String> {
    let cwd = WorkspaceRelativePath::new(reader.string(MAX_REMOTE_STRING_BYTES, "remote cwd")?)?;
    let tool = RemoteTool::new(reader.string(MAX_REMOTE_TOOL_BYTES, "remote tool")?)?;
    let argv = (0..reader.count(MAX_REMOTE_ARG_COUNT, "remote argv")?)
        .map(|_| reader.string(MAX_REMOTE_ARG_BYTES, "remote argument"))
        .collect::<Result<Vec<_>, _>>()?;
    let env = (0..reader.count(MAX_REMOTE_ENV_COUNT, "remote environment")?)
        .map(|_| {
            Ok((
                reader.string(MAX_REMOTE_ENV_KEY_BYTES, "remote environment key")?,
                reader.string(MAX_REMOTE_ENV_VALUE_BYTES, "remote environment value")?,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    RemoteBuild::new(cwd, tool, argv, env)
}

fn validate_remote_build_fields(cwd: &WorkspaceRelativePath, tool: &RemoteTool, argv: &[String], env: &[(String, String)]) -> Result<(), String> {
    validate_remote_string("remote cwd", cwd.as_str(), MAX_REMOTE_STRING_BYTES)?;
    validate_remote_string("remote tool", tool.as_str(), MAX_REMOTE_TOOL_BYTES)?;
    validate_remote_count(argv.len(), MAX_REMOTE_ARG_COUNT, "remote argv")?;
    argv.iter().try_for_each(|arg| validate_remote_string("remote argument", arg, MAX_REMOTE_ARG_BYTES))?;
    validate_remote_count(env.len(), MAX_REMOTE_ENV_COUNT, "remote environment")?;
    env.iter().try_for_each(|(key, value)| {
        validate_remote_string("remote environment key", key, MAX_REMOTE_ENV_KEY_BYTES)?;
        validate_env_key("remote environment key", key)?;
        validate_remote_string("remote environment value", value, MAX_REMOTE_ENV_VALUE_BYTES)
    })
}

fn validate_remote_string(field: &str, value: &str, max: usize) -> Result<(), String> {
    validate_process_string(field, value)?;
    if value.len() > max {
        return Err(format!("{field} exceeds maximum length {max}"));
    }
    Ok(())
}

fn validate_remote_count(count: usize, max: usize, field: &str) -> Result<(), String> {
    if count > max {
        return Err(format!("{field} exceeds maximum count {max}"));
    }
    Ok(())
}

struct WireWriter {
    bytes: Vec<u8>,
}

impl WireWriter {
    fn new(magic: [u8; 4]) -> Self {
        Self { bytes: magic.to_vec() }
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn i32(&mut self, value: i32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn bytes(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn count(&mut self, count: usize, max: usize, field: &str) -> Result<(), String> {
        validate_remote_count(count, max, field)?;
        self.u16(count as u16);
        Ok(())
    }

    fn string(&mut self, value: &str, max: usize, field: &str) -> Result<(), String> {
        validate_remote_string(field, value, max)?;
        let length = u16::try_from(value.len()).map_err(|_| format!("{field} is too long"))?;
        self.u16(length);
        self.bytes(value.as_bytes());
        Ok(())
    }

    fn blob(&mut self, value: &[u8], max: usize, field: &str) -> Result<(), String> {
        if value.len() > max {
            return Err(format!("{field} exceeds maximum length {max}"));
        }
        let length = u32::try_from(value.len()).map_err(|_| format!("{field} is too long"))?;
        self.bytes.extend_from_slice(&length.to_le_bytes());
        self.bytes(value);
        Ok(())
    }

    fn into_frame(self, frame_type: FrameType) -> Result<Frame, String> {
        if self.bytes.len() > MAX_FRAME_PAYLOAD {
            return Err(format!("remote payload exceeds frame limit {MAX_FRAME_PAYLOAD}"));
        }
        Ok(Frame::new(frame_type, self.bytes))
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

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self.offset.checked_add(length).ok_or_else(|| "remote payload length overflow".to_string())?;
        let value = self.bytes.get(self.offset..end).ok_or_else(|| "truncated remote payload".to_string())?;
        self.offset = end;
        Ok(value)
    }

    fn magic(&mut self, expected: [u8; 4]) -> Result<(), String> {
        if self.take(4)? != expected {
            return Err("invalid remote payload magic".to_string());
        }
        Ok(())
    }

    fn version(&mut self) -> Result<(), String> {
        let version = self.u16()?;
        if version != REMOTE_PROTOCOL_VERSION {
            return Err(format!("unsupported remote protocol version: {version}"));
        }
        Ok(())
    }

    fn zero_reserved(&mut self) -> Result<(), String> {
        if self.u8()? != 0 {
            return Err("remote payload reserved byte is nonzero".to_string());
        }
        Ok(())
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| "invalid remote integer".to_string())?))
    }

    fn i32(&mut self) -> Result<i32, String> {
        let bytes = self.take(4)?;
        Ok(i32::from_le_bytes(bytes.try_into().map_err(|_| "invalid remote integer".to_string())?))
    }

    fn array16(&mut self) -> Result<[u8; 16], String> {
        self.take(16)?.try_into().map_err(|_| "invalid remote identifier".to_string())
    }

    fn count(&mut self, max: usize, field: &str) -> Result<usize, String> {
        let count = self.u16()? as usize;
        validate_remote_count(count, max, field)?;
        Ok(count)
    }

    fn string(&mut self, max: usize, field: &str) -> Result<String, String> {
        let length = self.u16()? as usize;
        if length > max {
            return Err(format!("{field} exceeds maximum length {max}"));
        }
        let value = std::str::from_utf8(self.take(length)?).map_err(|_| format!("{field} is not valid UTF-8"))?;
        validate_remote_string(field, value, max)?;
        Ok(value.to_string())
    }

    fn blob(&mut self, max: usize, field: &str) -> Result<Vec<u8>, String> {
        let bytes = self.take(4)?;
        let length = u32::from_le_bytes(bytes.try_into().map_err(|_| "invalid remote blob length".to_string())?) as usize;
        if length > max {
            return Err(format!("{field} exceeds maximum length {max}"));
        }
        Ok(self.take(length)?.to_vec())
    }

    fn finish(self) -> Result<(), String> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err("trailing bytes in remote payload".to_string())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteErrorCode {
    Failed = 1,
}

impl RemoteErrorCode {
    fn from_u16(value: u16) -> Result<Self, String> {
        match value {
            1 => Ok(Self::Failed),
            _ => Err(format!("unknown remote error code: {value}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteEventKind {
    SyncProgress { completed_bytes: u64, total_bytes: Option<u64> },
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Error { code: RemoteErrorCode, message: String },
    Cancelled,
    Completed { exit_code: i32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEvent {
    pub request_id: RequestId,
    pub kind: RemoteEventKind,
}

impl RemoteEvent {
    pub fn from_backend_event(request_id: remote_domain::RequestId, event: remote_domain::RemoteBackendEvent) -> Self {
        let request_id = RequestId(request_id.0);
        let kind = match event {
            remote_domain::RemoteBackendEvent::SyncProgress { completed_bytes, total_bytes } => {
                RemoteEventKind::SyncProgress { completed_bytes, total_bytes }
            }
            remote_domain::RemoteBackendEvent::Stdout(data) => RemoteEventKind::Stdout(data),
            remote_domain::RemoteBackendEvent::Stderr(data) => RemoteEventKind::Stderr(data),
            remote_domain::RemoteBackendEvent::Error { message } => RemoteEventKind::Error { code: RemoteErrorCode::Failed, message },
            remote_domain::RemoteBackendEvent::Cancelled => RemoteEventKind::Cancelled,
            remote_domain::RemoteBackendEvent::Completed { exit_code } => RemoteEventKind::Completed { exit_code },
        };
        Self { request_id, kind }
    }

    pub fn to_frame(&self) -> Result<Frame, String> {
        let mut writer = WireWriter::new(*b"BBE1");
        writer.u16(REMOTE_PROTOCOL_VERSION);
        writer.u8(match &self.kind {
            RemoteEventKind::SyncProgress { .. } => 1,
            RemoteEventKind::Stdout(_) => 2,
            RemoteEventKind::Stderr(_) => 3,
            RemoteEventKind::Error { .. } => 4,
            RemoteEventKind::Cancelled => 5,
            RemoteEventKind::Completed { .. } => 6,
        });
        writer.u8(0);
        writer.bytes(&self.request_id.0);

        match &self.kind {
            RemoteEventKind::SyncProgress { completed_bytes, total_bytes } => {
                writer.u64(*completed_bytes);
                writer.u8(u8::from(total_bytes.is_some()));
                if let Some(total_bytes) = total_bytes {
                    writer.u64(*total_bytes);
                }
            }
            RemoteEventKind::Stdout(data) | RemoteEventKind::Stderr(data) => writer.blob(data, MAX_FRAME_PAYLOAD, "remote output")?,
            RemoteEventKind::Error { code, message } => {
                writer.u16(*code as u16);
                writer.string(message, MAX_REMOTE_ERROR_BYTES, "remote error")?;
            }
            RemoteEventKind::Cancelled => {}
            RemoteEventKind::Completed { exit_code } => writer.i32(*exit_code),
        }

        writer.into_frame(FrameType::RemoteEvent)
    }

    pub fn from_frame(frame: Frame) -> Result<Self, String> {
        if !matches!(frame.frame_type, FrameType::RemoteEvent) {
            return Err("expected RemoteEvent frame".to_string());
        }

        let mut reader = WireReader::new(&frame.payload);
        reader.magic(*b"BBE1")?;
        reader.version()?;
        let event_kind = reader.u8()?;
        reader.zero_reserved()?;
        let request_id = RequestId(reader.array16()?);
        let kind = match event_kind {
            1 => {
                let completed_bytes = reader.u64()?;
                let total_bytes = match reader.u8()? {
                    0 => None,
                    1 => Some(reader.u64()?),
                    value => return Err(format!("invalid remote progress total flag: {value}")),
                };
                RemoteEventKind::SyncProgress { completed_bytes, total_bytes }
            }
            2 => RemoteEventKind::Stdout(reader.blob(MAX_FRAME_PAYLOAD, "remote stdout")?),
            3 => RemoteEventKind::Stderr(reader.blob(MAX_FRAME_PAYLOAD, "remote stderr")?),
            4 => RemoteEventKind::Error {
                code: RemoteErrorCode::from_u16(reader.u16()?)?,
                message: reader.string(MAX_REMOTE_ERROR_BYTES, "remote error")?,
            },
            5 => RemoteEventKind::Cancelled,
            6 => RemoteEventKind::Completed { exit_code: reader.i32()? },
            value => return Err(format!("unknown remote event kind: {value}")),
        };
        reader.finish()?;
        Ok(Self { request_id, kind })
    }
}

pub fn validate_process_string(field: &str, value: &str) -> Result<(), String> {
    if value.as_bytes().contains(&0) {
        return Err(format!("{field} contains a NUL byte"));
    }

    Ok(())
}

pub fn validate_process_path(field: &str, value: &Path) -> Result<(), String> {
    if os_str_contains_nul(value.as_os_str()) {
        return Err(format!("{field} contains a NUL byte"));
    }

    Ok(())
}

pub fn validate_env_key(field: &str, key: &str) -> Result<(), String> {
    validate_process_string(field, key)?;
    if key.is_empty() {
        return Err(format!("{field} is empty"));
    }
    if key.contains('=') {
        return Err(format!("{field} contains '='"));
    }

    Ok(())
}

pub fn validate_exec_request(req: &ExecRequest) -> Result<(), String> {
    validate_process_string("request cwd", &req.cwd)?;
    if req.command.is_empty() {
        return Err("request command is empty".to_string());
    }
    validate_process_string("request command", &req.command)?;

    for (index, arg) in req.args.iter().enumerate() {
        validate_process_string(&format!("request argument {index}"), arg)?;
    }

    for (index, (key, value)) in req.env.iter().enumerate() {
        validate_env_key(&format!("request environment key {index}"), key)?;
        validate_process_string(&format!("request environment value for '{key}'"), value)?;
    }

    Ok(())
}

fn os_str_contains_nul(value: &OsStr) -> bool {
    #[cfg(unix)]
    {
        value.as_bytes().contains(&0)
    }

    #[cfg(not(unix))]
    {
        value.to_string_lossy().contains('\0')
    }
}

pub struct Frame {
    pub frame_type: FrameType,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(frame_type: FrameType, payload: Vec<u8>) -> Self {
        Self { frame_type, payload }
    }

    pub fn read<R: Read>(reader: &mut R) -> io::Result<Self> {
        let mut header = [0u8; 6];
        reader.read_exact(&mut header)?;

        let (frame_type, payload_len) = decode_header(&header)?;

        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            reader.read_exact(&mut payload)?;
        }

        Ok(Self { frame_type, payload })
    }

    pub async fn read_async<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Self> {
        let mut header = [0u8; 6];
        reader.read_exact(&mut header).await?;

        let (frame_type, payload_len) = decode_header(&header)?;
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            reader.read_exact(&mut payload).await?;
        }

        Ok(Self { frame_type, payload })
    }

    pub fn write<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        let header = self.header()?;
        writer.write_all(&header)?;

        if !self.payload.is_empty() {
            writer.write_all(&self.payload)?;
        }

        writer.flush()?;
        Ok(())
    }

    pub async fn write_async<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> io::Result<()> {
        let header = self.header()?;
        writer.write_all(&header).await?;

        if !self.payload.is_empty() {
            writer.write_all(&self.payload).await?;
        }

        writer.flush().await
    }

    fn header(&self) -> io::Result<[u8; 6]> {
        validate_payload_size(self.payload.len())?;

        let mut header = [0u8; 6];
        header[0..2].copy_from_slice(&(self.frame_type as u16).to_le_bytes());
        header[2..6].copy_from_slice(&(self.payload.len() as u32).to_le_bytes());
        Ok(header)
    }
}

fn decode_header(header: &[u8; 6]) -> io::Result<(FrameType, usize)> {
    let frame_type_raw = u16::from_le_bytes([header[0], header[1]]);
    let frame_type = FrameType::from_u16(frame_type_raw)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("unknown frame type: {frame_type_raw}")))?;
    let payload_len = u32::from_le_bytes([header[2], header[3], header[4], header[5]]) as usize;
    validate_payload_size(payload_len)?;
    Ok((frame_type, payload_len))
}

fn validate_payload_size(payload_len: usize) -> io::Result<()> {
    if payload_len > MAX_FRAME_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame payload too large: {payload_len} bytes (maximum {MAX_FRAME_PAYLOAD})"),
        ));
    }

    Ok(())
}

impl ExecRequest {
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        buf.extend_from_slice(self.cwd.as_bytes());
        buf.push(0);

        buf.extend_from_slice(self.command.as_bytes());
        buf.push(0);

        for arg in &self.args {
            buf.extend_from_slice(arg.as_bytes());
            buf.push(0);
        }
        buf.push(0);

        for (key, val) in &self.env {
            buf.extend_from_slice(key.as_bytes());
            buf.push(b'=');
            buf.extend_from_slice(val.as_bytes());
            buf.push(0);
        }
        buf.push(0);

        buf
    }

    pub fn deserialize(data: &[u8]) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut start = 0;

        for (i, &byte) in data.iter().enumerate() {
            if byte == 0 {
                parts.push(&data[start..i]);
                start = i + 1;
            }
        }

        let mut iter = parts.into_iter();

        let cwd = iter.next().map(|b| String::from_utf8_lossy(b).to_string()).ok_or_else(|| "missing cwd".to_string())?;

        let command = iter.next().map(|b| String::from_utf8_lossy(b).to_string()).ok_or_else(|| "missing command".to_string())?;

        let mut args = Vec::new();
        loop {
            let next = iter.next().ok_or_else(|| "unexpected end of args".to_string())?;
            if next.is_empty() {
                break;
            }
            args.push(String::from_utf8_lossy(next).to_string());
        }

        let mut env = Vec::new();
        for raw in iter {
            if raw.is_empty() {
                continue;
            }
            let s = String::from_utf8_lossy(raw);
            if let Some((key, val)) = s.split_once('=') {
                env.push((key.to_string(), val.to_string()));
            }
        }

        Ok(Self { cwd, command, args, env })
    }
}

pub fn encode_ui_payload(widget: &str, command: &str, options: &str, value: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(widget.as_bytes());
    buf.push(0);
    buf.extend_from_slice(command.as_bytes());
    buf.push(0);
    buf.extend_from_slice(options.as_bytes());
    buf.push(0);
    buf.extend_from_slice(value.as_bytes());
    buf.push(0);
    buf
}

pub fn decode_ui_payload(payload: &[u8]) -> Option<(&str, &str, &str, &str)> {
    let mut parts = payload.split(|&b| b == 0);
    let widget = std::str::from_utf8(parts.next()?).ok()?;
    let command = std::str::from_utf8(parts.next()?).ok()?;
    let options = std::str::from_utf8(parts.next()?).ok()?;
    let value = std::str::from_utf8(parts.next()?).ok()?;
    Some((widget, command, options, value))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Trigger {
    OnPty,
    DelayMs(u64),
    DelayMsAfterPty(u64),
}

pub fn parse_triggers(options: &str) -> Vec<Trigger> {
    if options.is_empty() {
        return Vec::new();
    }

    let mut has_on_pty = false;
    let mut delay_ms: Option<u64> = None;

    for s in options.split(',') {
        let s = s.trim();
        if s == "ON_PTY" {
            has_on_pty = true;
        } else if let Some(secs) = s.strip_prefix("SEC_") {
            if let Ok(secs) = secs.parse::<f64>() {
                delay_ms = Some((secs * 1000.0) as u64);
            }
        }
    }

    if has_on_pty {
        if let Some(ms) = delay_ms {
            vec![Trigger::DelayMsAfterPty(ms)]
        } else {
            vec![Trigger::OnPty]
        }
    } else if let Some(ms) = delay_ms {
        vec![Trigger::DelayMs(ms)]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
#[path = "mod_ut.rs"]
mod vscomm_tests;
