// Dead-code warnings are expected here: vscomm is shared between
// two binaries (bunkerbox and bunkerbox-vscomm) that use different items.
#![allow(dead_code)]
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::Path;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

pub mod buildsys;
pub const TOOLCHAIN_PORT: u32 = 9999;
// Keep UI traffic on a separate vsock endpoint from command execution.
pub const TUI_STATUS_PORT: u32 = 10000;
pub const VSCOMM_BIN_DIR: &str = "/usr/local/bunkerbox/bin";

#[repr(u16)]
#[derive(Clone, Copy)]
pub enum FrameType {
    ExecReq = 1,
    Stdout = 2,
    Stderr = 3,
    Exit = 4,
    Disconnect = 5,
    UiCommand = 10,
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

        let frame_type_raw = u16::from_le_bytes([header[0], header[1]]);
        let frame_type = FrameType::from_u16(frame_type_raw)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("unknown frame type: {frame_type_raw}")))?;

        let payload_len = u32::from_le_bytes([header[2], header[3], header[4], header[5]]) as usize;

        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            reader.read_exact(&mut payload)?;
        }

        Ok(Self { frame_type, payload })
    }

    pub fn write<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        let frame_type_raw = self.frame_type as u16;
        let payload_len = self.payload.len() as u32;

        let mut header = [0u8; 6];
        header[0..2].copy_from_slice(&frame_type_raw.to_le_bytes());
        header[2..6].copy_from_slice(&payload_len.to_le_bytes());
        writer.write_all(&header)?;

        if !self.payload.is_empty() {
            writer.write_all(&self.payload)?;
        }

        writer.flush()?;
        Ok(())
    }
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
