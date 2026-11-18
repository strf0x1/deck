//! The control kind's client for `deckd`: one operation, one reply.
//!
//! The design choice: `deckd` names a refusal by its contract code and the
//! gateway turns it into a status, so the status table lives in one place
//! ([`super::wire::Code`]). Any failure to get an answer is
//! `deck_control_unavailable`, with a fixed message: the daemon's own error
//! text can name host paths, and the guest needs only to know to retry.

use super::wire::{Code, Operation, Refusal};
use crate::control::Control;
use serde_json::{Value, json};
use std::path::PathBuf;

/// What `deckd` answered.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The success body, sent to the guest as it is.
    Reply(Value),
    Refused(Refusal),
}

/// Where operations go; a test substitutes its own.
pub trait Daemon: Send + Sync {
    fn call(&self, coordinator: &str, operation: &Operation) -> Outcome;
}

/// The real `deckd`, over its control socket.
pub struct Deckd {
    socket: PathBuf,
}

impl Deckd {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }
}

impl Daemon for Deckd {
    fn call(&self, coordinator: &str, operation: &Operation) -> Outcome {
        let control = Control::new(self.socket.clone(), coordinator.to_owned());
        let request = json!({ "op": "control", "task": coordinator, "request": operation });
        match control.call_within(&request, operation.timeout()) {
            Ok(reply) => outcome(reply),
            Err(error) => {
                eprintln!("deck-gateway: control {} for {coordinator}: {error}", operation.name());
                Outcome::Refused(Refusal::new(Code::Unavailable, "deckd is not reachable"))
            }
        }
    }
}

/// `deckd`'s reply: `{"reply": body}` or `{"refused": {code, message}}`.
pub fn outcome(reply: Value) -> Outcome {
    if let Some(refused) = reply.get("refused") {
        let code = refused["code"].as_str().and_then(Code::named).unwrap_or(Code::Unavailable);
        let message = refused["message"].as_str().unwrap_or("deckd refused the operation");
        return Outcome::Refused(Refusal::new(code, message));
    }
    match reply.get("reply") {
        Some(body) if body.is_object() => Outcome::Reply(body.clone()),
        _ => Outcome::Refused(Refusal::new(Code::Unavailable, "deckd answered with no reply")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_daemon_reply_or_refusal_maps_to_the_contracts_status() {
        assert_eq!(outcome(json!({ "reply": { "id": "t" } })), Outcome::Reply(json!({ "id": "t" })));
        let Outcome::Refused(busy) = outcome(json!({ "refused": { "code": "deck_control_busy", "message": "m" } }))
        else {
            panic!("a refusal")
        };
        assert_eq!((busy.code, busy.status, busy.message.as_str()), (Code::Busy, 409, "m"));
        let Outcome::Refused(odd) = outcome(json!({ "refused": { "code": "made_up" } })) else { panic!("a refusal") };
        assert_eq!(odd.code, Code::Unavailable);
        assert!(matches!(outcome(json!(null)), Outcome::Refused(Refusal { code: Code::Unavailable, .. })));
    }

    #[test]
    fn an_unreachable_daemon_is_unavailable_with_a_fixed_message() {
        let deckd = Deckd::new(PathBuf::from("/nonexistent/deckd.sock"));
        let Outcome::Refused(refusal) = deckd.call("c1", &Operation::Tasks { project: None }) else {
            panic!("refused")
        };
        assert_eq!((refusal.code, refusal.status), (Code::Unavailable, 503));
        assert_eq!(refusal.message, "deckd is not reachable");
    }
}
