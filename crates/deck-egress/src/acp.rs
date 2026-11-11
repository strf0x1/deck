//! The guest's ACP transport: open the stream out to the host first, then
//! make it the agent's stdin and stdout and exec the agent.
//!
//! The design choice: the connection is the readiness signal, so it is made
//! before the agent exists as a process. The host may already have sent
//! `initialize`; those bytes wait in the socket while the agent starts.
//! The agent's stderr stays on the guest console, never the ACP stream.

use crate::vsock::{self, Address};
use std::io;
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::process::Command;

/// Connect, wire the stream to fd 0 and 1, and become `command`.
pub fn exec(address: Address, command: &[String]) -> io::Result<()> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no agent command after --"))?;
    let stream = vsock::connect(address)?;
    let fd = stream.as_raw_fd();
    // If the socket landed on fd 0 (a closed stdin), clear close-on-exec so
    // it is still stdin after exec; fd 0 cannot be duplicated onto itself.
    if fd < 3 {
        // SAFETY: fcntl on an owned descriptor with a flags word.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: both targets are process descriptors; dup2 replaces them with
    // the connected socket and clears their close-on-exec flag.
    if unsafe { libc::dup2(fd, 0) } < 0 || unsafe { libc::dup2(fd, 1) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // A socket above fd 2 is SOCK_CLOEXEC and closes at exec. Keep a possible
    // fd 0/1 copy open: it is the agent's own stdio now.
    std::mem::forget(stream);
    Err(Command::new(program).args(args).exec())
}
