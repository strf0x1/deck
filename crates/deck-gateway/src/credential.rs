//! The gateway's side of `deckd`'s credential broker (ADR 0004 §4.4–§4.5).
//!
//! The design choice: a value is fetched from `deckd` for one operation, after
//! policy admitted it, and dropped when the operation ends; nothing is cached,
//! so a rotated value serves the next operation. Every operation that holds a
//! value registers here with its fingerprint and a way to close it. A
//! `credential_watch` connection, opened at start and reopened with backoff,
//! carries revocations; one closes the matching operations and refuses any
//! later registration of that value. Anything short of a value from `deckd`
//! (no daemon, no queue configured, a malformed reply) is *unavailable*.

use crate::control::{Control, read_line};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Longer than `deckd`'s 5 s store bound, so a slow Keychain read is reported
/// by the daemon as unavailable rather than cut short here.
const FETCH: Duration = Duration::from_secs(10);
const BACKOFF_FIRST: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Why an operation went ahead with no credential: nothing was injected and
/// nothing reached the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Missing(String),
    Unavailable(String),
    Refused(String),
    Revoked(String),
}

impl Refusal {
    pub fn status(&self) -> u16 {
        match self {
            Self::Missing(_) | Self::Unavailable(_) => 503,
            Self::Refused(_) | Self::Revoked(_) => 403,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing(_) => "deck_credential_missing",
            Self::Unavailable(_) => "deck_credential_unavailable",
            Self::Refused(_) => "deck_credential_refused",
            Self::Revoked(_) => "deck_credential_revoked",
        }
    }

    /// Only an unreachable daemon or store may pass on the agent's retry; the
    /// rest are the user's to fix.
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Missing(message) | Self::Unavailable(message) | Self::Refused(message) | Self::Revoked(message) => {
                message
            }
        }
    }

    /// The daemon's error line, `code: reason`. Any other error, or none at
    /// all, is a daemon that could not answer.
    fn from_reply(error: &str) -> Self {
        let (code, reason) = error.split_once(": ").unwrap_or(("", error));
        let reason = reason.to_string();
        match code {
            "deck_credential_missing" => Self::Missing(reason),
            "deck_credential_refused" => Self::Refused(reason),
            "deck_credential_revoked" => Self::Revoked(reason),
            "deck_credential_unavailable" => Self::Unavailable(reason),
            _ => Self::Unavailable(format!("deckd could not answer: {error}")),
        }
    }

    /// The bounded JSON error the guest sees, in the model gateway's shape.
    pub fn write(&self, stream: &mut impl std::io::Write) -> std::io::Result<()> {
        let body = json!({
            "error": { "message": self.message(), "type": self.code(), "code": self.code() },
            "retryable": self.retryable(),
        });
        crate::http::write(
            stream,
            self.status(),
            &[("content-type", "application/json".into())],
            body.to_string().as_bytes(),
        )
    }
}

/// One value for one operation. Its `Debug` never shows the value.
pub struct Credential {
    name: String,
    value: String,
    fingerprint: String,
}

impl Credential {
    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The refusal an operation on this value gets once it is revoked.
    pub fn revoked(&self) -> Refusal {
        revoked(self)
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential").field("name", &self.name).field("fingerprint", &self.fingerprint).finish()
    }
}

type Cancel = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Registry {
    revoked: HashSet<String>,
    operations: HashMap<u64, (String, Cancel)>,
    next: u64,
}

pub struct Broker {
    control: Option<Control>,
    registry: Mutex<Registry>,
}

/// An operation's hold on its value. Dropping it ends the registration, so a
/// later revocation no longer reaches the finished operation.
pub struct Registration {
    broker: Arc<Broker>,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.broker.lock().operations.remove(&self.id);
    }
}

#[derive(Deserialize)]
struct Revocation {
    #[serde(default)]
    name: String,
    fingerprint: String,
}

impl Broker {
    /// `control` is `deckd`'s socket and this task; `None` (a gateway run with
    /// no daemon, in a test) answers every fetch as unavailable.
    pub fn new(control: Option<Control>) -> Arc<Self> {
        Arc::new(Self { control, registry: Mutex::new(Registry::default()) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ask `deckd` for `name`'s current value. Called once per operation,
    /// after policy admitted it and just before the outbound request.
    pub fn fetch(&self, name: &str) -> Result<Credential, Refusal> {
        let Some(control) = &self.control else {
            return Err(Refusal::Unavailable(format!("{name} cannot be fetched: this gateway has no deckd")));
        };
        let request = json!({ "op": "credential", "task": control.task(), "name": name });
        let reply = control.call_within(&request, FETCH).map_err(|error| Refusal::from_reply(&error))?;
        let (Some(value), Some(fingerprint)) =
            (reply.get("value").and_then(Value::as_str), reply.get("fingerprint").and_then(Value::as_str))
        else {
            return Err(Refusal::Unavailable(format!("deckd's reply for {name} carried no value")));
        };
        let credential = Credential { name: name.into(), value: value.into(), fingerprint: fingerprint.into() };
        // A revocation that arrived on the watch between deckd's read and now
        // still wins.
        if self.lock().revoked.contains(&credential.fingerprint) {
            return Err(revoked(&credential));
        }
        Ok(credential)
    }

    /// Register a running operation that holds `credential`. `cancel` closes
    /// it and may run on the watch thread at any time until the registration
    /// is dropped. Refused when the value was revoked meanwhile.
    pub fn register(
        self: &Arc<Self>,
        credential: &Credential,
        cancel: impl Fn() + Send + Sync + 'static,
    ) -> Result<Registration, Refusal> {
        let mut registry = self.lock();
        if registry.revoked.contains(&credential.fingerprint) {
            return Err(revoked(credential));
        }
        let id = registry.next;
        registry.next += 1;
        registry.operations.insert(id, (credential.fingerprint.clone(), Arc::new(cancel)));
        Ok(Registration { broker: self.clone(), id })
    }

    /// Whether `fingerprint` was revoked, for an operation checking between
    /// its steps.
    pub fn is_revoked(&self, fingerprint: &str) -> bool {
        self.lock().revoked.contains(fingerprint)
    }

    /// Mark `fingerprint` revoked and close every operation using it. The
    /// cancels run outside the lock, so one may drop its own registration.
    pub fn revoke(&self, fingerprint: &str) {
        let cancels: Vec<Cancel> = {
            let mut registry = self.lock();
            registry.revoked.insert(fingerprint.to_string());
            registry
                .operations
                .values()
                .filter(|(used, _)| used == fingerprint)
                .map(|(_, cancel)| cancel.clone())
                .collect()
        };
        for cancel in cancels {
            cancel();
        }
    }

    /// Hold the `credential_watch` connection for the gateway's life. While
    /// it is down `deckd` is down, so no fetch succeeds; the snapshot on each
    /// reconnect catches anything revoked in the gap.
    pub fn watch(self: &Arc<Self>) -> std::io::Result<()> {
        let Some(control) = self.control.clone() else { return Ok(()) };
        let broker = self.clone();
        std::thread::Builder::new().name("gateway-credential-watch".into()).spawn(move || {
            let mut backoff = BACKOFF_FIRST;
            loop {
                if broker.follow(&control) {
                    backoff = BACKOFF_FIRST;
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        })?;
        Ok(())
    }

    /// One watch connection until it ends. True when deckd answered at all.
    fn follow(&self, control: &Control) -> bool {
        let Ok(mut stream) = UnixStream::connect(control.socket()) else { return false };
        let request = json!({ "op": "credential_watch", "task": control.task() });
        let mut line = request.to_string().into_bytes();
        line.push(b'\n');
        if stream.write_all(&line).is_err() {
            return false;
        }
        let mut reader = BufReader::new(stream);
        let Ok(first) = read_line(&mut reader) else { return false };
        let Ok(reply) = serde_json::from_slice::<Value>(&first) else { return false };
        let Some(snapshot) = reply.pointer("/ok/revoked").and_then(Value::as_array) else { return false };
        for revocation in snapshot {
            if let Ok(revocation) = serde_json::from_value::<Revocation>(revocation.clone()) {
                self.revoke(&revocation.fingerprint);
            }
        }
        while let Ok(line) = read_line(&mut reader) {
            if let Ok(revocation) = serde_json::from_slice::<Revocation>(&line) {
                eprintln!("deck-gateway: {} {} was revoked", revocation.name, revocation.fingerprint);
                self.revoke(&revocation.fingerprint);
            }
        }
        true
    }
}

fn revoked(credential: &Credential) -> Refusal {
    Refusal::Revoked(format!(
        "{}'s current value ({}) was revoked; set a new value to restore it",
        credential.name, credential.fingerprint
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn credential(fingerprint: &str) -> Credential {
        Credential { name: "OPENAI_API_KEY".into(), value: "sk-secret".into(), fingerprint: fingerprint.into() }
    }

    #[test]
    fn a_daemon_error_keeps_its_code_and_anything_else_is_unavailable() {
        assert_eq!(Refusal::from_reply("deck_credential_missing: no value").code(), "deck_credential_missing");
        assert_eq!(Refusal::from_reply("deck_credential_refused: not yours").status(), 403);
        let revoked = Refusal::from_reply("deck_credential_revoked: gone");
        assert_eq!((revoked.code(), revoked.retryable()), ("deck_credential_revoked", false));
        let other = Refusal::from_reply("the approval queue is unreachable: refused");
        assert_eq!((other.code(), other.status(), other.retryable()), ("deck_credential_unavailable", 503, true));
    }

    #[test]
    fn without_a_daemon_every_fetch_is_unavailable() {
        assert!(matches!(Broker::new(None).fetch("OPENAI_API_KEY"), Err(Refusal::Unavailable(_))));
    }

    #[test]
    fn a_revocation_closes_matching_operations_and_refuses_later_ones() {
        let broker = Broker::new(None);
        let closed = Arc::new(AtomicUsize::new(0));
        let count = |closed: &Arc<AtomicUsize>| {
            let closed = closed.clone();
            move || {
                closed.fetch_add(1, Ordering::SeqCst);
            }
        };
        let held = broker.register(&credential("sha256:aaaaaaaaaaaa"), count(&closed)).unwrap();
        let other = broker.register(&credential("sha256:bbbbbbbbbbbb"), count(&closed)).unwrap();
        let finished = broker.register(&credential("sha256:aaaaaaaaaaaa"), count(&closed)).unwrap();
        drop(finished);
        broker.revoke("sha256:aaaaaaaaaaaa");
        assert_eq!(closed.load(Ordering::SeqCst), 1, "only the running operation on that value");
        assert!(broker.is_revoked("sha256:aaaaaaaaaaaa"));
        assert!(matches!(broker.register(&credential("sha256:aaaaaaaaaaaa"), || {}), Err(Refusal::Revoked(_))));
        drop((held, other));
    }

    #[test]
    fn a_credentials_debug_never_shows_its_value() {
        let shown = format!("{:?}", credential("sha256:aaaaaaaaaaaa"));
        assert!(shown.contains("sha256:aaaaaaaaaaaa") && !shown.contains("sk-secret"), "{shown}");
    }
}
