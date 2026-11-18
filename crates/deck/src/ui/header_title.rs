//! What the header's title block says: the selected task's title, its
//! project's name, and the status line the card's second line used to carry
//! (ADR 0010 §1, §9).
//!
//! The design choice: one plain value built from the board's live lane and the
//! task list, so the view, the control socket and their tests read the same
//! text instead of each deriving it.

use super::root::Deck;

/// The header's left block, in reading order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderTitle {
    /// The selected task's title; `None` when the project has no task yet.
    pub title: Option<String>,
    /// The selected project's name, shown as a faint chip beside the title.
    pub project: Option<String>,
    /// The selected lane's detail: the agent's status.
    pub detail: Option<String>,
    /// The project view's checkout and branch, under the project's name.
    pub place: Option<String>,
    /// Who started the selected task, when a coordinator did (ADR 0011 §6).
    pub provenance: Option<String>,
}

impl Deck {
    /// The selected task's title and status, and its project's name.
    pub fn header_title(&self) -> HeaderTitle {
        let project = self.project_name().map(str::to_owned);
        // The project view, the coordinator's included, is titled by its
        // project and where it is checked out (ADR 0011 §9).
        if self.showing_project() || self.task_id().is_none() {
            let place = self.project_checkout().map(|dir| {
                let branch = super::prompt::branch(&dir);
                let dir = super::prompt::tilde(&dir);
                branch.map_or_else(|| dir.clone(), |branch| format!("{dir} · {branch}"))
            });
            return HeaderTitle { project, place, ..HeaderTitle::default() };
        }
        let Some(id) = self.task_id() else { return HeaderTitle::default() };
        let lane = self.live_lane();
        let detail = self.board().cards().iter().find(|card| card.lane.id == lane).map(|card| card.lane.detail.clone());
        let provenance = self
            .tasks
            .iter()
            .find(|task| task.id == id)
            .and_then(|task| super::coordinator::provenance(task, &self.tasks));
        HeaderTitle { title: Some(self.task_title(id)), project, detail, place: None, provenance }
    }
}
