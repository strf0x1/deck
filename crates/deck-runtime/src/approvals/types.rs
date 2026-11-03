//! The plain approval vocabulary: what a request is, what can be decided,
//! what a grant allows, and the timing settings.
//!
//! The design choice: these types are the wire and disk shapes as well as the
//! view's model, so none of them carry a clock, a socket or a lock.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How long a pending item waits for a decision while a window is attached.
pub const HOLD: Duration = Duration::from_secs(20);
/// How long a pending item stays decidable before it expires.
pub const PENDING: Duration = Duration::from_secs(5 * 60);
/// How long an allow-once grant may be used by a retry.
pub const ONCE: Duration = Duration::from_secs(60);

/// What a guest request is asking to do across the boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Details {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// How broad an allowance is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Once,
    Task,
    Project,
}

impl Scope {
    /// The wire name used by the boundary status endpoint and notifications.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Task => "task",
            Self::Project => "project",
        }
    }
}

/// What the human chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Once,
    Task,
    Project,
    Deny,
}

impl Decision {
    pub(super) fn scope(self) -> Option<Scope> {
        match self {
            Self::Once => Some(Scope::Once),
            Self::Task => Some(Scope::Task),
            Self::Project => Some(Scope::Project),
            Self::Deny => None,
        }
    }
}

/// The item's state. `allowed` carries the scope it was allowed for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Allowed(Scope),
    Denied,
    Expired,
    Withdrawn,
}

impl State {
    pub fn pending(&self) -> bool {
        *self == Self::Pending
    }
}

/// One boundary approval as `deckd` records it and a window shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    pub task: String,
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub gateway: String,
    pub operation: String,
    pub destination: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    #[serde(default)]
    pub details: Details,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub created_ms: u64,
    pub expires_ms: u64,
    pub policy: u64,
    pub state: State,
    /// How many agents are long-polling this item through the guest's
    /// boundary status endpoint. The card says the agent is waiting while
    /// this is non-zero and that it has moved on otherwise.
    #[serde(default)]
    pub waiters: u32,
}

impl Approval {
    /// Whether two requests ask for the same thing and share a pending item.
    pub(super) fn same_request(&self, other: &Incoming) -> bool {
        self.task == other.task
            && self.gateway == other.gateway
            && self.operation == other.operation
            && self.destination == other.destination
    }
}

/// A gateway's request without the task record fields `deckd` adds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {
    pub gateway: String,
    pub operation: String,
    pub destination: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    #[serde(default)]
    pub details: Details,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Ask {
    pub(crate) fn into_incoming(self, task: String, project: String, session: Option<String>) -> Incoming {
        Incoming {
            task,
            project,
            session,
            gateway: self.gateway,
            operation: self.operation,
            destination: self.destination,
            credential: self.credential,
            details: self.details,
            reason: self.reason,
        }
    }
}

/// What the gateway asks `deckd`. `deckd` fills in the project and session
/// from the task record before it reaches the queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Incoming {
    pub task: String,
    #[serde(default)]
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub gateway: String,
    pub operation: String,
    pub destination: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    #[serde(default)]
    pub details: Details,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A live allowance. Task and project grants persist; once grants live in
/// memory for [`ONCE`] or until a request uses one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub gateway: String,
    pub operation: String,
    pub destination: String,
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<u64>,
}

impl Grant {
    pub(super) fn matches(&self, request: &Incoming) -> bool {
        self.gateway == request.gateway
            && self.operation == request.operation
            && self.destination == request.destination
    }
}

/// Timing knobs, fixed in production and shortened in tests.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub hold: Duration,
    pub pending: Duration,
    pub once: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self { hold: HOLD, pending: PENDING, once: ONCE }
    }
}
