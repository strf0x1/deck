//! Boundary events for this task, produced by the GitHub gateway (ADR 0004
//! §6, ADR 0007 §3).
//!
//! The design choice: a blocked publish is reported beside the model's and the
//! web's, with `gateway: "github"`, so an agent that sees `git` fail or a
//! `deck_boundary_denied` body learns the same facts it reads for any other
//! block. Only this gateway writes here.

use serde::Serialize;
use std::sync::{Mutex, MutexGuard};

/// Events kept for the agent's next tool result.
const EVENTS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    pub seq: u64,
    pub approval_id: String,
    pub gateway: String,
    pub destination: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

struct Inner {
    seq: u64,
    events: Vec<Event>,
}

pub struct Events {
    inner: Mutex<Inner>,
}

impl Events {
    pub fn new() -> Self {
        Self { inner: Mutex::new(Inner { seq: 0, events: Vec::new() }) }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Note what happened to one item. A decision is final: a later report
    /// cannot reopen an item the agent already saw resolved.
    pub fn observe(&self, id: &str, destination: &str, state: &str, scope: Option<&str>, reason: Option<&str>) {
        let mut inner = self.lock();
        if let Some(previous) = inner.events.iter_mut().rev().find(|event| event.approval_id == id) {
            if previous.state != "pending" {
                return;
            }
            if previous.state == state && previous.scope.as_deref() == scope {
                return;
            }
            previous.state = state.to_owned();
            previous.scope = scope.map(str::to_owned);
            previous.reason = reason.map(str::to_owned);
            return;
        }
        // A write that succeeded inside the hold has nothing for the agent to
        // learn; only a block is an event.
        if matches!(state, "allowed" | "expired" | "withdrawn") {
            return;
        }
        inner.seq += 1;
        let seq = inner.seq;
        inner.events.push(Event {
            seq,
            approval_id: id.to_owned(),
            gateway: crate::github::GATEWAY.into(),
            destination: destination.to_owned(),
            state: state.to_owned(),
            scope: scope.map(str::to_owned),
            reason: reason.map(str::to_owned),
        });
        if inner.events.len() > EVENTS {
            let excess = inner.events.len() - EVENTS;
            inner.events.drain(..excess);
        }
    }

    /// Events with a sequence greater than `after`, in order.
    pub fn since(&self, after: u64) -> Vec<Event> {
        let inner = self.lock();
        inner.events.iter().filter(|event| event.seq > after).cloned().collect()
    }
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_is_one_event_that_a_decision_updates() {
        let events = Events::new();
        events.observe("a1", "o/r#deck/t-1", "pending", None, Some("publish"));
        events.observe("a1", "o/r#deck/t-1", "denied", None, None);
        let listed = events.since(0);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, "denied");
        assert_eq!(listed[0].gateway, "github");
    }

    #[test]
    fn a_held_success_is_not_an_event() {
        let events = Events::new();
        events.observe("a1", "o/r#deck/t-1", "allowed", Some("once"), None);
        assert!(events.since(0).is_empty());
    }
}
