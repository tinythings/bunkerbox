use bunkerbox::remote_client::{execute_remote_request_to, remote_build_request, remote_sync_request};
use bunkerbox::vscomm::{RemoteRequest, RequestId, WorkspaceSessionId, TOOLCHAIN_PORT};
use rand::RngCore;
use std::env;
use std::io::{self, Read, Write};
use std::mem;
use std::path::Path;

const HOST_CID: u32 = 2;

#[derive(Debug, PartialEq, Eq)]
enum RemoteCommand {
    Sync,
    Build { tool: String, args: Vec<String> },
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("bunkerbox-remote: remote operation failed: {error}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<i32, String> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let command = parse_command(&args)?;
    match &command {
        RemoteCommand::Sync => eprintln!("bunkerbox-remote: syncing"),
        RemoteCommand::Build { tool, .. } => eprintln!("bunkerbox-remote: building {tool}"),
    }
    let cwd = logical_workspace_cwd(&env::current_dir().map_err(|error| format!("current directory: {error}"))?)?;
    let session = env::var("BUNKERBOX_REMOTE_SESSION")
        .map_err(|_| "BUNKERBOX_REMOTE_SESSION is missing".to_string())
        .and_then(|value| WorkspaceSessionId::from_hex(&value))?;
    let request = build_request(command, cwd, new_request_id(), session)?;
    let mut stream = connect_toolchain()?;
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    execute_remote_request_to(&mut stream, request, &mut stdout, &mut stderr)
}

fn parse_command(args: &[String]) -> Result<RemoteCommand, String> {
    match args {
        [command] if command == "sync" => Ok(RemoteCommand::Sync),
        [command, tool, rest @ ..] if command == "build" && !tool.is_empty() => Ok(RemoteCommand::Build { tool: tool.clone(), args: rest.to_vec() }),
        [command, ..] if command == "build" => Err("usage: bunkerbox-remote build <tool> [args...]".to_string()),
        [] => Err("usage: bunkerbox-remote sync | build <tool> [args...]".to_string()),
        _ => Err("usage: bunkerbox-remote sync | build <tool> [args...]".to_string()),
    }
}

fn build_request(command: RemoteCommand, cwd: String, request_id: RequestId, session_id: WorkspaceSessionId) -> Result<RemoteRequest, String> {
    match command {
        RemoteCommand::Sync => Ok(remote_sync_request(request_id, session_id)),
        RemoteCommand::Build { tool, args } => remote_build_request(request_id, session_id, cwd, tool, args, Vec::new()),
    }
}

fn logical_workspace_cwd(path: &Path) -> Result<String, String> {
    let relative = path.strip_prefix("/workspace").map_err(|_| "current directory must be under /workspace".to_string())?;
    let value = relative.to_str().ok_or_else(|| "current directory is not valid UTF-8".to_string())?.to_string();
    bunkerbox::remote::WorkspaceRelativePath::new(&value)?;
    Ok(value)
}

fn new_request_id() -> RequestId {
    let mut bytes = [0; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    RequestId(bytes)
}

fn connect_toolchain() -> Result<VsockStream, String> {
    vsock_connect(HOST_CID, TOOLCHAIN_PORT).map_err(|error| format!("toolchain vsock connect: {error}"))
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
            let error = io::Error::last_os_error();
            libc::close(fd);
            return Err(error);
        }
        Ok(VsockStream { fd })
    }
}

struct VsockStream {
    fd: libc::c_int,
}

impl Read for VsockStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let result = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(result as usize)
    }
}

impl Write for VsockStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let result = unsafe { libc::write(self.fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(result as usize)
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

#[cfg(test)]
#[path = "../bunkerbox-remote_ut.rs"]
mod tests;
