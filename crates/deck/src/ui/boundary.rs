//! The boundary card's state: pending host approvals for this window's task.
//!
//! The design choice: the view is a mirror of the daemon's queue, never the
//! authority. A watch connection carries the item changes; a decision goes
//! back over the control socket, and the daemon's reply (or its next watch
//! update) is what removes the card. Pure state, so a card sequence is
//! testable without a window.

use deck_runtime::approvals::{Approval, State};

#[derive(Default)]
pub struct Boundary {
    items: Vec<Approval>,
}

impl Boundary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply an item update. Only pending items are kept; a decided or
    /// expired item closes its card.
    pub fn apply(&mut self, approval: Approval) {
        let pending = approval.state == State::Pending;
        match self.items.iter_mut().find(|item| item.id == approval.id) {
            Some(slot) => {
                if pending {
                    *slot = approval;
                } else {
                    let id = approval.id;
                    self.items.retain(|item| item.id != id);
                }
            }
            None if pending => self.items.push(approval),
            None => {}
        }
    }

    pub fn pending(&self) -> Option<&Approval> {
        self.items.first()
    }

    pub fn remove(&mut self, id: &str) {
        self.items.retain(|item| item.id != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_runtime::approvals::{Details, Scope};

    fn item(id: &str, state: State) -> Approval {
        Approval {
            id: id.into(),
            task: "t".into(),
            project: "/w".into(),
            session: None,
            gateway: "model".into(),
            operation: "chat_completions".into(),
            destination: "other".into(),
            credential: Some("OPENAI_API_KEY".into()),
            details: Details::default(),
            reason: None,
            created_ms: 0,
            expires_ms: 0,
            policy: 0,
            state,
        }
    }

    #[test]
    fn updates_upsert_and_a_decision_closes_the_card() {
        let mut boundary = Boundary::new();
        boundary.apply(item("a", State::Pending));
        boundary.apply(item("a", State::Pending));
        assert_eq!(boundary.pending().map(|item| item.id.as_str()), Some("a"));
        boundary.apply(item("a", State::Allowed(Scope::Task)));
        assert!(boundary.pending().is_none());
    }

    #[test]
    fn a_decided_item_never_opens_a_card() {
        let mut boundary = Boundary::new();
        boundary.apply(item("a", State::Denied));
        assert!(boundary.pending().is_none());
    }
}
