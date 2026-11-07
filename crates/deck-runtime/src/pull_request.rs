//! Live publication state: what `deckd` reads about a task's branch and pull
//! request through the task's own GitHub gateway socket (ADR 0008 §8).
//!
//! The design choice: this path can only read. The one request type it can
//! build is [`Read`], whose two variants are `refs` and `review`, so no write
//! operation is reachable from here however a window asks. Reads are pulled,
//! never scheduled: a window asks while it shows the task, and the reader
//! answers from its last read until the task's interval has passed, keeps one
//! read in flight per task, and backs off when GitHub or the gateway fails.
//! A stopped task answers with the recorded publication and the last read,
//! which is persisted beside the task's record with its time.

use crate::lifecycle::Publication;
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The shortest wait between two reads of one task, and the longest backoff.
pub const INTERVAL: Duration = Duration::from_secs(15);
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// An explicit refresh may come sooner than the interval, but not this soon.
pub const MIN_REFRESH: Duration = Duration::from_secs(2);
/// One gateway read; the gateway's own GitHub calls are bounded inside this.
const TIMEOUT: Duration = Duration::from_secs(30);
/// The most of a reply this path reads.
const MAX_REPLY: u64 = 256 * 1024;
const FILE: &str = "pull_request.json";

/// The only operations this path sends (contract 1.2.0 §11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Read {
    Refs,
    Review(u64),
}

impl Read {
    pub fn path(self) -> &'static str {
        match self {
            Self::Refs => "/operations/refs",
            Self::Review(_) => "/operations/pull_request/review",
        }
    }

    pub fn body(self) -> Value {
        match self {
            Self::Refs => json!({ "operation": "refs" }),
            Self::Review(number) => json!({ "operation": "review", "number": number }),
        }
    }
}

/// One read's result, as a window shows it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Live {
    /// When this was read, Unix milliseconds.
    pub read_at_ms: u64,
    /// The branch's commit on GitHub, for a task that pushed but opened no
    /// pull request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_commit: Option<String>,
    /// The pull request's state and checks (contract `github-review-response`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<Value>,
    /// Why the latest read failed; the other fields are from the last success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What a window gets: the recorded publication, the last read, and whether
/// the read is from now or from before (the task stopped, or its interval has
/// not passed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub publication: Option<Publication>,
    pub live: Option<Live>,
    pub fresh: bool,
}

#[derive(Default)]
struct Entry {
    last: Option<Live>,
    /// When the last read finished, and when the next one is due.
    read: Option<Instant>,
    next: Option<Instant>,
    backoff: Duration,
    in_flight: bool,
}

#[derive(Default)]
pub struct Reader {
    entries: Mutex<HashMap<String, Entry>>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl Reader {
    /// The state for task `id`. `dir` is its record directory; `socket` is its
    /// gateway socket while it runs, `None` otherwise.
    pub fn state(
        &self,
        id: &str,
        dir: &Path,
        publication: Option<Publication>,
        socket: Option<PathBuf>,
        refresh: bool,
    ) -> State {
        let read = publication.as_ref().map(|p| match p {
            Publication::PullRequest { number, .. } => Read::Review(*number),
            Publication::Branch { .. } => Read::Refs,
        });
        let now = Instant::now();
        let (cached, due) = {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            let entry = entries.entry(id.to_string()).or_default();
            if entry.last.is_none() {
                entry.last = load(dir);
            }
            // A refresh skips the interval but never a backoff, and never
            // comes within MIN_REFRESH of the last read.
            let early = refresh && entry.backoff.is_zero() && entry.read.is_none_or(|at| now >= at + MIN_REFRESH);
            let due = read.is_some()
                && socket.is_some()
                && !entry.in_flight
                && (early || entry.next.is_none_or(|next| now >= next));
            if due {
                entry.in_flight = true;
            }
            (entry.last.clone(), due)
        };
        let (Some(read), Some(socket), true) = (read, socket, due) else {
            return State { publication, live: cached, fresh: false };
        };
        let outcome = fetch(&socket, read);
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.entry(id.to_string()).or_default();
        entry.in_flight = false;
        entry.read = Some(Instant::now());
        let mut live = entry.last.clone().unwrap_or_default();
        live.read_at_ms = now_ms();
        match outcome {
            Ok(reply) => {
                live.error = None;
                match (read, &publication) {
                    (Read::Review(_), _) => live.pull_request = Some(reply),
                    (Read::Refs, Some(Publication::Branch { branch, .. })) => {
                        live.branch_commit = reply["branches"]
                            .as_array()
                            .and_then(|all| all.iter().find(|b| b["branch"] == branch.as_str()))
                            .and_then(|b| b["commit"].as_str())
                            .map(str::to_string);
                    }
                    (Read::Refs, _) => {}
                }
                entry.backoff = Duration::ZERO;
                entry.next = Some(Instant::now() + INTERVAL);
            }
            Err(error) => {
                live.error = Some(format!("{error:#}"));
                entry.backoff = (entry.backoff * 2).clamp(INTERVAL, MAX_BACKOFF);
                entry.next = Some(Instant::now() + entry.backoff);
            }
        }
        if let Err(error) = save(dir, &live) {
            eprintln!("deckd: saving {id}'s pull request state: {error:#}");
        }
        entry.last = Some(live.clone());
        State { publication, live: Some(live), fresh: true }
    }
}

fn load(dir: &Path) -> Option<Live> {
    serde_json::from_slice(&std::fs::read(dir.join(FILE)).ok()?).ok()
}

fn save(dir: &Path, live: &Live) -> Result<()> {
    let temporary = dir.join(format!("{FILE}.tmp"));
    std::fs::write(&temporary, serde_json::to_vec(live)?)?;
    std::fs::rename(&temporary, dir.join(FILE))?;
    Ok(())
}

/// One read over the gateway's Unix socket: a plain HTTP/1.1 POST, one
/// request per connection, as the guest's relay sends it.
pub fn fetch(socket: &Path, read: Read) -> Result<Value> {
    let mut stream = UnixStream::connect(socket).context("connecting to the task's GitHub gateway")?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let body = read.body().to_string();
    let request = format!(
        "POST {} HTTP/1.1\r\nhost: gateway\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        read.path(),
        body.len()
    );
    stream.write_all(request.as_bytes()).context("sending the read to the task's GitHub gateway")?;
    let mut reply = Vec::new();
    stream.take(MAX_REPLY).read_to_end(&mut reply).context("reading the gateway's reply")?;
    let text = String::from_utf8_lossy(&reply);
    let (head, body) = text.split_once("\r\n\r\n").context("the gateway's reply has no body")?;
    let status: u16 =
        head.split(' ').nth(1).and_then(|s| s.parse().ok()).context("the gateway's reply has no status")?;
    let value: Value = serde_json::from_str(body).context("the gateway's reply is not JSON")?;
    if status != 200 {
        let code = value.pointer("/error/code").and_then(Value::as_str).unwrap_or("unknown");
        let message = value.pointer("/error/message").and_then(Value::as_str).unwrap_or_default();
        bail!("{code} ({status}): {message}");
    }
    Ok(value)
}

#[cfg(test)]
#[path = "pull_request_tests.rs"]
mod tests;
