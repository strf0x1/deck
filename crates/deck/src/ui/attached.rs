//! Everything one task's attachment owns while the window holds it: the
//! daemon handle, the ACP connection, the approval lease, the session and its
//! boundary card.
//!
//! The design choice: all of it lives in one `Attached`, keyed by task id in
//! `Deck::attached`, so selecting another task only moves the selection and
//! never drops a live connection (ADR 0010 §8). The transcript list, the
//! composer layout and the status row stay on `Deck`; each task keeps its
//! editor, including its draft, selection and prompt recall.

use super::boundary::Boundary;
use super::editor::Editor;
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
    pub(super) editor: Editor,
    pub(super) boundary: Boundary,
    /// Whether the agent advertised GitHub operations; `None` until ready.
    pub(super) agent_publishes: Option<bool>,
    /// The connection or operation this attachment's replies are valid for.
    pub(super) generation: u64,
    /// The latest turn number and state, kept even while not selected.
    pub(super) turn: Option<(u64, TurnState)>,
    /// A prompt sent with an idempotency key and not answered yet (contract
    /// 1.3.0 §12.3). It outlives the connection: a restart resends it with the
    /// same key, and an agent that already took it starts no second turn.
    pub(super) in_flight: Option<InFlight>,
}

/// A keyed prompt waiting for its answer.
#[derive(Debug, Clone, PartialEq)]
pub struct InFlight {
    pub key: String,
    pub text: String,
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
            editor: Editor::new(),
            boundary: Boundary::new(),
            agent_publishes: None,
            generation: 0,
            turn: None,
            in_flight: None,
        }
    }

    pub fn task_id(&self) -> Option<&str> {
        self.task.as_ref().map(WindowTask::id)
    }

    /// Keep a prompt sent with `key` until the agent answers it.
    pub(super) fn keyed(&mut self, key: String, text: &str) {
        self.in_flight = Some(InFlight { key, text: text.to_string() });
    }

    /// The agent answered the prompt sent with `key`: nothing to resend.
    pub(super) fn settled(&mut self, key: &str) {
        if self.in_flight.as_ref().is_some_and(|f| f.key == key) {
            self.in_flight = None;
        }
    }

    /// The prompt to resend on a rebuilt connection. Only to an agent that
    /// dedupes by key: a 1.2 agent would run a prompt it already took again,
    /// so that one is the user's to resend (§12.5).
    pub(super) fn resend(&self, idempotent: bool) -> Option<InFlight> {
        self.in_flight.clone().filter(|_| idempotent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keyed_prompt_is_resent_until_answered_and_only_to_an_agent_that_dedupes() {
        let mut a = Attached::unattached();
        assert_eq!(a.resend(true), None);
        a.keyed("k1".into(), "ship it");
        let pending = InFlight { key: "k1".into(), text: "ship it".into() };
        assert_eq!(a.resend(true), Some(pending.clone()));
        assert_eq!(a.resend(false), None, "a 1.2 agent would run it twice");
        a.settled("k0");
        assert_eq!(a.resend(true), Some(pending), "another prompt's answer settles nothing");
        a.settled("k1");
        assert_eq!(a.resend(true), None);
    }
}
