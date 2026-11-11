//! Wait for one descriptor to become readable, with a timeout.
//!
//! The design choice: a nonblocking accept loop polls here instead of
//! sleeping for a fixed interval. The descriptor wakes it the moment a peer
//! connects, and the timeout still bounds every shutdown check, so a caller
//! keeps its idle cadence without adding latency to an attach or a control
//! request.

use std::io;
use std::os::fd::RawFd;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::fd::AsRawFd as _;
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
}
