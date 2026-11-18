//! `deckd`'s notice stream, once ([`DaemonClient::subscribe`]) or followed
//! across daemon restarts ([`DaemonClient::follow`], deck#199).
//!
//! The design choice: notices are hints and the records are the truth, so a
//! followed stream does not replay what it missed while `deckd` was gone. It
//! reconnects with backoff and says [`Followed::Resubscribed`], and the
//! reader catches up by reading the records again. A follower never starts
//! `deckd`: whatever brings the daemon back (the window's next request, a
//! watch, the user) also brings the stream back.

use super::DaemonClient;
use crate::Notice;
use crate::paths::Paths;
use crate::wire::{self, Request};
use anyhow::{Context as _, Result};
use futures::channel::mpsc;
use std::io::{BufReader, Read as _};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// The first wait before a reconnect, doubled up to [`BACKOFF_MAX`].
const BACKOFF: Duration = Duration::from_millis(50);
const BACKOFF_MAX: Duration = Duration::from_secs(2);
/// How often a blocked read checks whether the receiver was dropped.
const POLL: Duration = Duration::from_millis(100);

/// What a followed stream yields.
#[derive(Debug)]
pub enum Followed {
    Notice(Box<Notice>),
    /// The stream reconnected after `deckd` went away; notices sent in
    /// between are lost, so read the records again.
    Resubscribed,
}

/// Why [`pump`] returned.
enum Ended {
    /// The daemon closed the stream or the socket failed.
    Daemon,
    /// The receiver was dropped, or the daemon sent something unreadable.
    Done,
}

impl DaemonClient {
    /// The notice stream on a connection of its own, until `deckd` closes it.
    pub fn subscribe(&self) -> Result<mpsc::UnboundedReceiver<Notice>> {
        let input = open(&self.paths)?;
        let (tx, rx) = mpsc::unbounded();
        std::thread::spawn(move || pump(input, &tx, |notice| notice));
        Ok(rx)
    }

    /// The notice stream for as long as the receiver lives: after `deckd`
    /// exits, restarts or crashes it reconnects and yields
    /// [`Followed::Resubscribed`]. Fails only when no daemon answers now.
    pub fn follow(&self) -> Result<mpsc::UnboundedReceiver<Followed>> {
        let first = open(&self.paths)?;
        let paths = self.paths.clone();
        let (tx, rx) = mpsc::unbounded();
        std::thread::Builder::new()
            .name("deck-notices".into())
            .spawn(move || {
                let mut input = first;
                loop {
                    if let Ended::Done = pump(input, &tx, |notice| Followed::Notice(Box::new(notice))) {
                        return;
                    }
                    let Some(again) = reconnect(&paths, &tx) else { return };
                    input = again;
                    if tx.unbounded_send(Followed::Resubscribed).is_err() {
                        return;
                    }
                }
            })
            .context("starting the notice thread")?;
        Ok(rx)
    }
}

/// Connect and subscribe, with a short read timeout so a reader can notice
/// its receiver going away.
fn open(paths: &Paths) -> Result<BufReader<UnixStream>> {
    let socket = UnixStream::connect(paths.control())?;
    socket.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut input = BufReader::new(socket);
    wire::write(input.get_mut(), &Request::Subscribe)?;
    let reply = wire::read(&mut input)?.context("deckd closed subscription")?;
    super::decode::<()>(&reply)?;
    input.get_mut().set_read_timeout(Some(POLL))?;
    Ok(input)
}

/// Retry [`open`] with backoff until it works, or `None` once the receiver
/// is dropped.
fn reconnect<T>(paths: &Paths, tx: &mpsc::UnboundedSender<T>) -> Option<BufReader<UnixStream>> {
    let mut wait = BACKOFF;
    loop {
        // Sleep in steps, so a dropped receiver ends the thread promptly.
        let mut slept = Duration::ZERO;
        while slept < wait {
            if tx.is_closed() {
                return None;
            }
            std::thread::sleep(POLL.min(wait - slept));
            slept += POLL;
        }
        if let Ok(input) = open(paths) {
            return Some(input);
        }
        wait = (wait * 2).min(BACKOFF_MAX);
    }
}

/// Forward each notice line until the stream ends.
fn pump<T>(mut input: BufReader<UnixStream>, tx: &mpsc::UnboundedSender<T>, wrap: fn(Notice) -> T) -> Ended {
    // BufRead framing retains a partial notice across timed-out reads.
    let mut pending = Vec::new();
    while !tx.is_closed() {
        let mut bytes = [0; 4096];
        match input.read(&mut bytes) {
            Ok(0) => return Ended::Daemon,
            Ok(n) => {
                pending.extend_from_slice(&bytes[..n]);
                while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<_> = pending.drain(..=end).collect();
                    let Ok(notice) = serde_json::from_slice(&line) else { return Ended::Done };
                    if tx.unbounded_send(wrap(notice)).is_err() {
                        return Ended::Done;
                    }
                }
                if pending.len() > wire::LIMIT {
                    return Ended::Done;
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(_) => return Ended::Daemon,
        }
    }
    Ended::Done
}
