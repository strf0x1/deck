//! Wait for one descriptor to become readable, with a timeout.
//!
//! The design choice: a nonblocking accept loop polls here instead of
//! sleeping for a fixed interval. The descriptor wakes it the moment a peer
//! connects, and the timeout still bounds every shutdown check, so a caller
//! keeps its idle cadence without adding latency to an attach or a control
//! request.

use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::time::Duration;

/// `true` when `fd` is readable, `false` on timeout or an interrupted wait.
/// The caller rechecks its own stop flag and waits again.
pub(crate) fn readable(fd: RawFd, within: Duration) -> io::Result<bool> {
    let mut descriptor = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let millis = i32::try_from(within.as_millis().max(1)).unwrap_or(i32::MAX);
    // SAFETY: one initialised descriptor referring to a live file description
    // borrowed for this call; poll takes no pointer into it on return.
    let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
    match ready {
        0 => Ok(false),
        n if n > 0 => Ok(true),
        _ => {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted { Ok(false) } else { Err(error) }
        }
    }
}

/// Wait until either the listener or its shutdown signal becomes readable.
pub(crate) fn readable_or_shutdown(listener: RawFd, shutdown: RawFd) -> io::Result<()> {
    let mut fds = [
        libc::pollfd { fd: listener, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: shutdown, events: libc::POLLIN, revents: 0 },
    ];
    loop {
        // SAFETY: the descriptors are borrowed and the array stays live.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ready >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Subscribe before checking liveness so an exit cannot slip between the
/// check and registration. Failure leaves the caller's timed fallback intact.
#[cfg(target_os = "macos")]
pub(crate) fn process_exit(pid: u32) -> Option<OwnedFd> {
    // SAFETY: kqueue returns a new owned descriptor on success.
    let fd = unsafe { libc::kqueue() };
    if fd < 0 {
        return None;
    }
    // SAFETY: fd is a newly created descriptor and is now owned here.
    let queue = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: fcntl only changes this descriptor's close-on-exec flag.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return None;
    }
    let event = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: event is a valid one-element changelist; no events are read.
    if unsafe { libc::kevent(fd, &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
        return None;
    }
    Some(queue)
}

#[cfg(target_os = "linux")]
pub(crate) fn process_exit(pid: u32) -> Option<OwnedFd> {
    // SAFETY: pidfd_open takes scalar arguments and returns a new descriptor.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        None
    } else {
        // SAFETY: pidfd_open returned a new owned descriptor.
        Some(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn process_exit(_pid: u32) -> Option<OwnedFd> {
    None
}

/// Wait for exit or a fallback check. Discard a fired notification because a
/// process group or task lock can outlive its leader by a short time.
pub(crate) fn exit_or_timeout(notification: &mut Option<OwnedFd>) {
    if let Some(event) = notification {
        if !matches!(readable(event.as_raw_fd(), Duration::from_secs(1)), Ok(false)) {
            *notification = None;
        }
    } else {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::net::{UnixListener, UnixStream};

    #[test]
    fn readable_data_wakes_the_wait_and_a_quiet_descriptor_times_out() {
        let (read, write) = UnixStream::pair().unwrap();
        assert!(!readable(read.as_raw_fd(), Duration::from_millis(1)).unwrap());
        (&write).write_all(b"x").unwrap();
        assert!(readable(read.as_raw_fd(), Duration::from_secs(3)).unwrap());
    }

    #[test]
    fn a_connection_wakes_a_listener_wait() {
        let path = std::env::temp_dir().join(format!("deck-wait-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let connect = path.clone();
        let peer = std::thread::spawn(move || UnixStream::connect(&connect).unwrap());
        assert!(readable(listener.as_raw_fd(), Duration::from_secs(3)).unwrap());
        let _ = peer.join();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn a_process_exit_wakes_its_subscriber() {
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let Some(event) = process_exit(child.id()) else {
            child.kill().unwrap();
            child.wait().unwrap();
            eprintln!("process exit notifications are unavailable; the watcher uses its fallback");
            return;
        };
        assert!(!readable(event.as_raw_fd(), Duration::from_millis(10)).unwrap());
        child.kill().unwrap();
        assert!(readable(event.as_raw_fd(), Duration::from_secs(2)).unwrap());
        child.wait().unwrap();
    }
}
