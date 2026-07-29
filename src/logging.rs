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
