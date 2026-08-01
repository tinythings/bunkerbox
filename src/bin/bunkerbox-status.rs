#[path = "../vscomm/mod.rs"]
mod vscomm;

use std::env;
use std::io::{self, Read, Write};
use std::mem;
use std::os::unix::io::RawFd;
use vscomm::{Frame, FrameType, TUI_STATUS_PORT};

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
    let mut find_pattern: Option<String> = None;
    let mut find_ascii = false;
    let mut timeout_secs: Option<u64> = None;

    let mut i = 1;
    let mut positional = vec![args[0].clone()];
    while i < args.len() {
        if args[i] == "--help" {
            print_help(&args);
            std::process::exit(0);
        } else if args[i] == "--on-find" && i + 1 < args.len() {
            find_pattern = Some(args[i + 1].clone());
            i += 2;
        } else if let Some(p) = args[i].strip_prefix("--on-find=") {
            find_pattern = Some(p.to_string());
            i += 1;
        } else if args[i] == "--on-find-ascii" {
            find_ascii = true;
            i += 1;
        } else if args[i] == "--on-timeout" && i + 1 < args.len() {
            timeout_secs = args[i + 1].parse::<u64>().ok();
            i += 2;
        } else if let Some(s) = args[i].strip_prefix("--on-timeout=") {
            timeout_secs = s.parse::<u64>().ok();
            i += 1;
        } else {
            positional.push(args[i].clone());
            i += 1;
        }
    }

    let widget = positional.get(1).ok_or_else(|| "usage: bunkerbox-status <widget> <command> [flags] [value]".to_string())?;
    let command = positional.get(2).ok_or_else(|| "usage: bunkerbox-status <widget> <command> [flags] [value]".to_string())?;
    let pos_value = positional.get(3).map(|s| s.as_str()).unwrap_or("");

    let options = timeout_secs.map(|s| format!("SEC_{s}")).unwrap_or_default();
    let value = if find_ascii {
        "ASCII".to_string()
    } else if let Some(p) = find_pattern {
        p
    } else {
        pos_value.to_string()
    };

    if !widget.is_empty() && !command.is_empty() {
        let payload = vscomm::encode_ui_payload(widget, command, &options, &value);
        let frame = Frame::new(FrameType::UiCommand, payload);

        let mut stream = vsock_connect(HOST_CID, TUI_STATUS_PORT).map_err(|e| format!("TUI vsock connect: {e}"))?;
        frame.write(&mut stream).map_err(|e| format!("send: {e}"))?;
        stream.flush().map_err(|e| format!("flush: {e}"))?;
    }

    Ok(())
}

fn print_help(args: &[String]) {
    let name = args.first().map(|s| s.as_str()).unwrap_or("bunkerbox-status");
    eprintln!("Usage: {name} <widget> <command> [flags] [value]\n");
    eprintln!("Send a UI command to the bunkerbox TUI over vsock.\n");
    eprintln!("Flags:");
    eprintln!("  --on-find=<str>       Hide popup when exact text appears on screen");
    eprintln!("  --on-find-ascii       Hide popup when [a-zA-Z0-9] appears on screen");
    eprintln!("  --on-timeout=<secs>   Hide popup after N seconds");
    eprintln!("  --help                Show this message\n");
    eprintln!("Examples:");
    eprintln!("  {name} popup hide --on-find-ascii");
    eprintln!("  {name} popup hide --on-find='$' --on-timeout=10");
    eprintln!("  {name} status set \"Running: unit tests\"");
}

fn vsock_connect(cid: u32, port: u32) -> io::Result<VsockStream> {
    let delays = [100u64, 500, 1000];
    for (i, &ms) in delays.iter().enumerate() {
        match try_connect(cid, port) {
            Ok(s) => return Ok(s),
            Err(e) if i < delays.len() - 1 => {
                eprintln!("bunkerbox-status: vsock connect attempt {} failed: {e}", i + 1);
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

fn try_connect(cid: u32, port: u32) -> io::Result<VsockStream> {
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
