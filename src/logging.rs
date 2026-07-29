use std::cell::RefCell;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::io::RawFd;

const LOG_PATH: &str = "/tmp/bunkerbox.log";

thread_local! {
    static STATUS_FD: RefCell<Option<RawFd>> = const { RefCell::new(None) };
}

pub fn log_path() -> &'static str {
    LOG_PATH
}

pub fn set_status_fd(fd: RawFd) {
    STATUS_FD.with(|f| *f.borrow_mut() = Some(fd));
}

/// Sends a password prompt to the TUI, blocks reading the response from the status fd.
pub fn prompt_password(title: &str, prompt: &str) -> Result<String, String> {
    let fd = STATUS_FD.with(|f| f.borrow().ok_or("status fd not set".to_string()))?;

    let payload = crate::vscomm::encode_ui_payload("password", "show", title, prompt);
    let mut buf = b"@".to_vec();
    buf.extend_from_slice(&payload);
    buf.push(b'\n');
    unsafe {
        libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len());
    }

    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = unsafe { libc::read(fd, byte.as_mut_ptr() as *mut libc::c_void, 1) };
        if n <= 0 {
            return Err("failed to read password response".to_string());
        }
        if byte[0] == b'\n' {
            break;
        }
        response.push(byte[0]);
    }

    let payload = crate::vscomm::encode_ui_payload("password", "hide", "", "");
    let mut buf = b"@".to_vec();
    buf.extend_from_slice(&payload);
    buf.push(b'\n');
    unsafe {
        libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len());
    }

    String::from_utf8(response).map_err(|e| format!("invalid password encoding: {e}"))
}

pub fn log(msg: &str) {
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let line = format!("[{ts}] {msg}\n");

    eprint!("[bb] {line}");

    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(LOG_PATH) {
        let _ = f.write_all(line.as_bytes());
    }

    STATUS_FD.with(|f| {
        if let Some(fd) = *f.borrow() {
            let title = format!("Log: {LOG_PATH}");
            let payload = crate::vscomm::encode_ui_payload("popup", "info", &title, msg);
            let mut buf = b"@".to_vec();
            buf.extend_from_slice(&payload);
            buf.push(b'\n');
            unsafe {
                libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len());
            }
        }
    });
}
