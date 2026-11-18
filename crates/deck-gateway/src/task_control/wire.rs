//! The control kind's wire (contract 1.4.0 §13), pure: the bearer check, the
//! route, the closed operation set with its bounds, and the refusal codes
//! with their statuses. No sockets, so every rule is a unit test.
//!
//! The design choice: the gateway parses strictly and decides nothing about
//! policy. A request that passes here is forwarded to `deckd` as the typed
//! operation, re-serialized, so an unknown field or an operation outside the
//! set never reaches the host at all; scope, caps and provenance are
//! `deckd`'s to refuse.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::time::Duration;

/// The only path the surface serves.
pub const PATH: &str = "/control";
/// The largest request body, refused before it is read.
pub const MAX_BODY: usize = 32 * 1024;
/// The longest `wait`; a larger `wait_ms` is refused, never shortened.
pub const WAIT_LIMIT_MS: u64 = 300_000;
/// The operation set, in the contract's order.
pub const OPERATIONS: [&str; 6] = ["tasks", "task", "create", "prompt", "wait", "stop"];
/// The challenge a 401 carries.
pub const CHALLENGE: &str = "Bearer realm=\"deck-control\"";
/// The most characters a refusal's message carries.
pub const MESSAGE_CHARS: usize = 4096;
const TOKEN_MIN: usize = 32;
const TOKEN_MAX: usize = 256;

/// A refusal code, from the contract's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    BadRequest,
    Unauthorized,
    Scope,
    Cap,
    Refused,
    Unknown,
    Busy,
    Unavailable,
}

impl Code {
    pub const ALL: [Code; 8] = [
        Code::BadRequest,
        Code::Unauthorized,
        Code::Scope,
        Code::Cap,
        Code::Refused,
        Code::Unknown,
        Code::Busy,
        Code::Unavailable,
    ];

    pub fn code(self) -> &'static str {
        match self {
            Code::BadRequest => "deck_control_bad_request",
            Code::Unauthorized => "deck_control_unauthorized",
            Code::Scope => "deck_control_scope",
            Code::Cap => "deck_control_cap",
            Code::Refused => "deck_control_refused",
            Code::Unknown => "deck_control_unknown",
            Code::Busy => "deck_control_busy",
            Code::Unavailable => "deck_control_unavailable",
        }
    }

    /// Whether the same request can succeed later unchanged.
    pub fn retryable(self) -> bool {
        matches!(self, Code::Cap | Code::Busy | Code::Unavailable)
    }

    /// The status a refusal of this code carries, unless the route chose a
    /// more specific one (404, 405, 411, 413 for a bad request).
    pub fn status(self) -> u16 {
        match self {
            Code::BadRequest => 400,
            Code::Unauthorized => 401,
            Code::Scope | Code::Cap | Code::Refused => 403,
            Code::Unknown => 404,
            Code::Busy => 409,
            Code::Unavailable => 503,
        }
    }

    /// The code `deckd` named, by its wire string.
    pub fn named(code: &str) -> Option<Code> {
        Code::ALL.into_iter().find(|known| known.code() == code)
    }
}

/// One refusal: its code, status and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: Code,
    pub status: u16,
    pub message: String,
}

impl Refusal {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self::at(code, code.status(), message)
    }

    pub fn at(code: Code, status: u16, message: impl Into<String>) -> Self {
        let message: String = message.into().chars().take(MESSAGE_CHARS).collect();
        Self { code, status, message }
    }

    /// The `control-error` body.
    pub fn body(&self) -> Value {
        let code = self.code.code();
        json!({ "error": { "message": self.message, "type": code, "code": code }, "retryable": self.code.retryable() })
    }
}

/// A coordinator's bearer token, kept only as its digest, so comparing one
/// costs the same whatever the caller sent.
#[derive(Clone)]
pub struct Token([u8; 32]);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

impl Token {
    /// A token in the contract's alphabet and length (RFC 6750 `b64token`).
    pub fn new(token: &str) -> Result<Self, String> {
        let body = token.trim_end_matches('=');
        let alphabet = |b: u8| b.is_ascii_alphanumeric() || b"-._~+/".contains(&b);
        if !(TOKEN_MIN..=TOKEN_MAX).contains(&token.len()) || body.is_empty() || !body.bytes().all(alphabet) {
            return Err(format!("a control token is {TOKEN_MIN} to {TOKEN_MAX} b64token characters"));
        }
        Ok(Self(Sha256::digest(token.as_bytes()).into()))
    }

    fn matches(&self, presented: &str) -> bool {
        let presented: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        self.0.iter().zip(presented).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
    }
}

/// Whether the caller may use the surface. With no token (a VM guest's
/// loopback) any header is ignored; with one, exactly one `authorization`
/// header must carry it under the Bearer scheme, in any case.
pub fn authorized(headers: &[(String, String)], token: Option<&Token>) -> bool {
    let Some(token) = token else { return true };
    let mut values = headers.iter().filter(|(name, _)| name == "authorization").map(|(_, value)| value);
    let (Some(value), None) = (values.next(), values.next()) else { return false };
    let Some((scheme, presented)) = value.split_once(' ') else { return false };
    scheme.eq_ignore_ascii_case("bearer") && token.matches(presented.trim_start_matches(' '))
}

/// The path, method and length checks, in the contract's order, before any
/// body byte is read. Returns the body length to read.
pub fn route(method: &str, target: &str, length: Option<usize>) -> Result<usize, Refusal> {
    let path = target.split('?').next().unwrap_or(target);
    if path != PATH {
        return Err(Refusal::at(Code::BadRequest, 404, "the only path is /control"));
    }
    if method != "POST" {
        return Err(Refusal::at(Code::BadRequest, 405, "use POST"));
    }
    let Some(length) = length else {
        return Err(Refusal::at(Code::BadRequest, 411, "a content-length is required"));
    };
    if length > MAX_BODY {
        return Err(Refusal::at(Code::BadRequest, 413, format!("the body is at most {MAX_BODY} bytes")));
    }
    Ok(length)
}

/// A worker's budget, as `create` may ask for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
}

/// One operation of the closed set, with the contract's request fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Tasks {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
    },
    Task {
        id: String,
    },
    Create {
        project: String,
        brief: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget: Option<Budget>,
    },
    Prompt {
        id: String,
        text: String,
    },
    Wait {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_turn: Option<u64>,
        wait_ms: u64,
    },
    Stop {
        id: String,
    },
}

impl Operation {
    pub fn name(&self) -> &'static str {
        match self {
            Operation::Tasks { .. } => "tasks",
            Operation::Task { .. } => "task",
            Operation::Create { .. } => "create",
            Operation::Prompt { .. } => "prompt",
            Operation::Wait { .. } => "wait",
            Operation::Stop { .. } => "stop",
        }
    }

    /// How long `deckd` may take to answer: a wait's own bound, or the
    /// longest a prompt waits for a new task's session and its turn to start.
    pub fn timeout(&self) -> Duration {
        match self {
            Operation::Wait { wait_ms, .. } => Duration::from_millis(*wait_ms) + Duration::from_secs(30),
            _ => Duration::from_secs(180),
        }
    }

    fn check(&self) -> Result<(), String> {
        let bounded = |name: &str, value: &str, max: usize| {
            if value.is_empty() || value.len() > max {
                return Err(format!("`{name}` is 1 to {max} bytes"));
            }
            Ok(())
        };
        match self {
            Operation::Tasks { project } => project.as_deref().map_or(Ok(()), |p| bounded("project", p, 1024)),
            Operation::Task { id } | Operation::Stop { id } => bounded("id", id, 128),
            Operation::Prompt { id, text } => bounded("id", id, 128).and_then(|()| bounded("text", text, MAX_BODY)),
            Operation::Wait { id, wait_ms, .. } => {
                bounded("id", id, 128)?;
                if *wait_ms > WAIT_LIMIT_MS {
                    return Err(format!("wait_ms is at most {WAIT_LIMIT_MS}"));
                }
                Ok(())
            }
            Operation::Create { project, brief, title, budget } => {
                bounded("project", project, 1024)?;
                bounded("brief", brief, MAX_BODY)?;
                if let Some(title) = title
                    && (title.is_empty() || title.chars().count() > 80)
                {
                    return Err("`title` is 1 to 80 characters".into());
                }
                match budget {
                    Some(Budget { requests: None, tokens: None }) => Err("`budget` names requests or tokens".into()),
                    Some(Budget { requests: Some(0), .. } | Budget { tokens: Some(0), .. }) => {
                        Err("a budget is at least 1".into())
                    }
                    _ => Ok(()),
                }
            }
        }
    }
}

/// Parse one request body into an operation of the set.
pub fn parse(body: &[u8]) -> Result<Operation, Refusal> {
    let bad = |message: String| Refusal::new(Code::BadRequest, message);
    let value: Value = serde_json::from_slice(body).map_err(|_| bad("the body is not JSON".into()))?;
    let Some(object) = value.as_object() else { return Err(bad("the body is not a JSON object".into())) };
    let Some(operation) = object.get("operation").and_then(Value::as_str) else {
        return Err(bad("missing `operation`".into()));
    };
    if !OPERATIONS.contains(&operation) {
        let set = OPERATIONS.join(", ");
        return Err(Refusal::new(Code::Refused, format!("`{operation}` is not a control operation; the set is {set}")));
    }
    let operation: Operation = serde_json::from_value(value).map_err(|error| bad(plain(&error)))?;
    operation.check().map_err(bad)?;
    Ok(operation)
}

/// serde's message without its expectations or position: `unknown field
/// `force``, not the list of fields the guest might try next.
fn plain(error: &serde_json::Error) -> String {
    let text = error.to_string();
    let text = text.split(", expected").next().unwrap_or(&text);
    text.split(" at line ").next().unwrap_or(text).to_string()
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
