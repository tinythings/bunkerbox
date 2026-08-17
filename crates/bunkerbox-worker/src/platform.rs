use std::ffi::{CStr, CString, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::OpenOptionsExt;

pub const OPEN_DIRECTORY_FLAGS: i32 = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
pub const OPEN_FILE_FLAGS: i32 = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

pub fn open_root(path: &std::path::Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_DIRECTORY_FLAGS & !libc::O_RDONLY)
        .open(path)
        .map_err(|error| format!("open worker workspace root: {error}"))?;
    validate_private_directory(&file, "worker workspace root")?;
    Ok(file)
}

pub fn validate_private_directory(file: &File, label: &str) -> Result<(), String> {
    let metadata = stat_fd(file.as_raw_fd()).map_err(|error| format!("stat {label}: {error}"))?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(format!("{label} is not a directory"));
    }
    if metadata.st_uid != unsafe { libc::geteuid() } {
        return Err(format!("{label} is not owned by the worker account"));
    }
    if metadata.st_mode & 0o077 != 0 {
        return Err(format!("{label} is not private"));
    }
    Ok(())
}

pub fn open_dir_at(parent: &File, name: &str) -> io::Result<File> {
    open_at(parent.as_raw_fd(), name, OPEN_DIRECTORY_FLAGS)
}

pub fn open_file_at(parent: &File, name: &str) -> io::Result<File> {
    open_at(parent.as_raw_fd(), name, OPEN_FILE_FLAGS)
}

pub fn open_lock_at(parent: &File, name: &str) -> io::Result<File> {
    open_at(parent.as_raw_fd(), name, libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC)
}

pub fn open_at(parent: RawFd, name: &str, flags: i32) -> io::Result<File> {
    let name = c_string(name)?;
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub fn create_file_at(parent: &File, name: &str, mode: u32) -> io::Result<File> {
    let name = c_string(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::mode_t,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub fn create_dir_at(parent: &File, name: &str, mode: u32) -> io::Result<()> {
    let name = c_string(name)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn chmod_fd(file: &File, mode: u32) -> io::Result<()> {
    if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn sync_fd(file: &File) -> io::Result<()> {
    if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn stat_fd(fd: RawFd) -> io::Result<libc::stat> {
    let mut metadata = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut metadata) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(metadata)
}

pub fn stat_at(parent: &File, name: &str) -> io::Result<libc::stat> {
    let name = c_string(name)?;
    let mut metadata = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstatat(parent.as_raw_fd(), name.as_ptr(), &mut metadata, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(metadata)
}

pub fn unlink_at(parent: &File, name: &str, flags: i32) -> io::Result<()> {
    let name = c_string(name)?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn remove_tree_at(parent: &File, name: &str) -> io::Result<()> {
    let child = match open_dir_at(parent, name) {
        Ok(child) => Some(child),
        Err(error) if error.kind() == io::ErrorKind::NotADirectory || error.raw_os_error() == Some(libc::ELOOP) => None,
        Err(error) => return Err(error),
    };
    if let Some(child) = child {
        for entry in list_names(&child)? {
            let metadata = stat_at(&child, &entry)?;
            if metadata.st_mode & libc::S_IFMT == libc::S_IFDIR {
                remove_tree_at(&child, &entry)?;
            } else {
                unlink_at(&child, &entry, 0)?;
            }
        }
        unlink_at(parent, name, libc::AT_REMOVEDIR)
    } else {
        unlink_at(parent, name, 0)
    }
}

pub fn list_names(directory: &File) -> io::Result<Vec<String>> {
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(io::Error::last_os_error());
    }
    unsafe { libc::rewinddir(stream) };
    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            let name =
                OsString::from_vec(name.to_vec()).into_string().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 state name"))?;
            names.push(name);
        }
    }
    unsafe { libc::closedir(stream) };
    names.sort();
    Ok(names)
}

pub fn lock_exclusive(file: &File) -> io::Result<bool> {
    let mut lock = unsafe { std::mem::zeroed::<libc::flock>() };
    lock.l_type = libc::F_WRLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(code) if code == libc::EACCES || code == libc::EAGAIN) {
        Ok(false)
    } else {
        Err(error)
    }
}

pub fn set_nonblocking_fd(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn change_directory(fd: RawFd) -> io::Result<()> {
    if unsafe { libc::fchdir(fd) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn set_process_group() -> io::Result<()> {
    if unsafe { libc::setpgid(0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn signal_process_group(pgid: libc::pid_t, signal: libc::c_int) {
    if pgid > 0 {
        unsafe {
            libc::kill(-pgid, signal);
        }
    }
}

fn c_string(value: &str) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in worker path"))
}

#[cfg(test)]
#[path = "platform_ut.rs"]
mod tests;
