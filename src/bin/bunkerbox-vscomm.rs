#[path = "../vscomm/mod.rs"]
mod vscomm;

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::mem;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use vscomm::{ExecRequest, Frame, FrameType, VSCOMM_BIN_DIR, VSOCK_PORT};

const HOST_CID: u32 = 2;

fn main() {
    let result = run();
    if let Err(err) = result {
        eprintln!("bunkerbox-vscomm: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = env::args().collect::<Vec<_>>();

    if args.get(1).map(|a| a.as_str()) == Some("install") {
        return install_symlinks();
    }

    let invoked_as = Path::new(&args[0])
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| *n != "bunkerbox-vscomm")
        .ok_or_else(|| "bunkerbox-vscomm must be invoked via symlink (not directly)".to_string())?
        .to_string();

    let cwd = env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let env_vars: Vec<(String, String)> = env::vars().collect();

    let req = ExecRequest { cwd: cwd.to_string_lossy().to_string(), command: invoked_as, args: args[1..].to_vec(), env: env_vars };

    let frame = Frame::new(FrameType::ExecReq, req.serialize());
    let mut stream = vsock_connect(HOST_CID, VSOCK_PORT).map_err(|e| format!("vsock connect: {e}"))?;

    frame.write(&mut stream).map_err(|e| format!("send request: {e}"))?;
    stream.flush().map_err(|e| format!("flush: {e}"))?;

    loop {
        let response = Frame::read(&mut stream).map_err(|e| format!("read response: {e}"))?;

        match response.frame_type {
            FrameType::Stdout => {
                io::stdout().write_all(&response.payload).map_err(|e| format!("stdout: {e}"))?;
                io::stdout().flush().map_err(|e| format!("flush stdout: {e}"))?;
            }
            FrameType::Stderr => {
                io::stderr().write_all(&response.payload).map_err(|e| format!("stderr: {e}"))?;
                io::stderr().flush().map_err(|e| format!("flush stderr: {e}"))?;
            }
            FrameType::Exit => {
                if response.payload.len() >= 4 {
                    let code = i32::from_le_bytes([response.payload[0], response.payload[1], response.payload[2], response.payload[3]]);
                    std::process::exit(code);
                }
                return Ok(());
            }
            _ => return Err(format!("unexpected frame type from host: {:?}", response.frame_type as u16)),
        }
    }
}

fn install_symlinks() -> Result<(), String> {
    let config_path = find_config().ok_or_else(|| "no whitelist config found".to_string())?;

    let entries = read_whitelist_entries(&config_path)?;

    fs::create_dir_all(VSCOMM_BIN_DIR).map_err(|e| format!("mkdir {VSCOMM_BIN_DIR}: {e}"))?;

    let vscomm_path = env::current_exe().map_err(|e| format!("failed to locate vscomm binary: {e}"))?;

    let mut installed = 0;
    for entry in &entries {
        let cmd = extract_command_name(entry);
        if cmd.is_empty() {
            continue;
        }
        if command_exists_in_path_except(&cmd, &vscomm_path) {
            continue;
        }
        let target = PathBuf::from(VSCOMM_BIN_DIR).join(&cmd);
        if target.exists() {
            let _ = fs::remove_file(&target);
        }
        std::os::unix::fs::symlink(&vscomm_path, &target).map_err(|e| format!("symlink {cmd}: {e}"))?;
        installed += 1;
    }

    if installed > 0 {
        eprintln!("bunkerbox-vscomm: installed {installed} passthrough commands");
    }

    Ok(())
}

fn find_config() -> Option<PathBuf> {
    let paths = [PathBuf::from("/workspace/.bunkerbox/env.conf"), PathBuf::from(".bunkerbox/env.conf")];
    paths.into_iter().find(|p| p.exists())
}

fn read_whitelist_entries(path: &Path) -> Result<Vec<String>, String> {
    let contents = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;

    let mut in_passthrough = false;
    let mut entries = Vec::new();

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !in_passthrough {
            if line == "passthrough:" {
                in_passthrough = true;
            }
            continue;
        }
        if !line.starts_with('-') {
            break;
        }
        let entry = line[1..].trim();
        let entry = entry.trim_matches('"');
        entries.push(entry.to_string());
    }

    Ok(entries)
}

fn extract_command_name(entry: &str) -> String {
    let trimmed = entry.trim();
    if let Some(rest) = trimmed.strip_suffix(" *") {
        rest.trim().to_string()
    } else {
        trimmed.to_string()
    }
}

fn command_exists_in_path_except(cmd: &str, except: &Path) -> bool {
    if let Ok(path) = env::var("PATH") {
        for dir in path.split(':') {
            let candidate = Path::new(dir).join(cmd);
            if candidate == except {
                continue;
            }
            if candidate.is_file() {
                let metadata = match fs::metadata(&candidate) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if metadata.permissions().mode() & 0o111 != 0 {
                    return true;
                }
            }
        }
    }
    false
}

fn vsock_connect(cid: u32, port: u32) -> io::Result<VsockStream> {
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let addr = libc::sockaddr_vm { svm_family: libc::AF_VSOCK as u16, svm_reserved1: 0, svm_port: port, svm_cid: cid, svm_zero: [0u8; 4] };

        let addr_ptr = &addr as *const libc::sockaddr_vm as *const libc::sockaddr;
        let addr_len = mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;

        if libc::connect(fd, addr_ptr, addr_len) < 0 {
            let err = io::Error::last_os_error();
            libc::close(fd);
            return Err(err);
        }

        Ok(VsockStream { fd })
    }
}

struct VsockStream {
    fd: libc::c_int,
}

impl Read for VsockStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let ret = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ret as usize)
    }
}

impl Write for VsockStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let ret = unsafe { libc::write(self.fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ret as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for VsockStream {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}
