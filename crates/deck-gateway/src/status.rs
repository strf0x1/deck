//! The boundary status kind served on the extension port (ADR 0004 §6):
//! boundary events for this task, one item's state (long-polled), and an
//! agent's withdrawal of a pending item.
//!
//! The design choice: this endpoint can only read and withdraw. It never
//! decides, so nothing on it can widen the boundary if the guest forges a
//! request. The gateway is the task's only writer; `deckd` remains the
//! authority for the item and the decision.

use crate::control::{Control, State as Approval};
use crate::http;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The host caps a guest's requested wait; Construct clamps to the same.
const MAX_WAIT: u64 = 300;
/// Events are kept bounded; an agent only reads the tail after its own tool.
const EVENTS: usize = 512;
/// Pending items are refreshed from the daemon at most this often, so a
/// flooding guest cannot turn each `/events` into N control requests.
const REFRESH: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    pub seq: u64,
    pub approval_id: String,
    /// The gateway kind that produced the event. One process serves the
    /// model gateway and its status kind today.
    pub gateway: String,
    pub destination: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
}

struct Inner {
    seq: u64,
    events: Vec<Event>,
    refreshed: Option<Instant>,
}

pub struct Status {
    control: Option<Control>,
    inner: Mutex<Inner>,
}

impl Status {
    pub fn new(control: Option<Control>) -> Arc<Self> {
        Arc::new(Self { control, inner: Mutex::new(Inner { seq: 0, events: vec![], refreshed: None }) })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Note what happened to one item. A successful request that never had a
    /// denied or held attempt is not a boundary event; a decision changes the
    /// event the agent already knows about.
    pub fn observe(&self, id: &str, destination: &str, state: &str, scope: Option<&str>, expires_at_ms: Option<u64>) {
        let mut inner = self.lock();
        let previous = inner.events.iter().rposition(|event| event.approval_id == id);
        let destination = match previous {
            Some(index) if destination.is_empty() => inner.events[index].destination.clone(),
            _ => destination.to_owned(),
        };
        match previous {
            Some(index) => {
                let previous = &inner.events[index];
                // A decision is final; a later report cannot reopen or change it.
                if previous.state != "pending" {
                    return;
                }
                if previous.state == state && previous.destination == destination {
                    return;
                }
            }
            // An allowed request with no prior block succeeded inside the
            // hold, so the agent has nothing to learn from an event.
            None if matches!(state, "allowed" | "expired" | "withdrawn") => return,
            None => {}
        }
        inner.seq += 1;
        let seq = inner.seq;
        inner.events.push(Event {
            seq,
            approval_id: id.to_owned(),
            gateway: "model".into(),
            destination,
            state: state.to_owned(),
            scope: scope.map(str::to_owned),
            expires_at_ms,
        });
        if inner.events.len() > EVENTS {
            let excess = inner.events.len() - EVENTS;
            inner.events.drain(..excess);
        }
    }

    /// Serve one HTTP request on a connection that already named this kind.
    /// `web` carries the web gateway's boundary events, so a plain-HTTP block
    /// is visible to the agent beside the model's; a tunnel's own events are
    /// not here because the gateway keeps them and reports the decision.
    pub fn handle(&self, mut stream: UnixStream, web: &super::web::Events) -> io::Result<()> {
        // Best effort: macOS refuses a timeout on a peer that already closed,
        // and the subsequent read reports that EOF anyway.
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(MAX_WAIT + 5)));
        let mut reader = BufReader::new(&stream);
        let request = match http::read(&mut reader) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(()),
            Err(http::ReadError::Refused { status, message }) => {
                return http::write_error(&mut stream, status, "deck_bad_request", &message);
            }
            Err(http::ReadError::Io(error)) => return Err(error),
        };
        let (status, body) = self.route(&request.method, &request.target, web);
        http::write(&mut stream, status, &[("content-type", "application/json".into())], body.as_bytes())
    }

    fn route(&self, method: &str, target: &str, web: &super::web::Events) -> (u16, String) {
        let path = target.split('?').next().unwrap_or_default();
        if method == "GET" && path == "/events" {
            let after = query(target, "after").and_then(|value| value.parse().ok()).unwrap_or(0);
            return (200, self.events(after, web));
        }
        if let Some(rest) = path.strip_prefix("/approvals/") {
            if let Some(id) = rest.strip_suffix("/withdraw") {
                if method == "POST" {
                    return self.withdraw(id);
                }
            } else if method == "GET" {
                let wait = query(target, "wait").and_then(|value| value.parse().ok()).unwrap_or(0);
                return self.approval(rest, wait);
            }
        }
        (404, json!({ "error": "not found" }).to_string())
    }

    /// The model gateway's events and the web gateway's in one ordered list.
    /// Each sequence starts at its own zero, so the list is by time of
    /// observation within a kind; an agent reads both tails after a tool.
    fn events(&self, after: u64, web: &super::web::Events) -> String {
        self.refresh();
        let inner = self.lock();
        let model = inner.events.iter().filter(|event| event.seq > after).cloned();
        let events: Vec<serde_json::Value> = model
            .map(|event| serde_json::to_value(event).unwrap_or_default())
            .chain(web.since(after).into_iter().map(|event| serde_json::to_value(event).unwrap_or_default()))
            .collect();
        serde_json::to_string(&events).unwrap_or_else(|_| "[]".into())
    }

    fn approval(&self, id: &str, wait: u64) -> (u16, String) {
        if !valid(id) {
            return (404, json!({ "error": "no such approval" }).to_string());
        }
        let Some(control) = &self.control else {
            return (503, json!({ "error": "the approval queue is unreachable" }).to_string());
        };
        let state = match control.status(id) {
            Ok(Some(state)) => state,
            Ok(None) => return (404, json!({ "error": "no such approval" }).to_string()),
            Err(error) => return (503, json!({ "error": error }).to_string()),
        };
        let state = if wait > 0 && state.state == "pending" {
            match control.wait(id, Duration::from_secs(wait.min(MAX_WAIT))) {
                Ok(Some(state)) => state,
                Ok(None) => return (404, json!({ "error": "no such approval" }).to_string()),
                Err(error) => return (503, json!({ "error": error }).to_string()),
            }
        } else {
            state
        };
        if state.state != "pending" {
            self.observe(&state.id, &state.destination, &state.state, state.scope.as_deref(), state.expires_at_ms);
        }
        (200, body(&state))
    }

    fn withdraw(&self, id: &str) -> (u16, String) {
        if !valid(id) {
            return (404, json!({ "error": "no such approval" }).to_string());
        }
        let Some(control) = &self.control else {
            return (503, json!({ "error": "the approval queue is unreachable" }).to_string());
        };
        match control.withdraw(id) {
            Ok(()) => {
                self.observe(id, "", "withdrawn", None, None);
                (200, "{}".into())
            }
            Err(error) => (400, json!({ "error": error }).to_string()),
        }
    }

    /// Bring the gateway's event log up to date with decisions taken while no
    /// request or wait was open. Debounced; pending items only.
    fn refresh(&self) {
        let Some(control) = &self.control else { return };
        let pending: Vec<String> = {
            let mut inner = self.lock();
            if inner.refreshed.is_some_and(|last| last.elapsed() < REFRESH) {
                return;
            }
            inner.refreshed = Some(Instant::now());
            latest(&inner)
                .into_iter()
                .filter(|event| event.state == "pending")
                .map(|event| event.approval_id.clone())
                .collect()
        };
        for id in pending {
            let Ok(Some(state)) = control.status(&id) else { continue };
            if state.state != "pending" {
                self.observe(&state.id, &state.destination, &state.state, state.scope.as_deref(), state.expires_at_ms);
            }
        }
    }
}

/// The latest event per approval id, oldest first.
fn latest(inner: &Inner) -> Vec<&Event> {
    let mut latest: HashMap<&str, &Event> = HashMap::new();
    for event in &inner.events {
        match latest.get(event.approval_id.as_str()) {
            Some(previous) if previous.seq > event.seq => {}
            _ => {
                latest.insert(&event.approval_id, event);
            }
        }
    }
    let mut events: Vec<&Event> = latest.into_values().collect();
    events.sort_by_key(|event| event.seq);
    events
}

fn body(state: &Approval) -> String {
    let mut body = json!({
        "approval_id": state.id,
        "destination": state.destination,
        "state": state.state,
        "scope": state.scope,
    });
    if let Some(expires_at_ms) = state.expires_at_ms {
        body["expires_at_ms"] = json!(expires_at_ms);
    }
    body.to_string()
}

fn query<'a>(target: &'a str, key: &str) -> Option<&'a str> {
    target.split('?').nth(1)?.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then_some(value)
    })
}

fn valid(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web_events() -> super::super::web::Events {
        super::super::web::Events::new()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("deck-status-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_held_request_is_an_event_but_a_hold_that_succeeds_is_not() {
        let status = Status::new(None);
        status.observe("a1", "other", "pending", None, None);
        assert_eq!(
            status.events(0, &web_events()),
            r#"[{"seq":1,"approval_id":"a1","gateway":"model","destination":"other","state":"pending"}]"#
        );
        status.observe("a1", "other", "allowed", Some("task"), None);
        let events = status.events(0, &web_events());
        assert!(events.contains(r#""state":"allowed""#), "{events}");
        assert!(events.contains(r#""scope":"task""#), "{events}");

        status.observe("a2", "other", "allowed", Some("once"), None);
        assert!(!status.events(0, &web_events()).contains("a2"), "a successful hold is not a block");

        status.observe("a3", "other", "pending", None, None);
        status.observe("a3", "other", "allowed", Some("timed"), Some(1_800_000_300_000));
        let events = status.events(0, &web_events());
        assert!(events.contains(r#""scope":"timed""#), "{events}");
        assert!(events.contains(r#""expires_at_ms":1800000300000"#), "{events}");
    }

    #[test]
    fn a_decision_replaces_the_pending_event_and_never_reopens() {
        let status = Status::new(None);
        status.observe("a1", "other", "pending", None, None);
        status.observe("a1", "other", "denied", None, None);
        status.observe("a1", "other", "pending", None, None);
        let inner = status.lock();
        let latest = latest(&inner);
        assert_eq!(latest.len(), 1, "{:?}", inner.events);
        assert_eq!(latest[0].state, "denied");
        assert_eq!(inner.events.len(), 2, "the decision is a second event, not a rewrite");
    }

    #[test]
    fn event_sequence_survives_trimming_and_filters_by_after() {
        let status = Status::new(None);
        for i in 0..(EVENTS + 10) {
            status.observe(&format!("a{i}"), "d", "pending", None, None);
        }
        let inner = status.lock();
        assert_eq!(inner.events.len(), EVENTS);
        assert_eq!(inner.events.last().unwrap().seq, (EVENTS + 10) as u64);
        drop(inner);
        let events = status.events((EVENTS + 10 - 2) as u64, &web_events());
        assert!(events.contains(&format!("\"seq\":{}", EVENTS + 9)), "{events}");
        assert!(!events.contains(&format!("\"seq\":{}", EVENTS + 7)), "{events}");
    }

    #[test]
    fn query_and_ids_are_parsed_strictly() {
        assert_eq!(query("/events?after=4", "after"), Some("4"));
        assert_eq!(query("/approvals/a1?wait=9", "wait"), Some("9"));
        assert_eq!(query("/events", "after"), None);
        assert!(valid("a1f3-g2"));
        assert!(!valid(""));
        assert!(!valid("../x"));
    }

    #[test]
    fn a_connection_without_a_request_is_not_an_error() {
        let dir = scratch("empty");
        let path = dir.join("status.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = Status::new(None);
        let client = std::thread::spawn(move || {
            let stream = UnixStream::connect(&path).unwrap();
            drop(stream);
        });
        let (stream, _) = listener.accept().unwrap();
        server.handle(stream, &web_events()).unwrap();
        client.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
