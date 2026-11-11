//! A JSON-agnostic holder for one agent stream. Each peer owns its input
//! fragment; stdout is always drained and complete lines go only to the peer
//! attached when they finish. A slow peer is disconnected, never backpressuring
//! the agent's stdout. This module can move unchanged to B2's holder.

use crate::Stream;
use anyhow::Result;
use std::io::{BufRead, BufReader, Write as _};
use std::net::Shutdown;
use std::os::fd::AsRawFd as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

/// Maximum bytes in a whole line. Oversized lines are drained and dropped;
/// their attachment is disconnected so partial messages never reach a peer.
pub const LINE_LIMIT: usize = 1024 * 1024;
const QUEUE: usize = 8;
const POLL: Duration = Duration::from_millis(10);

type Current = Arc<Mutex<Option<Arc<Peer>>>>;
struct Peer {
    socket: UnixStream,
    output: mpsc::SyncSender<Option<Vec<u8>>>,
}

pub struct Relay {
    path: PathBuf,
    done: Arc<AtomicBool>,
    current: Current,
    writer: Arc<Mutex<Box<dyn deck_acp::Writer>>>,
}

impl Relay {
    pub fn serve(path: &Path, stream: Stream) -> Result<Self> {
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        let done = Arc::new(AtomicBool::new(false));
        let current: Current = Arc::new(Mutex::new(None));
        let output_peer = current.clone();
        std::thread::Builder::new().name("holder-output".into()).spawn(move || {
            let _ = lines(stream.reader, |line| {
                let peer = output_peer.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if let Some(peer) = peer {
                    let sent = line.is_some_and(|line| enqueue(&peer, Some(line)));
                    if !sent {
                        detach(&output_peer, &peer);
                    }
                }
            });
            let peer = output_peer.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(peer) = peer
                && !enqueue(&peer, None)
            {
                detach(&output_peer, &peer);
            }
        })?;
        let input_peer = current.clone();
        let quit = done.clone();
        let writer = Arc::new(Mutex::new(stream.writer));
        let accept_writer = writer.clone();
        std::thread::Builder::new().name("holder-accept".into()).spawn(move || {
            while !quit.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        if socket.set_nonblocking(false).is_err() {
                            continue;
                        }
                        let slot = input_peer.lock().unwrap_or_else(|e| e.into_inner());
                        if slot.is_some() {
                            let _ = socket.shutdown(Shutdown::Both);
                            continue;
                        }
                        let _ = serve_peer(socket, slot, &input_peer, &accept_writer);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // Wake on the peer's connection, not on a poll tick:
                        // the window attaches immediately after start.
                        if crate::wait::readable(listener.as_raw_fd(), POLL).is_err() {
                            break;
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionAborted
                        ) => {}
                    Err(_) => break,
                }
            }
        })?;
        Ok(Self { path: path.to_owned(), done, current, writer })
    }
    /// Reserve the transport for runtime shutdown, evicting the window. No
    /// listener peer can enter until this private stream is dropped.
    pub(crate) fn take_over(&self) -> Result<UnixStream> {
        let (client, held) = UnixStream::pair()?;
        // Never wait for a blocked agent stdin here. Input threads serialize
        // writes and reject queued old-peer lines after this identity swap.
        let slot = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(peer) = slot.as_ref() {
            let _ = peer.socket.shutdown(Shutdown::Both);
        }
        serve_peer(held, slot, &self.current, &self.writer)?;
        Ok(client)
    }
}

fn serve_peer(
    socket: UnixStream,
    mut slot: std::sync::MutexGuard<'_, Option<Arc<Peer>>>,
    current: &Current,
    writer: &Arc<Mutex<Box<dyn deck_acp::Writer>>>,
) -> Result<()> {
    let read = socket.try_clone()?;
    let mut write = socket.try_clone()?;
    let _ = write.set_write_timeout(Some(Duration::from_millis(200)));
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    let peer = Arc::new(Peer { socket, output: tx });
    *slot = Some(peer.clone());
    drop(slot);
    let output_current = current.clone();
    let output_identity = peer.clone();
    std::thread::spawn(move || {
        loop {
            let active = output_current
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|p| Arc::ptr_eq(p, &output_identity));
            if !active {
                break;
            }
            match rx.recv_timeout(POLL) {
                Ok(Some(line)) => {
                    if write.write_all(&line).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        detach(&output_current, &output_identity);
    });
    let input_current = current.clone();
    let writer = writer.clone();
    std::thread::spawn(move || {
        let mut failed = false;
        let _ = lines(BufReader::new(read), |line| {
            if failed {
                return;
            }
            let Some(line) = line else {
                detach(&input_current, &peer);
                failed = true;
                return;
            };
            let mut out = writer.lock().unwrap_or_else(|e| e.into_inner());
            // Serialize the identity check with agent writes. A line
            // accepted before detach finishes before the next peer's
            // initialize; buffered old-peer lines are discarded.
            let active =
                input_current.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|p| Arc::ptr_eq(p, &peer));
            if !active {
                failed = true;
                return;
            }
            if out.write_all(&line).and_then(|_| out.flush()).is_err() {
                detach(&input_current, &peer);
                failed = true;
            }
        });
        detach(&input_current, &peer);
    });
    Ok(())
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        let peer = self.current.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(peer) = peer {
            detach(&self.current, &peer);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

// Give a healthy writer time to run during replay bursts. The finite grace
// still disconnects a non-reading peer and resumes draining stdout.
fn enqueue(peer: &Peer, mut line: Option<Vec<u8>>) -> bool {
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        match peer.output.try_send(line) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Full(pending)) if Instant::now() < deadline => {
                line = pending;
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return false,
        }
    }
}

fn detach(current: &Current, peer: &Arc<Peer>) {
    let _ = peer.socket.shutdown(Shutdown::Both);
    let mut slot = current.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|p| Arc::ptr_eq(p, peer)) {
        *slot = None;
    }
}

/// Bounded framing for arbitrary bytes. EOF discards an incomplete line.
fn lines(mut reader: impl BufRead, mut emit: impl FnMut(Option<Vec<u8>>)) -> std::io::Result<()> {
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let part = reader.fill_buf()?;
        if part.is_empty() {
            return Ok(());
        }
        let n = part.iter().position(|&b| b == b'\n').map_or(part.len(), |i| i + 1);
        let complete = part[n - 1] == b'\n';
        if !oversized {
            if line.len() + n <= LINE_LIMIT {
                line.extend_from_slice(&part[..n]);
            } else {
                line.clear();
                oversized = true;
            }
        }
        reader.consume(n);
        if complete {
            emit(if oversized { None } else { Some(std::mem::take(&mut line)) });
            oversized = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::sync::atomic::AtomicUsize;
    static N: AtomicUsize = AtomicUsize::new(0);

    fn relay() -> (Relay, UnixStream, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "deck-relay-{}-{}.sock",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let (agent, held) = UnixStream::pair().unwrap();
        agent.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let stream = Stream { reader: Box::new(BufReader::new(held.try_clone().unwrap())), writer: Box::new(held) };
        (Relay::serve(&path, stream).unwrap(), agent, path)
    }

    fn connect(path: &Path) -> UnixStream {
        let s = UnixStream::connect(path).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        s
    }

    #[test]
    fn a_detach_discards_input_fragment_and_refuses_a_simultaneous_peer() {
        let (_relay, mut agent, path) = relay();
        let mut first = connect(&path);
        first.write_all(b"whole\nfragment").unwrap();
        let mut bytes = [0; 6];
        agent.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"whole\n");
        let mut second = connect(&path);
        assert_eq!(second.read(&mut bytes).unwrap(), 0, "one active client only");
        first.shutdown(Shutdown::Both).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let mut next = connect(&path);
        next.write_all(b"next\n").unwrap();
        let mut bytes = [0; 5];
        agent.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"next\n", "old fragment never reaches agent");
    }

    #[test]
    fn detached_output_is_drained_bounded_and_not_replayed() {
        let (_relay, mut agent, path) = relay();
        agent.write_all(&vec![b'x'; LINE_LIMIT * 3]).unwrap();
        agent.write_all(b"\ndiscarded\n").unwrap();
        // Synchronize stdout draining with a marker returned on a temporary attachment.
        std::thread::sleep(Duration::from_millis(50));
        let mut peer = connect(&path);
        peer.write_all(b"attached\n").unwrap();
        let mut input = [0; 9];
        agent.read_exact(&mut input).unwrap();
        agent.write_all(b"not JSON\xff\n").unwrap();
        let mut output = [0; 10];
        peer.read_exact(&mut output).unwrap();
        assert_eq!(&output, b"not JSON\xff\n");
    }

    #[test]
    fn an_agent_fragment_goes_to_the_peer_attached_when_the_line_ends() {
        let (_relay, mut agent, path) = relay();
        let mut first = connect(&path);
        first.write_all(b"first\n").unwrap();
        let mut input = [0; 6];
        agent.read_exact(&mut input).unwrap();
        agent.write_all(b"partial ").unwrap();
        first.shutdown(Shutdown::Both).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let mut next = connect(&path);
        next.write_all(b"second\n").unwrap();
        let mut input = [0; 7];
        agent.read_exact(&mut input).unwrap();
        agent.write_all(b"line\n").unwrap();
        let mut output = [0; 13];
        next.read_exact(&mut output).unwrap();
        assert_eq!(&output, b"partial line\n");
    }

    #[test]
    fn a_nonreading_peer_cannot_stop_the_agent_output_drain() {
        let (_relay, mut agent, path) = relay();
        let mut slow = connect(&path);
        slow.write_all(b"attached\n").unwrap();
        let mut input = [0; 9];
        agent.read_exact(&mut input).unwrap();
        let (done, observed) = mpsc::channel();
        let mut output = agent.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut line = vec![b'x'; LINE_LIMIT];
            line[LINE_LIMIT - 1] = b'\n';
            for _ in 0..32 {
                output.write_all(&line).unwrap();
            }
            done.send(()).unwrap();
        });
        observed.recv_timeout(Duration::from_secs(5)).expect("stdout kept draining despite a nonreading client");
        // The old peer may still have kernel-buffered output, but it has
        // lost ownership: a new peer can now send through the same holder.
        let mut next = connect(&path);
        next.write_all(b"next\n").unwrap();
        let mut input = [0; 5];
        agent.read_exact(&mut input).unwrap();
        assert_eq!(&input, b"next\n");
    }

    #[test]
    fn complete_output_is_flushed_before_agent_eof() {
        let (_relay, mut agent, path) = relay();
        let mut peer = connect(&path);
        peer.write_all(b"attached\n").unwrap();
        let mut input = [0; 9];
        agent.read_exact(&mut input).unwrap();
        agent.write_all(b"final reply\n").unwrap();
        agent.shutdown(Shutdown::Write).unwrap();
        let mut output = Vec::new();
        peer.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"final reply\n");
    }

    #[test]
    fn shutdown_takeover_evicts_the_window_and_reserves_the_stream() {
        let (relay, mut agent, path) = relay();
        let mut window = connect(&path);
        window.write_all(b"ready\n").unwrap();
        let mut ready = [0; 6];
        agent.read_exact(&mut ready).unwrap();
        let mut closing = relay.take_over().unwrap();
        let mut bytes = [0; 1];
        assert_eq!(window.read(&mut bytes).unwrap(), 0);
        let mut intruder = connect(&path);
        assert_eq!(intruder.read(&mut bytes).unwrap(), 0);
        closing.write_all(b"close\n").unwrap();
        agent.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"close\n");
        agent.write_all(b"done\n").unwrap();
        let mut done = [0; 5];
        closing.read_exact(&mut done).unwrap();
        assert_eq!(&done, b"done\n");
    }

    #[test]
    fn framing_is_bounded_and_keeps_only_complete_lines() {
        let mut data = vec![b'a'; LINE_LIMIT + 1];
        data.extend_from_slice(b"\nok\npartial");
        let mut output = vec![];
        lines(BufReader::with_capacity(7, data.as_slice()), |line| output.push(line)).unwrap();
        assert_eq!(output, vec![None, Some(b"ok\n".to_vec())]);
    }
}
