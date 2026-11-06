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
    reviewed: Option<String>,
}

impl Boundary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply an item update. Only pending items are kept; a decided or
    /// expired item closes its card. A decision the agent did not wait for is
    /// returned, because the agent would otherwise never learn the path is
    /// open (ADR 0004 §6).
    pub fn apply(&mut self, approval: Approval) -> Option<Approval> {
        let pending = approval.state == State::Pending;
        let decided = matches!(approval.state, State::Allowed(_) | State::Denied);
        match self.items.iter_mut().find(|item| item.id == approval.id) {
            Some(slot) => {
                if pending {
                    *slot = approval;
                    None
                } else {
                    // The decision carries how many waits were open when it
                    // was made, so a wait that ended between the decision and
                    // the watch update cannot suppress the note.
                    let waited = approval.waiters > 0;
                    let id = approval.id.clone();
                    if self.reviewed.as_deref() == Some(id.as_str()) {
                        self.reviewed = None;
                    }
                    self.items.retain(|item| item.id != id);
                    (decided && !waited).then_some(approval)
                }
            }
            None if pending => {
                self.items.push(approval);
                None
            }
            None => None,
        }
    }

    pub fn pending(&self) -> Option<&Approval> {
        self.items.first()
    }

    pub fn items(&self) -> &[Approval] {
        &self.items
    }

    /// A request explicitly opened with Review owns the decision keys until
    /// it leaves the queue, even if the agent also has a permission question.
    pub fn is_reviewing(&self) -> bool {
        self.reviewed.is_some()
    }

    pub fn select(&mut self, id: &str) {
        if let Some(index) = self.items.iter().position(|item| item.id == id) {
            self.reviewed = Some(id.to_owned());
            let item = self.items.remove(index);
            self.items.insert(0, item);
        }
    }

    pub fn remove(&mut self, id: &str) {
        if self.reviewed.as_deref() == Some(id) {
            self.reviewed = None;
        }
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
            grant_expires_ms: None,
            policy: 0,
            state,
            waiters: 0,
        }
    }

    #[test]
    fn updates_upsert_and_a_decision_closes_the_card() {
        let mut boundary = Boundary::new();
        assert!(boundary.apply(item("a", State::Pending)).is_none());
        assert!(boundary.apply(item("a", State::Pending)).is_none());
        assert_eq!(boundary.pending().map(|item| item.id.as_str()), Some("a"));
        assert!(boundary.apply(item("a", State::Allowed(Scope::Task))).is_some());
        assert!(boundary.pending().is_none());
    }

    #[test]
    fn a_decided_item_never_opens_a_card() {
        let mut boundary = Boundary::new();
        assert!(boundary.apply(item("a", State::Denied)).is_none());
        assert!(boundary.pending().is_none());
    }

    #[test]
    fn a_decision_the_agent_waited_for_needs_no_note() {
        let mut boundary = Boundary::new();
        boundary.apply(item("a", State::Pending));
        let mut waiting = item("a", State::Pending);
        waiting.waiters = 1;
        boundary.apply(waiting);
        let mut decided = item("a", State::Allowed(Scope::Task));
        decided.waiters = 1;
        assert!(boundary.apply(decided).is_none());
    }
}
