//! Boundary events for this task, produced by the web gateway (ADR 0004 §6).
//!
//! The design choice: the web gateway owns a sequence of its own, because a
//! CONNECT never carries an HTTP body and a client that reports "CONNECT
//! failed: 403" never shows the approval id. The agent reads the event to
//! learn which destination was blocked, why, and what the user decided. Only
//! this gateway writes here; the status endpoint on the extension port serves
//! the events of every kind it can see.

use serde::Serialize;
use std::sync::{Mutex, MutexGuard};

/// Events kept for the agent's next tool result. An agent reads the tail.
const EVENTS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    pub seq: u64,
    pub approval_id: String,
    /// Always `web` today; the field keeps a mixed stream legible.
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
        match inner.events.iter().rposition(|event| event.approval_id == id) {
            Some(index) => {
                let previous = &mut inner.events[index];
                // A decision is final; a later report cannot reopen or change
                // an item the agent already saw resolved.
                if previous.state != "pending" {
                    return;
                }
                if previous.state == state && previous.scope.as_deref() == scope {
                    return;
                }
                // The item keeps its sequence: the agent reads one event per
                // block, not one per state it passed through.
                previous.state = state.to_owned();
                previous.scope = scope.map(str::to_owned);
                previous.reason = reason.map(str::to_owned);
                return;
            }
            // An allowed request with no prior block succeeded inside the hold,
            // so the agent has nothing to learn from an event.
            None if matches!(state, "allowed" | "expired" | "withdrawn") => return,
            None => {}
        }
        inner.seq += 1;
        let seq = inner.seq;
        inner.events.push(Event {
            seq,
            approval_id: id.to_owned(),
            gateway: crate::web::GATEWAY.into(),
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
    fn a_blocked_request_is_one_event_and_a_decision_updates_it() {
        let events = Events::new();
        events.observe("a1", "registry.example:443", "pending", None, None);
        events.observe("a1", "registry.example:443", "allowed", Some("task"), None);
        events.observe("a1", "registry.example:443", "denied", None, None);
        let seen = events.since(0);
        assert_eq!(seen.len(), 1, "one item is one event whatever happens to it: {seen:?}");
        assert_eq!(seen[0].state, "allowed");
        assert_eq!(seen[0].scope.as_deref(), Some("task"));
        assert_eq!(seen[0].gateway, "web");
        assert!(events.since(seen[0].seq).is_empty());
    }

    #[test]
    fn a_request_that_never_had_a_block_leaves_no_event() {
        let events = Events::new();
        events.observe("a1", "crates.io:443", "allowed", Some("project"), None);
        events.observe("a2", "pypi.org:443", "withdrawn", None, None);
        assert!(events.since(0).is_empty());
    }
}
