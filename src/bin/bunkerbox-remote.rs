use bunkerbox::remote::RemoteSnapshotId;
use bunkerbox::remote_client::{
    execute_remote_request_to, logical_workspace_cwd, new_request_id, remote_build_request, remote_environment_names, remote_session_from_env,
    remote_sync_request, remote_tool_enabled, selected_remote_environment, RemoteCompletion,
};
#[cfg(test)]
use bunkerbox::vscomm::RequestId;
use bunkerbox::vscomm::{RemoteRequest, WorkspaceSessionId, TOOLCHAIN_PORT, VSCOMM_BIN_DIR};
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::mem;
use std::os::unix::fs::symlink;
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
    let invoked_as =
        env::args_os().next().and_then(|value| Path::new(&value).file_name().and_then(|name| name.to_str()).map(str::to_owned)).unwrap_or_default();
    let args = env::args().skip(1).collect::<Vec<_>>();

    if invoked_as == "make" {
        return run_transparent_make(&args);
    }
    if invoked_as != "bunkerbox-remote" {
        return Err("bunkerbox-remote must be invoked directly or through the managed make symlink".to_string());
    }
    if args.len() == 1 && args[0] == "install" {
        install_remote_links()?;
        return Ok(0);
    }

    run_explicit(&args)
}

fn run_explicit(args: &[String]) -> Result<i32, String> {
    let command = parse_command(args)?;
    match &command {
        RemoteCommand::Sync => eprintln!("bunkerbox-remote: syncing"),
        RemoteCommand::Build { tool, .. } => eprintln!("bunkerbox-remote: building {tool}"),
    }

    let cwd = logical_workspace_cwd(&env::current_dir().map_err(|error| format!("current directory: {error}"))?)?;
    let session = remote_session_from_env()?;
    match command {
        RemoteCommand::Sync => {
            sync_snapshot(session)?;
            Ok(0)
        }
        RemoteCommand::Build { tool, args } => run_build_with_sync(cwd, tool, args, session),
    }
}

fn run_transparent_make(args: &[String]) -> Result<i32, String> {
    let cwd = logical_workspace_cwd(&env::current_dir().map_err(|error| format!("current directory: {error}"))?)?;
    let session = remote_session_from_env()?;
    run_build_with_sync(cwd, "make".to_string(), args.to_vec(), session)
}

fn run_build_with_sync(cwd: String, tool: String, args: Vec<String>, session: WorkspaceSessionId) -> Result<i32, String> {
    let environment = selected_remote_environment(remote_environment_names());
    run_build_with_sync_using(cwd, tool, args, environment, session, execute_request_over_vsock)
}

fn run_build_with_sync_using<F>(
    cwd: String, tool: String, args: Vec<String>, environment: Vec<(String, String)>, session: WorkspaceSessionId, mut execute: F,
) -> Result<i32, String>
where
    F: FnMut(RemoteRequest) -> Result<RemoteCompletion, String>,
{
    let snapshot_id = match execute(remote_sync_request(new_request_id(), session))? {
        RemoteCompletion::Synced(snapshot_id) => snapshot_id,
        RemoteCompletion::Completed(_) => return Err("remote sync returned a build completion".to_string()),
    };
    let request = remote_build_request(new_request_id(), session, cwd, tool, args, environment, snapshot_id)?;
    match execute(request)? {
        RemoteCompletion::Completed(code) => Ok(code),
        RemoteCompletion::Synced(_) => Err("remote build returned a sync completion".to_string()),
    }
}

fn execute_request_over_vsock(request: RemoteRequest) -> Result<RemoteCompletion, String> {
    let mut stream = connect_toolchain()?;
    execute_remote_request_to(&mut stream, request, &mut io::stdout(), &mut io::stderr())
}

fn sync_snapshot(session: WorkspaceSessionId) -> Result<RemoteSnapshotId, String> {
    match execute_request_over_vsock(remote_sync_request(new_request_id(), session))? {
        RemoteCompletion::Synced(snapshot_id) => Ok(snapshot_id),
        RemoteCompletion::Completed(_) => Err("remote sync returned a build completion".to_string()),
    }
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

#[cfg(test)]
fn build_request(
    command: RemoteCommand, cwd: String, request_id: RequestId, session_id: WorkspaceSessionId, snapshot_id: RemoteSnapshotId,
) -> Result<RemoteRequest, String> {
    match command {
        RemoteCommand::Sync => Ok(remote_sync_request(request_id, session_id)),
        RemoteCommand::Build { tool, args } => remote_build_request(request_id, session_id, cwd, tool, args, Vec::new(), snapshot_id),
    }
}

fn install_remote_links() -> Result<(), String> {
    fs::create_dir_all(VSCOMM_BIN_DIR).map_err(|error| format!("mkdir {VSCOMM_BIN_DIR}: {error}"))?;
    let executable = env::current_exe().map_err(|error| format!("failed to locate remote binary: {error}"))?;
    install_remote_make_link(Path::new(VSCOMM_BIN_DIR), &executable, remote_tool_enabled("make"))
}

fn install_remote_make_link(bin_dir: &Path, executable: &Path, enabled: bool) -> Result<(), String> {
    let target = bin_dir.join("make");
    let managed = is_managed_link(&target, executable);

    if enabled {
        if target.exists() || fs::symlink_metadata(&target).is_ok() {
            if !managed {
                return Err(format!("cannot install remote make wrapper over existing {}", target.display()));
            }
            fs::remove_file(&target).map_err(|error| format!("remove existing remote make wrapper: {error}"))?;
        }
        symlink(executable, &target).map_err(|error| format!("symlink remote make wrapper: {error}"))?;
    } else if managed {
        fs::remove_file(&target).map_err(|error| format!("remove disabled remote make wrapper: {error}"))?;
    }
    Ok(())
}

fn is_managed_link(target: &Path, executable: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(target) else { return false };
    if !metadata.file_type().is_symlink() {
        return false;
    }
    let Ok(link) = fs::read_link(target) else { return false };
    link == executable
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
