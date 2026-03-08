//! Authentication utilities: peer PID retrieval and process tree walking.

use std::os::unix::io::RawFd;

use anyhow::{bail, Result};

/// Get peer PID from a Unix socket file descriptor.
#[cfg(target_os = "linux")]
pub fn get_peer_pid(fd: RawFd) -> Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if ret == 0 {
        Ok(cred.pid as u32)
    } else {
        bail!(
            "failed to get peer credentials: {}",
            std::io::Error::last_os_error()
        )
    }
}

/// Get peer PID from a Unix socket file descriptor.
#[cfg(target_os = "macos")]
pub fn get_peer_pid(fd: RawFd) -> Result<u32> {
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            &mut pid as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if ret == 0 {
        Ok(pid as u32)
    } else {
        bail!(
            "failed to get peer credentials: {}",
            std::io::Error::last_os_error()
        )
    }
}

/// Get the parent PID of a process.
/// Returns None if the process doesn't exist or we can't read its info.
#[cfg(target_os = "linux")]
pub fn get_parent_pid(pid: u32) -> Option<u32> {
    let stat_path = format!("/proc/{}/stat", pid);
    let content = std::fs::read_to_string(stat_path).ok()?;
    // Format: pid (comm) state ppid ...
    // Find the closing paren to handle commands with spaces/parens
    let after_comm = content.rfind(')')? + 1;
    let fields: Vec<&str> = content[after_comm..].split_whitespace().collect();
    // fields[0] is state, fields[1] is ppid
    fields.get(1)?.parse().ok()
}

/// Get the parent PID of a process.
/// Returns None if the process doesn't exist or we can't read its info.
#[cfg(target_os = "macos")]
pub fn get_parent_pid(pid: u32) -> Option<u32> {
    use std::mem::MaybeUninit;

    let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;

    let ret = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr() as *mut libc::c_void,
            size,
        )
    };

    if ret == size {
        let info = unsafe { info.assume_init() };
        Some(info.pbi_ppid)
    } else {
        None
    }
}

/// Get the current working directory of a process by PID.
#[cfg(target_os = "linux")]
pub fn get_process_cwd(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{}/cwd", pid))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Get the current working directory of a process by PID.
#[cfg(target_os = "macos")]
pub fn get_process_cwd(pid: u32) -> Option<String> {
    use std::mem::MaybeUninit;

    let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;

    let ret = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr() as *mut libc::c_void,
            size,
        )
    };

    if ret == size {
        let info = unsafe { info.assume_init() };
        let path_ptr = info.pvi_cdir.vip_path.as_ptr() as *const libc::c_char;
        let cstr = unsafe { std::ffi::CStr::from_ptr(path_ptr) };
        Some(cstr.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// Walk the PID tree from a starting PID, calling the callback for each ancestor.
/// Returns the first PID for which the callback returns Some(T).
/// Stops at PID 1 (init) or if parent lookup fails.
pub fn find_ancestor_pid<T, F>(start_pid: u32, mut f: F) -> Option<T>
where
    F: FnMut(u32) -> Option<T>,
{
    let mut current = start_pid;
    loop {
        if let Some(result) = f(current) {
            return Some(result);
        }
        if current <= 1 {
            return None;
        }
        match get_parent_pid(current) {
            Some(ppid) if ppid != current && ppid > 0 => current = ppid,
            _ => return None,
        }
    }
}
