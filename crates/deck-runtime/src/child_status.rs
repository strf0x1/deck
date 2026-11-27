//! The child supervisor's private exit report. A separate socket preserves
//! the agent's status before group cleanup kills the supervisor itself;
//! nothing is added to the ACP stream or inherited by the agent.

use anyhow::{Context as _, Result};
use std::fs::File;
use std::io::Read as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::process::Command;
use std::time::Duration;

pub(crate) const FD_ENV: &str = "DECK_CHILD_STATUS_FD";

/// Set up one report channel. Only the helper's exec inherits the writer.
pub(crate) fn channel(command: &mut Command) -> Result<(UnixStream, UnixStream)> {
    let (reader, writer) = UnixStream::pair()?;
    reader.set_read_timeout(Some(Duration::from_millis(100)))?;
    let fd = writer.as_raw_fd();
    command.env(FD_ENV, fd.to_string());
    // SAFETY: fcntl is async-signal-safe. The writer stays open through spawn,
    // and CLOEXEC changes only in this child, never in concurrent launches.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok((reader, writer))
}

/// Consume the descriptor our parent supplied, protecting it from agent exec.
pub(crate) fn writer() -> Result<File> {
    let fd: i32 = std::env::var(FD_ENV).context("missing child status descriptor")?.parse()?;
    anyhow::ensure!(fd >= 3, "invalid child status descriptor");
    // SAFETY: this internal entry point receives one owned descriptor from
    // channel(). fcntl verifies it before ownership is transferred to File.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error()).context("protecting child status descriptor");
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Called after the helper exits. Missing reports retain the helper's status,
/// including a forced kill after the stop grace or a holder crash.
pub(crate) fn read(reader: UnixStream) -> Option<String> {
    let mut reason = String::new();
    reader.take(1024).read_to_string(&mut reason).ok()?;
    (!reason.is_empty()).then_some(reason)
}
