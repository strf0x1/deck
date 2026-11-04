//! The GitHub operation surface on the gateway's own socket (ADR 0007 §1).
//!
//! The design choice: one request is one operation, decided before anything
//! leaves the host. The path selects the operation, the JSON body names the
//! branch or pull request, a publish appends one bounded git bundle after its
//! body, and every refusal is a JSON body the agent can read — including the
//! `deck_boundary_denied` shape with an approval id, so a blocked push reads
//! the same as a blocked fetch. Nothing here is a proxy: a path that is not
//! in the table is a 404, not a forwarded request.

pub mod bundle;
pub mod client;
pub mod control;
pub mod policy;
pub mod publish;
pub mod status;

pub use client::Client;
pub use control::{Answer, Control};
pub use policy::{Ask, Operation};
pub use status::Events;

use crate::http::{self, ReadError, Request};

/// The most a request head may be; the same bound every listener applies.
const MAX_HEAD: usize = crate::http::MAX_HEAD;
use serde_json::json;
use std::io::{self, Read as _};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

/// The gateway kind approvals, grants and events carry, and the framed kind
/// the extension port dispatches.
pub use policy::{GATEWAY, KIND, PROFILE};

/// A request's JSON head. A pull request body is the largest thing in it.
const MAX_ASK: usize = 64 * 1024;
/// The gateway answers one request per connection; a publish is the slow one.
const READ: Duration = Duration::from_secs(60);
const WRITE: Duration = Duration::from_secs(180);

pub struct GitHub {
    client: Client,
    control: Option<Control>,
    status: Arc<Events>,
}

impl GitHub {
    pub fn new(client: Client, control: Option<Control>, status: Arc<Events>) -> Self {
        Self { client, control, status }
    }

    /// Serve one guest connection: read the head, run the operation, answer.
    pub fn handle(&self, mut stream: UnixStream) -> io::Result<()> {
        stream.set_read_timeout(Some(READ))?;
        stream.set_write_timeout(Some(WRITE))?;
        let (request, rest) = {
            match read(&mut stream) {
                Ok(Some(request)) => request,
                Ok(None) => return Ok(()),
                Err(ReadError::Refused { status, message }) => {
                    let _ = drain(&mut stream);
                    return reply(&mut stream, status, json!({ "error": message }));
                }
                Err(ReadError::Io(error)) => return Err(error),
            }
        };
        if request.method != "POST" {
            return reply(&mut stream, 405, json!({ "error": "the GitHub operations take POST" }));
        }
        // A bounded body is read for every path, so a mismatch is answered
        // after the request is complete and the client never sees a stall.
        let Some(operation) =
            crate::github::policy::operations().find(|candidate| candidate.path() == request.target).copied()
        else {
            return reply(
                &mut stream,
                404,
                json!({ "error": format!("{} is not a Deck GitHub operation", request.target) }),
            );
        };
        // A publish's body is one JSON head followed by the length-framed
        // bundle; every other operation's body is only the JSON head.
        let (request, rest) = match operation.carries_bundle() {
            true => split_body(request, rest),
            false => (request, rest),
        };
        let (ask, parsed) = match Ask::parse(&request.body) {
            Ok(parsed) => parsed,
            Err(error) => return reply(&mut stream, 400, json!({ "error": error })),
        };
        if parsed != operation {
            return reply(
                &mut stream,
                400,
                json!({ "error": format!("{} is served at {}", parsed.name(), operation.path()) }),
            );
        }
        let bundle = match operation.carries_bundle() {
            true => match bundle(rest) {
                Ok(bytes) => Some(bytes),
                Err(ReadError::Refused { status, message }) => {
                    let _ = drain(&mut stream);
                    return reply(&mut stream, status, json!({ "error": message }));
                }
                Err(ReadError::Io(error)) => return Err(error),
            },
            false => None,
        };
        let response = match self.run(&ask, operation, bundle.as_deref()) {
            Ok((domain, body)) => {
                let mut response = serde_json::from_str::<serde_json::Value>(&body).unwrap_or_else(|_| json!(body));
                response["domain"] = json!(domain);
                (200, response)
            }
            Err(failure) => (failure.status, failure.body),
        };
        // A refusal closes the connection with a request the peer may not have
        // finished writing. Reading those bytes first is what keeps the close
        // a FIN rather than a RST, which Linux delivers to the client as a
        // lost response.
        let _ = drain(&mut stream);
        reply(&mut stream, response.0, response.1)
    }

    /// Decide, then perform. A write asks the queue unless the project holds a
    /// rule for it; a read never asks.
    fn run(&self, ask: &Ask, operation: Operation, bundle: Option<&[u8]>) -> Result<(String, String), Failure> {
        if !self.client.has_token() {
            return Err(Failure::error(503, "deck_credential_missing", "the host has no GitHub credential"));
        }
        let detail = match operation {
            Operation::Publish => ask.branch.clone(),
            Operation::PullRequest | Operation::UpdatePullRequest | Operation::Review => {
                ask.number.map(|number| format!("pr{number}")).unwrap_or_else(|| ask.head.clone())
            }
            Operation::Refs => String::new(),
        };
        let domain = self.client.destination(&detail);
        self.decide(operation, &domain, &reason(ask, operation))?;
        let result = match operation {
            Operation::Publish => {
                let bytes = bundle.ok_or_else(|| Failure::error(400, "deck_bad_request", "publish needs a bundle"))?;
                self.client.publish(&ask.branch, bytes)
            }
            Operation::Refs => self.client.refs(),
            Operation::PullRequest => {
                self.client.pull_request(&ask.head, &ask.base, &ask.title, &ask.body, ask.draft.unwrap_or(true))
            }
            Operation::UpdatePullRequest => {
                let number = ask.number.ok_or_else(|| {
                    Failure::error(400, "deck_bad_request", "update_pr needs the pull request number")
                })?;
                self.client.update_pull_request(
                    number,
                    (!ask.base.is_empty()).then_some(ask.base.as_str()),
                    (!ask.title.is_empty()).then_some(ask.title.as_str()),
                    ask.empty_body
                        .then_some(ask.body.as_str())
                        .or_else(|| (!ask.body.is_empty()).then_some(ask.body.as_str())),
                    ask.draft,
                )
            }
            Operation::Review => {
                let number = ask
                    .number
                    .ok_or_else(|| Failure::error(400, "deck_bad_request", "review needs the pull request number"))?;
                self.client.review(number)
            }
        };
        result.map(|body| (domain.clone(), body)).map_err(|message| {
            self.status.observe("", &domain, "failed", None, Some(&message));
            Failure::error(502, "deck_github_failed", &message)
        })
    }

    fn decide(&self, operation: Operation, destination: &str, reason: &str) -> Result<(), Failure> {
        if !operation.asks() {
            return Ok(());
        }
        let Some(control) = &self.control else {
            return Err(Failure::error(
                403,
                "deck_boundary_denied",
                &format!("{destination} needs approval, but this task has no approval queue"),
            ));
        };
        match control.ask(operation.name(), destination, Some(reason.to_string())) {
            Answer::Allowed(_) => Ok(()),
            Answer::Pending(id) => {
                self.status.observe(&id, destination, "pending", None, Some(reason));
                Err(Failure::denied(
                    Some(&id),
                    true,
                    &format!(
                        "{} on {destination} needs the user's approval; retry after they decide",
                        operation.name()
                    ),
                ))
            }
            Answer::Denied(id, state) => {
                self.status.observe(&id, destination, &state, None, None);
                Err(Failure::denied(
                    (!id.is_empty()).then_some(id.as_str()),
                    false,
                    &format!("{} on {destination} was denied by the user's boundary policy", operation.name()),
                ))
            }
            Answer::Unavailable(why) => Err(Failure::error(502, "deck_boundary_unavailable", &why)),
        }
    }
}

/// What the card shows about a request: the operation and what it touches,
/// never a credential or a body.
fn reason(ask: &Ask, operation: Operation) -> String {
    match operation {
        Operation::Publish => format!("publish a commit to {}", ask.branch),
        Operation::PullRequest => format!("open a pull request from {} to {}", ask.head, ask.base),
        Operation::UpdatePullRequest => format!("update pull request {}", ask.number.unwrap_or(0)),
        Operation::Refs | Operation::Review => String::new(),
    }
}

/// A refusal the agent can read. The `deck_boundary_denied` body is the same
/// shape the model and web gateways use, so Construct reports it the same way.
struct Failure {
    status: u16,
    body: serde_json::Value,
}

impl Failure {
    fn denied(approval: Option<&str>, retryable: bool, message: &str) -> Self {
        let mut body = json!({
            "error": { "message": message, "type": "deck_boundary_denied", "code": "deck_boundary_denied" },
            "retryable": retryable,
        });
        if let Some(id) = approval {
            body["approval_id"] = json!(id);
        }
        Self { status: 403, body }
    }

    fn error(status: u16, code: &str, message: &str) -> Self {
        Self { status, body: json!({ "error": { "message": message, "type": code, "code": code } }) }
    }
}

/// Read whatever the peer still has to send, briefly. A request that ended
/// early is the common case here: the gateway answers it and closes.
fn drain(stream: &mut UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(50)))?;
    let mut scratch = [0u8; 4096];
    loop {
        match stream.read(&mut scratch) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Ok(()),
        }
    }
}

/// Read one request and keep what arrived after its head.
///
/// The design choice: this reader owns the bytes after the head instead of a
/// `BufReader`, because a publish's bundle follows the head on the same
/// connection and a buffered read of the head would swallow it. The head is
/// read to its `\r\n\r\n`, parsed, and everything after it — the JSON body
/// and then the bundle — is returned unread, in order.
fn read(reader: &mut impl io::Read) -> Result<Option<(Request, Vec<u8>)>, ReadError> {
    let mut head: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if head.len() >= MAX_HEAD {
            return Err(ReadError::Refused { status: 431, message: format!("request head exceeds {MAX_HEAD} bytes") });
        }
        match reader.read(&mut byte) {
            Ok(0) if head.is_empty() => return Ok(None),
            Ok(0) => return Err(ReadError::Io(io::Error::from(io::ErrorKind::UnexpectedEof))),
            Ok(_) => head.push(byte[0]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(ReadError::Io(error)),
        }
        if head.ends_with(b"\r\n\r\n") {
            head.truncate(head.len() - 4);
            break;
        }
    }
    let text = std::str::from_utf8(&head)
        .map_err(|_| ReadError::Refused { status: 400, message: "request head is not UTF-8".into() })?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut words = request_line.split(' ');
    let (Some(method), Some(target), Some("HTTP/1.1" | "HTTP/1.0"), None) =
        (words.next(), words.next(), words.next(), words.next())
    else {
        return Err(ReadError::Refused { status: 400, message: "malformed request line".into() });
    };
    let method = method.to_string();
    let target = target.to_string();
    let mut headers = Vec::new();
    let mut length = 0usize;
    // The lines after the request line are the headers; an empty one ends
    // them, and nothing after it is part of the head.
    for line in lines.take_while(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(ReadError::Refused { status: 400, message: "malformed header line".into() });
        };
        let name = name.to_ascii_lowercase();
        let value = value.trim();
        if name == "transfer-encoding" {
            return Err(ReadError::Refused { status: 400, message: "chunked request bodies are not accepted".into() });
        }
        if name == "content-length" {
            length = value
                .parse()
                .map_err(|_| ReadError::Refused { status: 400, message: "content-length is not a number".into() })?;
        }
        headers.push((name, value.to_string()));
    }
    if length > MAX_ASK {
        return Err(ReadError::Refused { status: 413, message: format!("the request body exceeds {MAX_ASK} bytes") });
    }
    // The body is exactly its content-length. A publish's bundle follows it on
    // the same connection, so what arrives after the body belongs to the
    // caller; this read never waits for a peer to close.
    let mut after = vec![0; length];
    if length > 0 {
        reader.read_exact(&mut after).map_err(|error| match error.kind() {
            io::ErrorKind::UnexpectedEof => {
                ReadError::Refused { status: 400, message: "connection closed in the request body".into() }
            }
            _ => ReadError::Io(error),
        })?;
    }
    Ok(Some((Request { method, target, headers, body: after }, Vec::new())))
}

/// Split a publish body into its JSON head and the bytes after it.
///
/// The design choice: the head is the first JSON value in the body, found by
/// the depth of its braces — the operation object is flat. Everything after
/// that value is the length-framed bundle, so a client that sent one body
/// does not have to guess a second length.
fn split_body(mut request: Request, mut rest: Vec<u8>) -> (Request, Vec<u8>) {
    let mut body = std::mem::take(&mut request.body);
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut end = None;
    for (index, byte) in body.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    end = Some(index + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    if let Some(end) = end {
        let mut after = body.split_off(end);
        after.extend_from_slice(&rest);
        rest = after;
    }
    request.body = body;
    (request, rest)
}

/// The bundle after the JSON head: a big-endian length, then exactly that
/// many bytes. `head` is what followed the request line in the same body —
/// the reader keeps every byte after the head — so a small bundle is never
/// lost to a buffer. The length is the guest's number, so it is checked
/// against the policy's ceiling before anything is allocated.
fn bundle(head: Vec<u8>) -> Result<Vec<u8>, ReadError> {
    let mut reader = std::io::Cursor::new(head);
    let mut length = [0u8; 4];
    reader.read_exact(&mut length).map_err(|error| match error.kind() {
        io::ErrorKind::UnexpectedEof => ReadError::Refused { status: 411, message: "publish needs a bundle".into() },
        _ => ReadError::Io(error),
    })?;
    let length = u32::from_be_bytes(length) as usize;
    if length > bundle::MAX_BUNDLE {
        return Err(ReadError::Refused {
            status: 413,
            message: format!("the bundle exceeds {} bytes", bundle::MAX_BUNDLE),
        });
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).map_err(|error| match error.kind() {
        io::ErrorKind::UnexpectedEof => {
            ReadError::Refused { status: 400, message: "the connection ended inside the bundle".into() }
        }
        _ => ReadError::Io(error),
    })?;
    Ok(bytes)
}

fn reply(stream: &mut UnixStream, status: u16, body: serde_json::Value) -> io::Result<()> {
    let mut headers = vec![("content-type", "application/json".to_string())];
    if let Some(id) = body.get("approval_id").and_then(serde_json::Value::as_str) {
        headers.push(("x-deck-approval", id.to_string()));
    }
    http::write(stream, status, &headers, body.to_string().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fail(reader: &mut &[u8]) -> Result<Vec<u8>, ReadError> {
        bundle(reader.to_vec())
    }

    #[test]
    fn a_bundle_length_is_checked_before_it_is_allocated() {
        let too_big = ((bundle::MAX_BUNDLE + 1) as u32).to_be_bytes();
        assert!(matches!(fail(&mut &too_big[..]), Err(ReadError::Refused { status: 413, .. })));
        assert!(matches!(fail(&mut &[0u8; 2][..]), Err(ReadError::Refused { status: 411, .. })));
        let mut short = 4u32.to_be_bytes().to_vec();
        short.extend_from_slice(b"ab");
        assert!(matches!(fail(&mut &short[..]), Err(ReadError::Refused { status: 400, .. })));
        let mut good = 3u32.to_be_bytes().to_vec();
        good.extend_from_slice(b"abc");
        assert_eq!(fail(&mut &good[..]).unwrap(), b"abc");
    }

    #[test]
    fn a_denial_names_the_approval_and_says_whether_a_retry_works() {
        let denied = Failure::denied(Some("a1"), true, "needs approval");
        assert_eq!(denied.status, 403);
        assert_eq!(denied.body["approval_id"], "a1");
        assert_eq!(denied.body["retryable"], true);
        assert_eq!(denied.body["error"]["type"], "deck_boundary_denied");
    }

    #[test]
    fn the_card_sees_the_operation_and_its_target() {
        let ask = Ask {
            operation: "create_pr".into(),
            branch: String::new(),
            base: "main".into(),
            head: "deck/t-1".into(),
            title: "t".into(),
            body: "b".into(),
            empty_body: false,
            draft: None,
            number: None,
        };
        assert_eq!(reason(&ask, Operation::PullRequest), "open a pull request from deck/t-1 to main");
    }

    #[test]
    fn every_operation_has_its_own_path() {
        let mut paths: Vec<_> = crate::github::policy::operations().map(|operation| operation.path()).collect();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), 5, "{paths:?}");
        assert_eq!(policy::Policy::task_branch("t-1"), "deck/t-1");
    }
}
