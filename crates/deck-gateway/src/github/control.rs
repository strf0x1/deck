//! The GitHub gateway's client for the daemon's approval queue (ADR 0004 §5,
//! ADR 0007 §3).
//!
//! The design choice: a publish or a pull request is a human decision on the
//! host, and this gateway only asks. The destination it sends is the
//! repository and the branch or pull request, which is what the card must
//! say; the bounded hold is the daemon's because it knows whether a capable
//! window is attached. A failure to reach `deckd` is a denial for the same
//! reason it is everywhere else: a decision is impossible without it.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Longer than the daemon's hold, so a held decision is not cut short.
const REPLY: Duration = Duration::from_secs(30);

/// The gateway's classification of a GitHub write: whether it is covered by
/// the task's own branch, and the repository's visibility (`None` when the
/// read failed). The daemon applies the task's approval mode to these facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Coverage {
    pub own_branch: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public: Option<bool>,
}

/// The daemon's answer for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// A project rule, a held decision, or the approval mode allowed it, with
    /// the scope (`default` for a write no card showed).
    Allowed(String, String),
    /// The item is waiting for a window; a retry after a decision works.
    Pending(String),
    /// A human denied it, or it expired or was withdrawn.
    Denied(String, String),
    /// `deckd` did not answer.
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

    /// Ask for one operation on one destination. `reason` is what the card
    /// shows about the request; it never contains a credential.
    pub fn ask(
        &self,
        operation: &str,
        destination: &str,
        reason: Option<String>,
        credential: Option<&str>,
        identity: Option<&str>,
        coverage: Option<Coverage>,
    ) -> Answer {
        let mut request = json!({
            "op": "approval",
            "task": self.task,
            "gateway": crate::github::GATEWAY,
            "operation": operation,
            "destination": destination,
            "details": {},
        });
        if let Some(credential) = credential {
            request["credential"] = json!(credential);
        }
        if let Some(identity) = identity {
            request["details"]["identity"] = json!(identity);
        }
        if let Some(coverage) = coverage {
            request["coverage"] = json!(coverage);
        }
        if let Some(reason) = reason {
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
        let line = read_line(&mut BufReader::new(stream))
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
            Some(scope) => Answer::Allowed(id.clone(), scope.to_owned()),
            None => Answer::Unavailable("the approval queue returned no allowed scope".into()),
        },
        _ => Answer::Unavailable("the approval queue returned no state".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decision_is_read_from_either_shape_the_daemon_uses() {
        assert_eq!(
            answer(&json!({"id": "a", "state": {"allowed": "project"}})),
            Answer::Allowed("a".into(), "project".into())
        );
        assert_eq!(
            answer(&json!({"id": "b", "state": {"allowed": "default"}})),
            Answer::Allowed("b".into(), "default".into())
        );
        assert_eq!(answer(&json!({"id": "a", "state": "denied"})), Answer::Denied("a".into(), "denied".into()));
        assert!(matches!(answer(&json!({})), Answer::Unavailable(_)));
    }
}
