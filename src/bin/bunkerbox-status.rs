#[path = "../vscomm/mod.rs"]
mod vscomm;

use std::env;
use std::io::{self, Read, Write};
use std::mem;
use std::os::unix::io::RawFd;
use vscomm::{Frame, FrameType, STATUS_PORT};

const HOST_CID: u32 = 2;

fn main() {
    let result = run();
    if let Err(err) = result {
        eprintln!("bunkerbox-status: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();

    let widget = args.get(1).ok_or_else(|| "usage: bunkerbox-status <widget> <command> [options] [value]".to_string())?;
    let command = args.get(2).ok_or_else(|| "usage: bunkerbox-status <widget> <command> [options] [value]".to_string())?;
    let options = args.get(3).map(|s| s.as_str()).unwrap_or("");
    let value = args.get(4).map(|s| s.as_str()).unwrap_or("");

    if !widget.is_empty() && !command.is_empty() {
        let payload = vscomm::encode_ui_payload(widget, command, options, value);
        let frame = Frame::new(FrameType::UiCommand, payload);

        let mut stream = vsock_connect(HOST_CID, STATUS_PORT).map_err(|e| format!("vsock connect: {e}"))?;
        frame.write(&mut stream).map_err(|e| format!("send: {e}"))?;
        stream.flush().map_err(|e| format!("flush: {e}"))?;
    }

    Ok(())
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
    fd: RawFd,
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
