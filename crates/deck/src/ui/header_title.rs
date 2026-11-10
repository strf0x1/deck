//! What the header's title block says: the selected task's title, its
//! project's name, and the status line the card's second line used to carry
//! (ADR 0010 §1, §9).
//!
//! The design choice: one plain value built from the board's live lane and the
//! task list, so the view, the control socket and their tests read the same
//! text instead of each deriving it.

use super::root::{Deck, LIVE};

/// The header's left block, in reading order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderTitle {
    /// The selected task's title; `None` when the project has no task yet.
    pub title: Option<String>,
    /// The selected project's name, shown as a faint chip beside the title.
    pub project: Option<String>,
    /// The selected lane's detail: the agent's status.
    pub detail: Option<String>,
}

impl Deck {
    /// The selected task's title and status, and its project's name.
    pub fn header_title(&self) -> HeaderTitle {
        let project = self.project_name().map(str::to_owned);
        let Some(id) = self.task_id() else {
            return HeaderTitle { title: None, project, detail: None };
        };
        let detail = self.board().cards().iter().find(|card| card.lane.id == LIVE).map(|card| card.lane.detail.clone());
        HeaderTitle { title: Some(self.task_title(id)), project, detail }
    }
}
