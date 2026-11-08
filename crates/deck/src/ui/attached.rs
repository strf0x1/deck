//! Everything one task's attachment owns while the window holds it: the
//! daemon handle, the ACP connection, the approval lease, the session and its
//! boundary card.
//!
//! The design choice: all of it lives in one `Attached`, keyed by task id in
//! `Deck::attached`, so selecting another task only moves the selection and
//! never drops a live connection (ADR 0010 §8). The transcript list, the
//! composer and the status row stay on `Deck` because they show the selection.

use super::boundary::Boundary;
use super::session::Session;
use crate::task::WindowTask;
use deck_acp::{Client, TurnState};
use deck_runtime::Watch;
use std::sync::Arc;

pub struct Attached {
    /// The daemon handle. `None` only for a deck with no task at all: tests
    /// feed events to a detached window before any daemon exists.
    pub(super) task: Option<WindowTask>,
    /// The ACP connection. Dropping it detaches; it never stops the agent.
    pub(super) client: Option<Arc<Client>>,
    /// The approval-queue lease; dropping it ends this window's attachment.
    pub(super) watch: Option<Watch>,
    pub(super) session: Session,
    pub(super) boundary: Boundary,
    /// Whether the agent advertised GitHub operations; `None` until ready.
    pub(super) agent_publishes: Option<bool>,
    /// The connection or operation this attachment's replies are valid for.
    pub(super) generation: u64,
    /// The latest turn number and state, kept even while not selected.
    pub(super) turn: Option<(u64, TurnState)>,
}

impl Attached {
    pub fn new(task: WindowTask) -> Attached {
        Attached { task: Some(task), ..Attached::empty() }
    }

    /// State without a daemon handle, for a headless deck before launch.
    pub(super) fn unattached() -> Attached {
        Attached::empty()
    }

    fn empty() -> Attached {
        Attached {
            task: None,
            client: None,
            watch: None,
            session: Session::new(),
            boundary: Boundary::new(),
            agent_publishes: None,
            generation: 0,
            turn: None,
        }
    }

    pub fn task_id(&self) -> Option<&str> {
        self.task.as_ref().map(WindowTask::id)
    }
}
