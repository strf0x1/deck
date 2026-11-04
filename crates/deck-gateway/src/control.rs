//! The gateway's client for the daemon's approval queue (ADR 0004 §5).
//!
//! The design choice: a new scope is decided by `deckd`, never here. The
//! gateway sends what it observed and waits for the answer; the hold is the
//! daemon's (it knows whether a capable window is attached), so this client's
//! read timeout only has to outlast that hold. Any failure to reach `deckd`
//! is a denial, because the same failure makes a decision impossible. The
//! same client also reads an item, reports an open status wait for the
//! extension port's boundary kind (§6), and reports model usage totals after
//! a response was already delivered.

use crate::policy::Usage;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Longer than the daemon's 20 s hold, so a held decision is not cut short.
const REPLY: Duration = Duration::from_secs(30);
/// Usage is accounting after the response was delivered; a slow daemon must
/// not accumulate saturated reporter threads.
const USAGE_REPLY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub operation: String,
    pub destination: String,
    pub credential: Option<String>,
    pub method: Option<String>,
    pub path: Option<String>,
    pub size: Option<u64>,
}

/// One item as the daemon reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub id: String,
    pub destination: String,
    /// `pending`, `allowed`, `denied`, `expired` or `withdrawn`.
    pub state: String,
    /// `once`, `task` or `project` for an allowed item.
    pub scope: Option<String>,
}

/// The daemon's answer for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// A grant or a held decision allowed this request, with the granting
    /// scope (`once`, `task` or `project`).
    Allowed(String),
    /// The item exists and is waiting for a window decision; retrying later
    /// (after a decision) is meaningful.
    Pending(String),
    /// A human denied it, or it expired or was withdrawn; the second value is
    /// the state.
    Denied(String, String),
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

    pub fn socket(&self) -> &PathBuf {
        &self.socket
    }

    pub fn task(&self) -> &str {
        &self.task
    }

    pub fn ask(&self, ask: &Ask) -> Answer {
        let mut request = json!({
            "op": "approval",
            "task": self.task,
            "gateway": "model",
            "operation": ask.operation,
            "destination": ask.destination,
            "details": {},
        });
        if let Some(credential) = &ask.credential {
            request["credential"] = json!(credential);
        }
        let details = &mut request["details"];
        if let Some(method) = &ask.method {
            details["method"] = json!(method);
        }
        if let Some(path) = &ask.path {
            details["path"] = json!(path);
        }
        if let Some(size) = ask.size {
            details["size"] = json!(size);
        }
        match self.call(&request) {
            Ok(approval) => answer(&approval),
            Err(error) => Answer::Unavailable(error),
        }
    }

    /// One item's state, or `None` once the daemon has forgotten it.
    pub fn status(&self, id: &str) -> Result<Option<State>, String> {
        let request = json!({ "op": "approval_status", "task": self.task, "id": id });
        let value = self.call(&request)?;
        if value.is_null() {
            return Ok(None);
        }
        parse_state(&value).map(Some).ok_or_else(|| "the approval queue returned an item with no state".into())
    }

    /// Long-poll one item for a decision. The daemon owns the wait and caps
    /// it, so this is one bounded control call rather than a polling loop.
    pub fn wait(&self, id: &str, wait: Duration) -> Result<Option<State>, String> {
        let request = json!({
            "op": "approval_wait",
            "task": self.task,
            "id": id,
            "wait_ms": u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
        });
        let reply = self.call_within(&request, wait + Duration::from_secs(10))?;
        if reply.is_null() {
            return Ok(None);
        }
        parse_state(&reply).map(Some).ok_or_else(|| "the approval queue returned an item with no state".into())
    }

    /// Drop a pending item on the agent's behalf.
    pub fn withdraw(&self, id: &str) -> Result<(), String> {
        let request = json!({ "op": "withdraw_approval", "task": self.task, "id": id });
        self.call(&request).map(|_| ())
    }

    /// Report this task's model totals so `deckd` records them. The gateway
    /// owns the counters; the daemon only stores the latest totals.
    pub fn usage(&self, usage: Usage) -> Result<(), String> {
        let request = json!({
            "op": "model_usage",
            "task": self.task,
            "requests": usage.requests,
            "tokens": usage.tokens,
        });
        self.call_within(&request, USAGE_REPLY).map(|_| ())
    }

    fn call(&self, request: &Value) -> Result<Value, String> {
        self.call_within(request, REPLY)
    }

    /// One request and its one reply line. The credential broker shares it:
    /// it is the same socket and the same framing.
    pub(crate) fn call_within(&self, request: &Value, within: Duration) -> Result<Value, String> {
        let mut stream =
            UnixStream::connect(&self.socket).map_err(|error| format!("the approval queue is unreachable: {error}"))?;
        stream.set_read_timeout(Some(within)).map_err(|error| error.to_string())?;
        stream.set_write_timeout(Some(Duration::from_secs(5))).map_err(|error| error.to_string())?;
        let mut line = serde_json::to_vec(request).map_err(|error| error.to_string())?;
        line.push(b'\n');
        stream.write_all(&line).map_err(|error| format!("writing to the approval queue: {error}"))?;
        let line = read_line(&mut BufReader::new(stream))
            .map_err(|error| format!("reading from the approval queue: {error}"))?;
        let reply: Reply = serde_json::from_slice(&line).map_err(|error| error.to_string())?;
        match reply.error {
            Some(error) => Err(error),
            // `{"ok":null}` is the daemon's success for an operation with no
            // payload, so null is not an error here.
            None => Ok(reply.ok),
        }
    }
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    ok: Value,
    #[serde(default)]
    error: Option<String>,
}

/// One bounded reply line. The daemon writes exactly one JSON object.
pub(crate) fn read_line(reader: &mut impl std::io::BufRead) -> std::io::Result<Vec<u8>> {
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
            "denied" | "expired" | "withdrawn" => Answer::Denied(id, state.clone()),
            other => Answer::Unavailable(format!("the approval queue returned state {other:?}")),
        },
        Some(Value::Object(state)) => match state.get("allowed").and_then(Value::as_str) {
            Some(scope) => Answer::Allowed(scope.to_owned()),
            None => Answer::Unavailable("the approval queue returned no allowed scope".into()),
        },
        _ => Answer::Unavailable("the approval queue returned no state".into()),
    }
}

fn parse_state(value: &Value) -> Option<State> {
    let id = value.get("id")?.as_str()?.to_owned();
    let destination = value.get("destination").and_then(Value::as_str).unwrap_or_default().to_owned();
    let state = match value.get("state")? {
        Value::String(state) => state.clone(),
        Value::Object(map) if map.contains_key("allowed") => "allowed".into(),
        _ => return None,
    };
    let scope = match value.get("state") {
        Some(Value::Object(map)) => map.get("allowed").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    };
    Some(State { id, destination, state, scope })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answers_are_distinguished() {
        assert_eq!(answer(&serde_json::json!({ "id": "a", "state": "pending" })), Answer::Pending("a".into()));
        assert_eq!(
            answer(&serde_json::json!({ "id": "a", "state": "denied" })),
            Answer::Denied("a".into(), "denied".into())
        );
        assert_eq!(
            answer(&serde_json::json!({ "id": "a", "state": "expired" })),
            Answer::Denied("a".into(), "expired".into())
        );
        assert_eq!(
            answer(&serde_json::json!({ "id": "a", "state": { "allowed": "task" } })),
            Answer::Allowed("task".into())
        );
        assert!(matches!(answer(&serde_json::json!({})), Answer::Unavailable(_)));
    }

    #[test]
    fn an_item_state_keeps_its_scope_and_destination() {
        let state = parse_state(&serde_json::json!({
            "id": "a", "destination": "other", "state": { "allowed": "project" }
        }))
        .unwrap();
        assert_eq!(
            state,
            State {
                id: "a".into(),
                destination: "other".into(),
                state: "allowed".into(),
                scope: Some("project".into()),
            }
        );
        assert_eq!(
            parse_state(&serde_json::json!({ "id": "a", "destination": "other", "state": "pending" })).unwrap(),
            State { id: "a".into(), destination: "other".into(), state: "pending".into(), scope: None }
        );
        assert!(parse_state(&serde_json::json!({ "nope": true })).is_none());
    }
}
