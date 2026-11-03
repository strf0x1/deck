//! The gateway's client for the daemon's approval queue (ADR 0004 §5).
//!
//! The design choice: a new scope is decided by `deckd`, never here. The
//! gateway sends what it observed and waits for the answer; the hold is the
//! daemon's (it knows whether a capable window is attached), so this client's
//! read timeout only has to outlast that hold. Any failure to reach `deckd`
//! is a denial, because the same failure makes a decision impossible.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Longer than the daemon's 20 s hold, so a held decision is not cut short.
const REPLY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub operation: String,
    pub destination: String,
    pub credential: Option<String>,
    pub method: Option<String>,
    pub path: Option<String>,
    pub size: Option<u64>,
}

/// The daemon's answer for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// A grant or a held decision allowed this request.
    Allowed,
    /// The item exists and is waiting for a window decision; retrying later
    /// (after a decision) is meaningful.
    Pending(String),
    /// A human denied it.
    Denied(String),
    /// `deckd` could not answer at all.
    Unavailable(String),
}

#[derive(Clone, Debug)]
pub struct Control {
    socket: PathBuf,
    task: String,
}

impl Control {
    pub fn new(socket: PathBuf, task: String) -> Self {
        Self { socket, task }
    }

    pub fn ask(&self, ask: &Ask) -> Answer {
        match self.call(ask) {
            Ok(approval) => answer(&approval),
            Err(error) => Answer::Unavailable(error),
        }
    }

    fn call(&self, ask: &Ask) -> Result<Value, String> {
        let mut stream =
            UnixStream::connect(&self.socket).map_err(|error| format!("the approval queue is unreachable: {error}"))?;
        stream.set_read_timeout(Some(REPLY)).map_err(|error| error.to_string())?;
        stream.set_write_timeout(Some(Duration::from_secs(5))).map_err(|error| error.to_string())?;
        let request = Request {
            op: "approval",
            task: &self.task,
            gateway: "model",
            operation: &ask.operation,
            destination: &ask.destination,
            credential: ask.credential.as_deref(),
            details: Details { method: ask.method.as_deref(), path: ask.path.as_deref(), size: ask.size },
        };
        let mut line = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
        line.push(b'\n');
        stream.write_all(&line).map_err(|error| format!("writing to the approval queue: {error}"))?;
        let line = read_line(&mut BufReader::new(stream))
            .map_err(|error| format!("reading from the approval queue: {error}"))?;
        let reply: Reply = serde_json::from_slice(&line).map_err(|error| error.to_string())?;
        match (reply.ok, reply.error) {
            (Some(value), _) => Ok(value),
            (None, Some(error)) => Err(error),
            (None, None) => Err("the approval queue replied with nothing".into()),
        }
    }
}

#[derive(Serialize)]
struct Request<'a> {
    op: &'static str,
    task: &'a str,
    gateway: &'static str,
    operation: &'a str,
    destination: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential: Option<&'a str>,
    details: Details<'a>,
}

#[derive(Serialize)]
struct Details<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    ok: Option<Value>,
    #[serde(default)]
    error: Option<String>,
}

/// One bounded reply line. The daemon writes exactly one JSON object.
fn read_line(reader: &mut impl std::io::BufRead) -> std::io::Result<Vec<u8>> {
    const LIMIT: usize = 1024 * 1024;
    let mut line = Vec::new();
    loop {
        let part = reader.fill_buf()?;
        if part.is_empty() {
            if line.is_empty() {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            return Ok(line);
        }
        let n = part.iter().position(|&b| b == b'\n').map_or(part.len(), |i| i + 1);
        if line.len() + n > LIMIT {
            return Err(std::io::Error::other("approval reply exceeds the line limit"));
        }
        line.extend_from_slice(&part[..n]);
        reader.consume(n);
        if line.last() == Some(&b'\n') {
            return Ok(line);
        }
    }
}

fn answer(approval: &Value) -> Answer {
    let id = approval.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    match approval.get("state") {
        Some(Value::String(state)) => match state.as_str() {
            "pending" => Answer::Pending(id),
            "denied" | "expired" | "withdrawn" => Answer::Denied(id),
            other => Answer::Unavailable(format!("the approval queue returned state {other:?}")),
        },
        Some(Value::Object(state)) if state.contains_key("allowed") => Answer::Allowed,
        _ => Answer::Unavailable("the approval queue returned no state".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_answers_are_distinguished() {
        assert_eq!(answer(&serde_json::json!({ "id": "a", "state": "pending" })), Answer::Pending("a".into()));
        assert_eq!(answer(&serde_json::json!({ "id": "a", "state": "denied" })), Answer::Denied("a".into()));
        assert_eq!(answer(&serde_json::json!({ "id": "a", "state": { "allowed": "once" } })), Answer::Allowed);
        assert!(matches!(answer(&serde_json::json!({})), Answer::Unavailable(_)));
    }
}
