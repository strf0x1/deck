//! CONNECT header reads with a total deadline, including dispatch time.
//! Recompute the socket timeout before each read so trickled bytes cannot
//! renew the budget; the established tunnel has its own idle timeout.

use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

pub(crate) const TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct Reader {
    stream: UnixStream,
    deadline: Instant,
}

impl Reader {
    pub(super) fn new(stream: UnixStream, deadline: Instant) -> Self {
        Self { stream, deadline }
    }
}

impl Read for Reader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "CONNECT header deadline expired"));
        }
        self.stream.set_read_timeout(Some(left))?;
        self.stream.read(buffer)
    }
}
