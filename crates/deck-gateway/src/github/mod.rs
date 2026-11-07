//! The GitHub operation surface on the gateway's own socket (ADR 0007 §1).
//!
//! The design choice: one request is one operation, decided before anything
//! leaves the host. The path selects the operation, the JSON body names the
//! branch or pull request, a publish appends one bounded git bundle after its
//! body, and every refusal is a JSON body the agent can read — including the
//! `deck_boundary_denied` shape with an approval id, so a blocked push reads
//! the same as a blocked fetch. Nothing here is a proxy: a path that is not
//! in the table is a 404, not a forwarded request.

mod ask;
pub mod bundle;
pub mod client;
pub mod control;
mod identity;
pub mod policy;
pub mod problem;
pub mod publish;

pub use client::Client;
pub use control::{Answer, Control, Coverage, Publication};
pub use policy::{Ask, Operation};

use crate::credential::{Broker, Credential, Registration};
use crate::grants::{Grants, Registration as GrantRegistration};
use crate::http::{self, ReadError, Request};
use crate::web::Events;
use std::sync::atomic::{AtomicBool, Ordering};

/// The most a request head may be; the same bound every listener applies.
const MAX_HEAD: usize = crate::http::MAX_HEAD;
use serde_json::json;
use std::collections::HashMap;
use std::io::{self, Read as _};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The gateway kind approvals, grants and events carry, and the framed kind
/// the extension port dispatches.
pub use policy::{GATEWAY, KIND, PROFILE};

/// A request's JSON head. A pull request body is the largest thing in it.
pub const MAX_ASK: usize = 64 * 1024;
/// The gateway answers one request per connection; a publish is the slow one.
const READ: Duration = Duration::from_secs(60);
/// The total request-head budget. Recompute the socket timeout for each byte
/// so a peer cannot renew it by trickling input.
const HEAD: Duration = Duration::from_secs(5);
const WRITE: Duration = Duration::from_secs(180);
/// The wait for the remainder of a body the peer declared. A publish's bundle
/// arrives on the same connection; anything slower than this is a peer that
/// stopped writing.
const BODY: Duration = Duration::from_secs(10);

pub struct GitHub {
    client: Client,
    control: Option<Control>,
    status: Arc<Events>,
    /// The broker each operation fetches its token from, and the credential
    /// name. `None` refuses every operation as unavailable.
    credentials: Option<(Arc<Broker>, String)>,
    /// The persisted rules that admitted running operations. `None` watches
    /// nothing, which is a gateway started without grant files.
    grants: Option<Arc<Grants>>,
    /// The last summary of each branch this task published. A publish in
    /// `own-branch` mode is admitted with no card, so its #85 summary would
    /// otherwise never be shown; a later pull request card carries it.
    summaries: Mutex<HashMap<String, ask::Summary>>,
}

/// One operation's token and its registration with the broker. Dropping it
/// drops the value; a revocation meanwhile sets `cancel` and stops `client`.
/// `grant_revoked` says which of the two revoked it, so the refusal names the
/// authority the user actually removed.
struct Held {
    credential: Credential,
    cancel: Arc<AtomicBool>,
    grant_revoked: Arc<AtomicBool>,
    client: Client,
    _registration: Registration,
}

impl GitHub {
    pub fn new(client: Client, control: Option<Control>, status: Arc<Events>) -> Self {
        Self { client, control, status, credentials: None, grants: None, summaries: Mutex::new(HashMap::new()) }
    }

    /// The grant files this gateway watches. A gateway without one cannot
    /// verify a persisted admission, so it watches nothing.
    pub fn with_grants(mut self, grants: Arc<Grants>) -> Self {
        self.grants = Some(grants);
        self
    }

    /// Whether the operation is a write to the task's own branch, before the
    /// repository's visibility is known (ADR 0008 §6). `head` is the head of
    /// the pull request an update touches, when that was checked.
    fn own_branch(ask: &Ask, operation: Operation, settings: &client::Settings, head: Option<&str>) -> bool {
        let task_branch = policy::Policy::task_branch(&settings.task);
        match operation {
            Operation::Publish => ask.branch == task_branch,
            Operation::PullRequest => ask.head == task_branch && ask.draft.unwrap_or(true),
            Operation::UpdatePullRequest => head == Some(task_branch.as_str()) && ask.draft != Some(false),
            Operation::Refs | Operation::Review => false,
        }
    }

    /// Fetch the token under `name` from `broker`, once per operation.
    pub fn with_credentials(mut self, broker: Arc<Broker>, name: String) -> Self {
        self.credentials = Some((broker, name));
        self
    }

    /// The token for one operation, registered so a revocation stops it. A
    /// publish's `git push` and its API calls share this one fetch.
    fn fetch(&self) -> Result<Held, Failure> {
        let Some((broker, name)) = &self.credentials else {
            return Err(Failure::credential(&crate::credential::Refusal::Unavailable(
                "this gateway has no credential broker".into(),
            )));
        };
        let credential = broker.fetch(name).map_err(|refusal| Failure::credential(&refusal))?;
        let cancel = Arc::new(AtomicBool::new(false));
        // One client per operation, so a revocation shuts this operation's
        // requests without disturbing another's.
        let client = self.client.authorized(credential.value(), cancel.clone());
        let registration = broker
            .register(&credential, {
                let cancel = cancel.clone();
                let client = client.clone();
                move || {
                    cancel.store(true, Ordering::Release);
                    client.stop();
                }
            })
            .map_err(|refusal| Failure::credential(&refusal))?;
        Ok(Held {
            credential,
            cancel,
            grant_revoked: Arc::new(AtomicBool::new(false)),
            client,
            _registration: registration,
        })
    }

    /// Serve one guest connection: read the head, run the operation, answer.
    pub fn handle(&self, mut stream: UnixStream) -> io::Result<()> {
        stream.set_read_timeout(Some(READ))?;
        stream.set_write_timeout(Some(WRITE))?;
        let deadline = Instant::now() + HEAD;
        let (request, rest) = {
            match read(&mut stream, deadline) {
                Ok(Some(request)) => request,
                Ok(None) => return Ok(()),
                Err(ReadError::Refused { status, message }) => {
                    let _ = drain(&mut stream);
                    return reply(&mut stream, status, refusal(BAD_REQUEST, &message));
                }
                Err(ReadError::Io(error)) => return Err(error),
            }
        };
        // Header reads temporarily narrow this before every byte. Bodies and
        // bundles retain their existing independent bounds.
        stream.set_read_timeout(Some(READ))?;
        if request.method != "POST" {
            return reply(&mut stream, 405, refusal(BAD_REQUEST, "the GitHub operations take POST"));
        }
        // A bounded body is read for every path, so a mismatch is answered
        // after the request is complete and the client never sees a stall.
        let Some(operation) =
            crate::github::policy::operations().find(|candidate| candidate.path() == request.target).copied()
        else {
            // `/operations/merge` and its kin are answered by name, like the
            // same names in a body; any other path is simply not served.
            let named = request.target.strip_prefix("/operations/").unwrap_or_default();
            return match policy::requested(named) {
                Err(message) if Operation::refused().contains(&named) => {
                    reply(&mut stream, 403, refusal(NOT_PERMITTED, &message))
                }
                _ => reply(
                    &mut stream,
                    404,
                    refusal(NOT_PERMITTED, &format!("{} is not a Deck GitHub operation", request.target)),
                ),
            };
        };
        // A publish's body is one JSON head followed by the length-framed
        // bundle; every other operation's body is only the JSON head.
        let (request, rest) = match operation.carries_bundle() {
            true => split_body(request, rest),
            false => (request, rest),
        };
        if request.body.len() > MAX_ASK {
            return reply(
                &mut stream,
                413,
                refusal(BAD_REQUEST, &format!("the operation's JSON exceeds {MAX_ASK} bytes")),
            );
        }
        let (ask, parsed) = match Ask::parse(&request.body) {
            Ok(parsed) => parsed,
            Err(policy::Unparsed::NotPermitted(message)) => {
                return reply(&mut stream, 403, refusal(NOT_PERMITTED, &message));
            }
            Err(policy::Unparsed::Invalid(message)) => return reply(&mut stream, 400, refusal(BAD_REQUEST, &message)),
        };
        if parsed != operation {
            return reply(
                &mut stream,
                400,
                refusal(BAD_REQUEST, &format!("{} is served at {}", parsed.name(), operation.path())),
            );
        }
        let bundle = match operation.carries_bundle() {
            true => match bundle(rest) {
                Ok(bytes) => Some(bytes),
                Err(ReadError::Refused { status, message }) => {
                    let _ = drain(&mut stream);
                    return reply(&mut stream, status, refusal(BAD_REQUEST, &message));
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

    /// Decide, then perform. A write asks deckd's queue, which answers at once
    /// from a project rule or a live grant; a read never asks.
    fn run(&self, ask: &Ask, operation: Operation, bundle: Option<&[u8]>) -> Result<(String, String), Failure> {
        let detail = match operation {
            Operation::Publish => ask.branch.clone(),
            // Opening a pull request names the branch it comes from; changing
            // or reading one names the pull request.
            Operation::PullRequest => ask.head.clone(),
            Operation::UpdatePullRequest | Operation::Review => {
                ask.number.map(|number| format!("pr{number}")).unwrap_or_else(|| ask.head.clone())
            }
            Operation::Refs => String::new(),
        };
        let domain = self.client.destination(&detail);
        let identity = identity::review(ask, operation, bundle, self.client.settings())
            .map_err(|problem| self.failed(&domain, problem))?;
        // An update may only touch a pull request this task opened from a
        // branch it may write; that is checked before anyone is asked, and is
        // the one check that already needs the token. Every other operation
        // fetches it only once the queue admitted it.
        let mut held = None;
        let mut head = None;
        if let (Operation::UpdatePullRequest, Some(number)) = (operation, ask.number) {
            let token = self.fetch()?;
            match token.client.owns_pull_request(number) {
                Ok(owned) => head = Some(owned),
                Err(problem) => return Err(self.stopped(&token).unwrap_or_else(|| self.failed(&domain, problem))),
            }
            held = Some(token);
        }
        // The queue needs the repository's visibility before it can admit a
        // covered write without a card (ADR 0008 §6). This read is bounded and
        // happens once per operation; a failure only means a card.
        let mut coverage = None;
        if operation.asks() && Self::own_branch(ask, operation, self.client.settings(), head.as_deref()) {
            if held.is_none() {
                held = Some(self.fetch()?);
            }
            let Some(token) = held.as_ref() else {
                return Err(Failure::error(502, "deck_github_failed", "no credential was fetched", true));
            };
            let public = token.client.visibility();
            if let Some(failure) = self.stopped(token) {
                return Err(failure);
            }
            coverage = Some(Coverage { own_branch: true, public });
        }
        let remembered = match operation {
            Operation::PullRequest => self.summary_for(&ask.head),
            Operation::UpdatePullRequest => head.as_deref().and_then(|branch| self.summary_for(branch)),
            _ => None,
        };
        let scope = self.decide(
            operation,
            &domain,
            &reason(ask, operation),
            identity.as_deref(),
            coverage,
            remembered.as_ref().or(ask.summary.as_ref()),
        )?;
        let token = match held {
            Some(token) => token,
            None => self.fetch()?,
        };
        // Hold the rule that admitted this operation until it ends; deleting
        // or expiring it meanwhile stops `git` and the API through the same
        // flag a credential revocation sets.
        let _grant = self.watch_grant(&token, operation, &domain, scope.as_deref())?;
        let result = self.perform(&token.client, ask, operation, bundle);
        // The write happened; record what it published even if the token was
        // revoked while it ran. A report that cannot be delivered does not
        // change the operation's outcome.
        if let Ok(body) = &result {
            self.report(operation, ask, body);
            // Keep the publish's #85 summary for the pull request card that
            // follows; a card is the only place the user can read it.
            if operation == Operation::Publish
                && let Some(summary) = &ask.summary
            {
                self.remember(&ask.branch, summary);
            }
        }
        // A revoked token's operation is reported as revoked, whatever git or
        // the API made of being stopped; a ref already pushed stays pushed.
        if let Some(failure) = self.stopped(&token) {
            return Err(failure);
        }
        result.map(|body| (domain.clone(), body)).map_err(|problem| self.failed(&domain, problem))
    }

    /// Record the publish summary a later pull request card carries.
    fn remember(&self, branch: &str, summary: &ask::Summary) {
        let mut summaries = self.summaries.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        summaries.insert(branch.to_string(), summary.clone());
    }

    /// The last publish summary for `branch`, when this task published it.
    fn summary_for(&self, branch: &str) -> Option<ask::Summary> {
        let summaries = self.summaries.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        summaries.get(branch).cloned()
    }

    /// Report a successful publish, create or update to the task's record.
    /// The body is the gateway's own reply, so only fields it built are read.
    fn report(&self, operation: Operation, ask: &Ask, body: &str) {
        let Some(control) = &self.control else { return };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else { return };
        let publication = match operation {
            Operation::Publish => {
                let (Some(branch), Some(commit)) = (
                    value.get("branch").and_then(serde_json::Value::as_str),
                    value.get("commit").and_then(serde_json::Value::as_str),
                ) else {
                    return;
                };
                Publication::Branch { branch: bounded(branch, 200), commit: bounded(commit, 64) }
            }
            Operation::PullRequest | Operation::UpdatePullRequest => {
                let Some(number) = value.get("number").and_then(serde_json::Value::as_u64) else { return };
                let url = value.get("html_url").and_then(serde_json::Value::as_str).unwrap_or_default();
                let title = value.get("title").and_then(serde_json::Value::as_str).unwrap_or_default();
                let draft = value
                    .get("draft")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or_else(|| ask.draft.unwrap_or(operation == Operation::PullRequest));
                Publication::PullRequest { number, url: bounded(url, MAX_URL), title: bounded(title, MAX_TITLE), draft }
            }
            Operation::Refs | Operation::Review => return,
        };
        control.report(&publication);
    }

    /// The revocation failure, when this operation's token or grant was
    /// revoked. A grant revocation is the boundary's refusal, not a
    /// credential one: the token value itself is still valid.
    fn stopped(&self, token: &Held) -> Option<Failure> {
        if !token.cancel.load(Ordering::Acquire) {
            return None;
        }
        Some(match token.grant_revoked.load(Ordering::Acquire) {
            true => Failure::denied(None, false, "the authorizing GitHub grant was revoked or expired"),
            false => Failure::credential(&token.credential.revoked()),
        })
    }

    /// Hold the persisted rule that admitted this operation until it ends.
    /// Once and `default` admissions have no rule, and a gateway without
    /// grant files cannot verify one. A rule that is gone at registration was
    /// revoked between the decision and the dial, so nothing reaches GitHub.
    fn watch_grant(
        &self,
        token: &Held,
        operation: Operation,
        destination: &str,
        scope: Option<&str>,
    ) -> Result<Option<GrantRegistration>, Failure> {
        let Some(grants) = &self.grants else { return Ok(None) };
        let Some(scope) = scope.filter(|scope| matches!(*scope, "timed" | "task" | "project")) else {
            return Ok(None);
        };
        let credential = self.credentials.as_ref().map(|(_, name)| name.as_str());
        let Some(permit) = grants.github(operation.name(), destination, credential, scope) else {
            return Err(Failure::denied(None, false, "the authorizing GitHub grant was revoked or expired"));
        };
        grants
            .register(Some(permit), {
                let cancel = token.cancel.clone();
                let grant_revoked = token.grant_revoked.clone();
                let client = token.client.clone();
                move || {
                    grant_revoked.store(true, Ordering::Release);
                    cancel.store(true, Ordering::Release);
                    client.stop();
                }
            })
            .map_err(|_| Failure::denied(None, false, "the authorizing GitHub grant was revoked or expired"))
    }

    /// Run the admitted operation with its own token.
    fn perform(
        &self,
        client: &Client,
        ask: &Ask,
        operation: Operation,
        bundle: Option<&[u8]>,
    ) -> Result<String, problem::Problem> {
        match operation {
            Operation::Publish => {
                let bytes = bundle.ok_or_else(|| problem::Problem::invalid("publish needs a bundle"))?;
                client.publish(&ask.branch, bytes)
            }
            Operation::Refs => client.refs(),
            Operation::PullRequest => {
                client.pull_request(&ask.head, &ask.base, &ask.title, ask.body(), ask.draft.unwrap_or(true))
            }
            Operation::UpdatePullRequest => {
                let number =
                    ask.number.ok_or_else(|| problem::Problem::invalid("update_pr needs the pull request number"))?;
                client.update_pull_request(
                    number,
                    (!ask.base.is_empty()).then_some(ask.base.as_str()),
                    (!ask.title.is_empty()).then_some(ask.title.as_str()),
                    ask.body.as_deref(),
                    ask.draft,
                )
            }
            Operation::Review => {
                let number =
                    ask.number.ok_or_else(|| problem::Problem::invalid("review needs the pull request number"))?;
                client.review(number)
            }
        }
    }

    /// Record a failure for the agent's event feed and turn it into a reply;
    /// [`problem::Kind`] is the one place a failure gets its status and code.
    fn failed(&self, domain: &str, problem: problem::Problem) -> Failure {
        self.status.observe_in(GATEWAY, "", domain, "failed", None, Some(&problem.message));
        Failure { status: problem.kind.status(), body: problem.body() }
    }

    fn decide(
        &self,
        operation: Operation,
        destination: &str,
        reason: &str,
        identity: Option<&str>,
        coverage: Option<Coverage>,
        summary: Option<&ask::Summary>,
    ) -> Result<Option<String>, Failure> {
        if !operation.asks() {
            return Ok(None);
        }
        let Some(control) = &self.control else {
            return Err(Failure::error(
                403,
                "deck_boundary_denied",
                &format!("{destination} needs approval, but this task has no approval queue"),
                false,
            ));
        };
        let credential = self.credentials.as_ref().map(|(_, name)| name.as_str());
        match control.ask(
            operation.name(),
            destination,
            Some(reason.to_string()),
            credential,
            identity,
            coverage,
            summary,
        ) {
            Answer::Allowed(id, scope) => {
                // A default admission is the record of a write no card showed;
                // every other allow already has a card the agent saw.
                if scope == "default" {
                    self.status.observe_in(GATEWAY, &id, destination, "allowed", Some("default"), Some(reason));
                }
                Ok(Some(scope))
            }
            Answer::Pending(id) => {
                self.status.observe_in(GATEWAY, &id, destination, "pending", None, Some(reason));
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
                self.status.observe_in(GATEWAY, &id, destination, &state, None, None);
                Err(Failure::denied(
                    (!id.is_empty()).then_some(id.as_str()),
                    false,
                    &format!("{} on {destination} was denied by the user's boundary policy", operation.name()),
                ))
            }
            Answer::Unavailable(why) => Err(Failure::error(502, "deck_boundary_unavailable", &why, true)),
        }
    }
}

/// The most of a pull request title or URL the task record carries.
const MAX_TITLE: usize = 1024;
const MAX_URL: usize = 2048;

/// `text` cut to at most `max` characters, keeping the record bounded even
/// though the agent supplied the title.
fn bounded(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
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

/// The codes the gateway itself answers with before an operation runs
/// (contract 1.2.0 §11.5).
const BAD_REQUEST: &str = "deck_bad_request";
const NOT_PERMITTED: &str = "deck_not_permitted";

/// Every non-2xx body: `error.{message,type,code}` and `retryable`, false
/// unless the caller sets it. Transport refusals use it too, so a client
/// parses one shape.
fn refusal(code: &str, message: &str) -> serde_json::Value {
    json!({ "error": { "message": message, "type": code, "code": code }, "retryable": false })
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

    fn error(status: u16, code: &str, message: &str, retryable: bool) -> Self {
        let mut body = refusal(code, message);
        body["retryable"] = json!(retryable);
        Self { status, body }
    }

    /// A broker refusal: nothing was injected and nothing reached GitHub.
    fn credential(refusal: &crate::credential::Refusal) -> Self {
        Self::error(refusal.status(), refusal.code(), refusal.message(), refusal.retryable())
    }
}

/// Fill a body of a declared length from a peer that is still writing it.
///
/// The design choice: the wait for the rest of the body is short. A guest that
/// declared a length and stopped sending is a malformed request, and holding
/// the connection for the listener's whole read timeout buys nothing — the
/// answer is a refusal either way.
fn read_body(reader: &mut UnixStream, body: &mut [u8]) -> io::Result<()> {
    let previous = reader.read_timeout()?;
    reader.set_read_timeout(Some(BODY))?;
    let result = reader.read_exact(body);
    reader.set_read_timeout(previous)?;
    result
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
/// The design choice: the head is read a byte at a time to its `\r\n\r\n`
/// rather than through a `BufReader`, so nothing past it is consumed; the body
/// is then exactly its content-length. For a publish that body is the JSON
/// head, the bundle's length and the bundle, and its ceiling is sized for the
/// bundle rather than for a JSON request.
fn read(reader: &mut UnixStream, deadline: Instant) -> Result<Option<(Request, Vec<u8>)>, ReadError> {
    let mut head: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if head.len() >= MAX_HEAD {
            return Err(ReadError::Refused { status: 431, message: format!("request head exceeds {MAX_HEAD} bytes") });
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ReadError::Refused { status: 408, message: "request head deadline expired".into() });
        }
        reader.set_read_timeout(Some(left)).map_err(ReadError::Io)?;
        match reader.read(&mut byte) {
            Ok(0) if head.is_empty() => return Ok(None),
            Ok(0) => return Err(ReadError::Io(io::Error::from(io::ErrorKind::UnexpectedEof))),
            Ok(_) => head.push(byte[0]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                return Err(ReadError::Refused { status: 408, message: "request head deadline expired".into() });
            }
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
    // A publish's content-length covers its JSON head, the bundle's 4-byte
    // length and the bundle, so its ceiling is the sum of those bounds; the
    // JSON head alone is held to `MAX_ASK` once it is split off. Every other
    // operation's body is only the JSON head.
    let limit = match crate::github::policy::operations().any(|op| op.carries_bundle() && op.path() == target) {
        true => MAX_ASK + 4 + bundle::MAX_BUNDLE,
        false => MAX_ASK,
    };
    if length > limit {
        return Err(ReadError::Refused { status: 413, message: format!("the request body exceeds {limit} bytes") });
    }
    // The body is exactly its content-length; this read never waits for a
    // peer to close.
    let mut after = vec![0; length];
    if length > 0 {
        // A body that stops arriving is a malformed request, not a peer that
        // went away: answer it and close, rather than dropping the connection
        // with nothing said.
        read_body(reader, &mut after).map_err(|error| match error.kind() {
            io::ErrorKind::UnexpectedEof => {
                ReadError::Refused { status: 400, message: "connection closed in the request body".into() }
            }
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                ReadError::Refused { status: 400, message: "the request body stopped before its content-length".into() }
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
            body: Some("b".into()),
            ..Ask::default()
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
