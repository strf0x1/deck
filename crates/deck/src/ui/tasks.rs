//! The sessions column from `deckd`'s task list: one card per task that is not
//! destroyed, the window's own task first as the live card.
//!
//! The design choice: lane ids are the window's, task ids are the daemon's,
//! and [`Lanes`] is the only place that maps one to the other, so a task
//! keeps its card (and the card its place and motion) across refreshes. The
//! window's own card stays [`LIVE`]: its state comes from the live session;
//! its title from its record, like every other card (ADR 0010 §9), except
//! that a merged publication marks it done.

use super::board::{Lane, LaneState};
use super::publication::{self, View};
use super::root::LIVE;
use super::session::Status;
use deck_runtime::pull_request::State as Published;
use deck_runtime::{State, Task};
use std::collections::HashMap;

/// A task's human title: what `deckd` recorded from its first prompt, or a
/// stand-in made from its id. Never its directory, and never its agent's
/// name (ADR 0010 §9).
pub fn title(task: &Task) -> String {
    task.title.clone().unwrap_or_else(|| untitled(&task.id))
}

/// The title a task has before one is recorded: "Untitled" and the first six
/// characters of its id.
fn untitled(id: &str) -> String {
    let head: String = id.chars().take(6).collect();
    format!("Untitled · {head}")
}

/// The title a prompt gives its task: its first non-empty line, with inner
/// whitespace collapsed to single spaces. `None` when it has none. The
/// 60-character cap is `deckd`'s.
pub fn title_of(prompt: &str) -> Option<String> {
    prompt.lines().map(|line| line.split_whitespace().collect::<Vec<_>>().join(" ")).find(|line| !line.is_empty())
}

/// What one task's card says, from its record and its publication.
pub fn lane(id: usize, task: &Task, view: Option<&View>) -> Lane {
    let place = if task.backend == "vm" { " · vm" } else { "" };
    // The dot's label already says Working; the detail says where.
    let backend = if task.backend == "vm" { "vm" } else { "local" };
    let (state, detail) = match &task.state {
        _ if view.is_some_and(View::done) => (LaneState::Review, "done · merged".to_string()),
        State::Running => (LaneState::Working, backend.to_string()),
        State::Created => (LaneState::Idle, format!("created{place}")),
        State::Stopped { clean: true, .. } => (LaneState::Idle, format!("stopped{place}")),
        State::Stopped { reason, .. } => (LaneState::Waiting, format!("ended: {reason}")),
        State::Failed { .. } => (LaneState::Waiting, "needs a restart".to_string()),
        State::Destroyed => (LaneState::Idle, "destroyed".to_string()),
    };
    Lane { id, title: title(task), detail, state, chip: view.map(|v| (v.dot, v.label.clone())), attention: None }
}

/// A settled task and when it settled: the row of the column's fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    pub task: Task,
    pub at_ms: u64,
}

/// How long a settled task stays in the column (ADR 0010 §6).
pub const SETTLED_WINDOW_MS: u64 = 14 * 24 * 60 * 60 * 1000;

/// The settled tasks of the column, most recently settled first, dropping
/// those that settled longer than [`SETTLED_WINDOW_MS`] ago. Their records
/// stay; only the column forgets them.
pub fn settled(tasks: &[Task], published: &HashMap<String, Published>, own: Option<&str>, now_ms: u64) -> Vec<Settled> {
    let mut out: Vec<Settled> = tasks
        .iter()
        .filter(|task| own != Some(task.id.as_str()))
        .filter_map(|task| {
            let at_ms = super::attention::settled(task, published.get(&task.id))?;
            (now_ms.saturating_sub(at_ms) < SETTLED_WINDOW_MS).then(|| Settled { task: task.clone(), at_ms })
        })
        .collect();
    out.sort_by(|a, b| b.at_ms.cmp(&a.at_ms).then_with(|| a.task.id.cmp(&b.task.id)));
    out
}

/// The window's own card: the live session's state, except that a pending
/// question or boundary card needs the user first, and a merged publication
/// means the task is done (ADR 0008 §8).
pub fn live(status: &Status, ask: Option<&str>, boundary_pending: bool, view: Option<&View>) -> (LaneState, String) {
    if boundary_pending {
        (LaneState::Waiting, "asks: external access".to_string())
    } else if let Some(ask) = ask {
        (LaneState::Waiting, format!("asks: {ask}"))
    } else if view.is_some_and(View::done) {
        (LaneState::Review, "done · merged".to_string())
    } else {
        match status {
            Status::Connecting => (LaneState::Idle, "connecting…".to_string()),
            Status::Ready => (LaneState::Idle, "ready".to_string()),
            Status::Waiting => (LaneState::Waiting, "waiting for permission".to_string()),
            Status::Working => (LaneState::Working, "working".to_string()),
            Status::Detached => (LaneState::Working, "running · detached".to_string()),
            Status::Down(e) => (LaneState::Idle, e.clone()),
        }
    }
}

/// Lane ids for task ids, stable for the window's life.
#[derive(Default)]
pub struct Lanes {
    ids: HashMap<String, usize>,
    next: usize,
}

impl Lanes {
    fn id(&mut self, task: &str, own: Option<&str>) -> usize {
        if own == Some(task) {
            return LIVE;
        }
        if let Some(id) = self.ids.get(task) {
            return *id;
        }
        // LIVE is 0, so other lanes start at 1.
        self.next += 1;
        self.ids.insert(task.to_string(), self.next);
        self.next
    }

    /// The lane a task shows, or the one it takes when it has none yet.
    pub fn slot(&mut self, task: &str, own: Option<&str>) -> usize {
        self.id(task, own)
    }

    /// The lane a task already shows, without allocating one.
    pub fn lane_of(&self, task: &str, own: Option<&str>) -> Option<usize> {
        (own == Some(task)).then_some(LIVE).or_else(|| self.ids.get(task).copied())
    }

    /// The task a lane shows, or `None` for an unknown lane.
    pub fn task(&self, lane: usize, own: Option<&str>) -> Option<String> {
        if lane == LIVE {
            return own.map(str::to_string);
        }
        self.ids.iter().find(|(_, id)| **id == lane).map(|(task, _)| task.clone())
    }

    /// The window switches from task `from` to the task lane `lane` shows.
    /// The selected task becomes [`LIVE`], and `from` takes the selected
    /// card's lane, so the column keeps its shape: the two cards trade what
    /// they show. Returns the selected task, or `None` for the live lane or
    /// an unknown one.
    pub fn switch(&mut self, from: Option<&str>, lane: usize) -> Option<String> {
        if lane == LIVE {
            return None;
        }
        let to = self.task(lane, from)?;
        self.ids.remove(&to);
        if let Some(from) = from {
            self.ids.insert(from.to_string(), lane);
        }
        Some(to)
    }

    /// The other tasks' lanes, in the daemon's order. The live lane is not
    /// built here; its chip is set from `published` by the caller.
    pub fn lanes(
        &mut self,
        tasks: &[Task],
        own: Option<&str>,
        published: &HashMap<String, Published>,
        attention: &[super::attention::Item],
        now_ms: u64,
    ) -> Vec<Lane> {
        tasks
            .iter()
            .filter(|t| t.state != State::Destroyed && own != Some(t.id.as_str()))
            .map(|t| {
                let view = published.get(&t.id).and_then(|p| publication::view(p, now_ms));
                let mut lane = lane(self.id(&t.id, own), t, view.as_ref());
                lane.attention = attention.iter().find(|item| item.task == t.id).map(|item| item.reason.word());
                lane
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_runtime::Publication;
    use deck_runtime::pull_request::Live;
    use serde_json::json;

    fn task(id: &str, state: Value, backend: &str) -> Task {
        serde_json::from_value(json!({
            "id": id, "backend": backend, "spec": { "argv": ["construct", "acp"], "cwd": format!("/src/{id}-project") },
            "state": state, "pid": null,
        }))
        .unwrap()
    }
    use serde_json::Value;

    #[test]
    fn a_recorded_title_is_used_and_otherwise_the_id_stands_in() {
        assert_eq!(title(&task("abcdef123", json!({ "state": "running" }), "local")), "Untitled · abcdef");
        let named: Task = serde_json::from_value(json!({
            "id": "abcdef123", "backend": "local", "title": "fix the rail",
            "spec": { "argv": ["construct", "acp"], "cwd": "/src/a-project" },
            "state": { "state": "running" }, "pid": null,
        }))
        .unwrap();
        assert_eq!(title(&named), "fix the rail", "never the directory name");
        assert_eq!(lane(1, &named, None).title, "fix the rail");
    }

    #[test]
    fn a_prompt_titles_its_task_from_its_first_non_empty_line() {
        assert_eq!(title_of("\n  fix   the rail\nmore"), Some("fix the rail".into()));
        assert_eq!(title_of("one\ttwo  three"), Some("one two three".into()));
        assert_eq!(title_of("  \n\t\n"), None);
        assert_eq!(title_of(""), None);
    }

    #[test]
    fn each_record_becomes_a_card_with_its_title_state_and_place() {
        let running = lane(1, &task("a", json!({ "state": "running" }), "vm"), None);
        assert_eq!(
            (running.title.as_str(), running.detail.as_str(), running.state),
            ("Untitled · a", "vm", LaneState::Working)
        );
        let crashed =
            lane(2, &task("b", json!({ "state": "stopped", "clean": false, "reason": "exit 1" }), "local"), None);
        assert_eq!((crashed.detail.as_str(), crashed.state), ("ended: exit 1", LaneState::Waiting));
        assert_eq!(
            lane(3, &task("c", json!({ "state": "stopped", "clean": true, "reason": "" }), "local"), None).detail,
            "stopped"
        );
    }

    #[test]
    fn a_merged_pull_request_marks_the_card_done_and_shows_the_chip() {
        let merged = Published {
            publication: Some(Publication::PullRequest { number: 7, url: "u".into(), title: "t".into(), draft: false }),
            live: Some(Live { pull_request: Some(json!({ "state": "closed", "merged": true })), ..Live::default() }),
            fresh: false,
        };
        let view = publication::view(&merged, 0).unwrap();
        let lane = lane(1, &task("a", json!({ "state": "running" }), "vm"), Some(&view));
        assert_eq!((lane.state, lane.detail.as_str()), (LaneState::Review, "done · merged"));
        assert_eq!(lane.chip, Some((publication::Dot::Merged, "#7".into())));
    }

    #[test]
    fn the_live_card_waits_for_whoever_needs_it_then_reads_done_once_merged() {
        let merged = Published {
            publication: Some(Publication::PullRequest { number: 7, url: "u".into(), title: "t".into(), draft: false }),
            live: Some(Live { pull_request: Some(json!({ "state": "closed", "merged": true })), ..Live::default() }),
            fresh: false,
        };
        let view = publication::view(&merged, 0).unwrap();
        assert_eq!(live(&Status::Ready, None, false, None), (LaneState::Idle, "ready".into()));
        assert_eq!(live(&Status::Working, None, false, None), (LaneState::Working, "working".into()));
        assert_eq!(live(&Status::Ready, None, false, Some(&view)), (LaneState::Review, "done · merged".into()));
        assert_eq!(
            live(&Status::Ready, Some("shell rm"), false, Some(&view)),
            (LaneState::Waiting, "asks: shell rm".into())
        );
        assert_eq!(live(&Status::Ready, None, true, Some(&view)), (LaneState::Waiting, "asks: external access".into()));
    }

    #[test]
    fn lane_ids_are_stable_the_own_task_is_live_and_destroyed_tasks_leave() {
        let mut lanes = Lanes::default();
        let tasks = vec![
            task("own", json!({ "state": "running" }), "vm"),
            task("a", json!({ "state": "running" }), "vm"),
            task("b", json!({ "state": "destroyed" }), "vm"),
            task("c", json!({ "state": "created" }), "local"),
        ];
        let first: Vec<usize> =
            lanes.lanes(&tasks, Some("own"), &HashMap::new(), &[], 0).iter().map(|l| l.id).collect();
        assert_eq!(first.len(), 2, "the own task is the live card; a destroyed one has none");
        assert!(!first.contains(&LIVE));
        let again: Vec<usize> =
            lanes.lanes(&tasks[1..], Some("own"), &HashMap::new(), &[], 0).iter().map(|l| l.id).collect();
        assert_eq!(again, first, "a refresh keeps each task's lane");
        assert_eq!(lanes.task(first[0], Some("own")).as_deref(), Some("a"));
        assert_eq!(lanes.task(LIVE, Some("own")).as_deref(), Some("own"));
    }

    #[test]
    fn switching_trades_the_live_lane_with_the_selected_card() {
        let mut lanes = Lanes::default();
        let tasks = vec![
            task("own", json!({ "state": "running" }), "local"),
            task("a", json!({ "state": "running" }), "local"),
            task("b", json!({ "state": "running" }), "local"),
        ];
        let before = lanes.lanes(&tasks, Some("own"), &HashMap::new(), &[], 0);
        let (a, b) = (before[0].id, before[1].id);
        assert_eq!(lanes.switch(Some("own"), LIVE), None, "the live card is already the window's");
        assert_eq!(lanes.switch(Some("own"), 99), None, "an unknown lane selects nothing");
        assert_eq!(lanes.switch(Some("own"), b).as_deref(), Some("b"));
        let after = lanes.lanes(&tasks, Some("b"), &HashMap::new(), &[], 0);
        let shown: Vec<(usize, &str)> = after.iter().map(|l| (l.id, l.title.as_str())).collect();
        assert_eq!(shown, vec![(b, "Untitled · own"), (a, "Untitled · a")], "the previous task takes b's card");
        assert_eq!(lanes.task(LIVE, Some("b")).as_deref(), Some("b"));
        // And back: the first task is live again, b returns to its card.
        assert_eq!(lanes.switch(Some("b"), b).as_deref(), Some("own"));
        assert_eq!(lanes.task(b, Some("own")).as_deref(), Some("b"));
    }
}
