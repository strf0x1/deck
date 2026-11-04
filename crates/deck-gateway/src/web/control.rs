//! The web gateway's client for the daemon's approval queue (ADR 0004 §5).
//!
//! The design choice: a new destination, port, mirror or redirect is decided
//! by `deckd`, never here. The gateway sends what it observed — the name, the
//! port, the method and path, the size, the reason — and waits for the answer;
//! the hold is the daemon's because it knows whether a capable window is
//! attached. Failing to reach `deckd` is a denial, since the same failure makes
//! a decision impossible. A blocked request is also reported to the gateway's
//! boundary events, so the agent learns why from the status endpoint (§6).

use serde_json::{Value, json};
use std::io::{BufRead, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Longer than the daemon's hold, so a held decision is not cut short.
const REPLY: Duration = Duration::from_secs(30);

/// What this gateway asks for. `operation` is `connect`, `fetch` or `profile`;
/// `destination` is `host:port` for the first two and the profile name for the
/// last, which is why a whole profile can be one decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub operation: String,
    pub destination: String,
    pub method: Option<String>,
    pub path: Option<String>,
    pub size: Option<u64>,
    pub reason: Option<String>,
}

impl Ask {
    pub fn connect(destination: String) -> Self {
        Self { operation: crate::web::CONNECT.into(), destination, method: None, path: None, size: None, reason: None }
    }

    pub fn fetch(destination: String, method: &str, path: &str, size: Option<u64>, reason: Option<String>) -> Self {
        Self {
            operation: crate::web::FETCH.into(),
            destination,
            method: Some(method.to_owned()),
            path: Some(path.to_owned()),
            size,
            reason,
        }
    }

    pub fn profile(name: String) -> Self {
        Self {
            operation: crate::web::PROFILE.into(),
            destination: name,
            method: None,
            path: None,
            size: None,
            reason: None,
        }
    }
}

/// The daemon's answer for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// A grant or a held decision allowed this request, with the granting
    /// scope (`once`, `task` or `project`).
    Allowed(String),
    /// The item is waiting for a window decision; a retry after it is decided
    /// is meaningful.
    Pending(String),
    /// A human denied it, or it expired or was withdrawn.
    Denied(String, String),
    /// `deckd` could not answer at all.
    Unavailable(String),
}

#[derive(Debug, Clone)]
pub struct Control {
    socket: PathBuf,
    task: String,
}

impl Control {
    pub fn new(socket: PathBuf, task: String) -> Self {
        Self { socket, task }
    }

    pub fn ask(&self, ask: &Ask) -> Answer {
        let mut request = json!({
            "op": "approval",
            "task": self.task,
            "gateway": crate::web::GATEWAY,
            "operation": ask.operation,
            "destination": ask.destination,
            "details": {},
        });
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
        if let Some(reason) = &ask.reason {
            request["reason"] = json!(reason);
        }
        match self.call(&request) {
            Ok(approval) => answer(&approval),
            Err(error) => Answer::Unavailable(error),
        }
    }

    fn call(&self, request: &Value) -> Result<Value, String> {
        let mut stream =
            UnixStream::connect(&self.socket).map_err(|error| format!("the approval queue is unreachable: {error}"))?;
        stream.set_read_timeout(Some(REPLY)).map_err(|error| error.to_string())?;
        stream.set_write_timeout(Some(Duration::from_secs(5))).map_err(|error| error.to_string())?;
        let mut line = serde_json::to_vec(request).map_err(|error| error.to_string())?;
        line.push(b'\n');
        stream.write_all(&line).map_err(|error| format!("writing to the approval queue: {error}"))?;
        let line = read_line(&mut std::io::BufReader::new(stream))
            .map_err(|error| format!("reading from the approval queue: {error}"))?;
        let reply: Reply = serde_json::from_slice(&line).map_err(|error| error.to_string())?;
        match reply.error {
            Some(error) => Err(error),
            None => Ok(reply.ok),
        }
    }
}

#[derive(serde::Deserialize)]
struct Reply {
    #[serde(default)]
    ok: Value,
    #[serde(default)]
    error: Option<String>,
}

/// One bounded reply line. The daemon writes exactly one JSON object.
fn read_line(reader: &mut impl BufRead) -> std::io::Result<Vec<u8>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_may_be_a_string_or_an_object() {
        assert_eq!(answer(&json!({ "id": "a", "state": { "allowed": "task" } })), Answer::Allowed("task".into()));
        assert_eq!(answer(&json!({ "id": "a", "state": "expired" })), Answer::Denied("a".into(), "expired".into()));
    }

    #[test]
    fn a_fetch_ask_carries_what_was_observed() {
        let ask = Ask::fetch("pypi.org:443".into(), "GET", "/simple/", Some(0), Some("redirect".into()));
        assert_eq!(ask.operation, "fetch");
        assert_eq!(ask.method.as_deref(), Some("GET"));
        assert_eq!(ask.path.as_deref(), Some("/simple/"));
        assert_eq!(ask.reason.as_deref(), Some("redirect"));
        assert_eq!(Ask::connect("a:22".into()).operation, "connect");
        assert_eq!(Ask::profile("registry".into()).destination, "registry");
    }
}
