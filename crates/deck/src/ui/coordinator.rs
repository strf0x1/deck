//! A project's coordinator as the window sees it (ADR 0011 §9): which task is
//! the project's coordinator, what its header says, where its workers came
//! from, and which backend a new one will run on.
//!
//! The design choice: the coordinator is found in `deckd`'s task list by kind
//! and project, never remembered by the window, so a coordinator another
//! window started, stopped or destroyed is what this one shows too. Destroyed
//! coordinators do not count, matching `deckd`'s one-per-scope rule.

use super::board::LaneState;
use super::session::Status;
use deck_runtime::{State, Task, TaskKind};

/// Whether `task` is a coordinator.
pub fn is_coordinator(task: &Task) -> bool {
    task.spec.kind == TaskKind::Coordinator
}

/// The project's coordinator: running, stopped or failed, never destroyed.
/// There is at most one per project and backend; the newest wins otherwise.
pub fn of_project<'a>(tasks: &'a [Task], project: &str) -> Option<&'a Task> {
    tasks
        .iter()
        .filter(|task| is_coordinator(task) && task.state != State::Destroyed)
        .filter(|task| task.spec.project.as_deref() == Some(project))
        .max_by(|a, b| a.id.cmp(&b.id))
}

/// What a worker's card and header say about who started it (ADR 0011 §6):
/// nothing for a task the user started.
pub fn provenance(task: &Task, tasks: &[Task]) -> Option<String> {
    let creator = task.spec.created_by.as_deref()?;
    let state = match tasks.iter().find(|t| t.id == creator).map(|t| &t.state) {
        Some(State::Running | State::Created) => return Some("started by coordinator".into()),
        Some(State::Destroyed) | None => "destroyed",
        Some(State::Stopped { .. }) => "stopped",
        Some(State::Failed { .. }) => "failed",
    };
    Some(format!("started by coordinator · {state}"))
}

/// The coordinator's status as its header chip says it, and the state colour
/// it wears. `status` is its attachment's, when the window has one.
pub fn status(task: &Task, status: Option<&Status>, asks: bool) -> (LaneState, String) {
    if asks {
        return (LaneState::Waiting, "Coordinator needs you".into());
    }
    match (&task.state, status) {
        (State::Running, Some(Status::Working)) => (LaneState::Working, "Coordinator working".into()),
        (State::Running, Some(Status::Waiting)) => (LaneState::Waiting, "Coordinator waiting".into()),
        (State::Running, Some(Status::Connecting) | None) => (LaneState::Idle, "Coordinator starting".into()),
        (State::Running, Some(Status::Down(_) | Status::Detached)) => {
            (LaneState::Idle, "Coordinator running, detached".into())
        }
        (State::Running, Some(Status::Ready)) => (LaneState::Idle, "Coordinator idle".into()),
        (State::Created, _) => (LaneState::Idle, "Coordinator not started".into()),
        (State::Stopped { clean: true, .. }, _) => (LaneState::Idle, "Coordinator stopped".into()),
        (State::Stopped { .. }, _) => (LaneState::Waiting, "Coordinator stopped unexpectedly".into()),
        (State::Failed { .. }, _) => (LaneState::Waiting, "Coordinator failed to start".into()),
        (State::Destroyed, _) => (LaneState::Idle, "Coordinator destroyed".into()),
    }
}

/// Why a coordinator the window did not start is down, for its restart card.
pub fn down(task: &Task) -> String {
    match &task.state {
        State::Failed { failure } => match failure {
            deck_runtime::Failure::StartFailed(why) | deck_runtime::Failure::ShutdownUnconfirmed(why) => why.clone(),
        },
        State::Stopped { clean: false, reason } => format!("ended: {reason}"),
        _ => "stopped".into(),
    }
}

/// Whether a connection failure is the agent refusing its session, as
/// Construct refuses a model below `optchat`'s window (deck#151 R6): an error
/// to `session/new` or `session/load`, not a broken stream.
pub fn refused_session(failure: &str) -> bool {
    failure.starts_with("session/new:") || failure.starts_with("session/load:")
}

/// The backend a new coordinator will run on: the one `deckd` ran the newest
/// task on, or the window's own guess from its environment before any task.
pub fn backend(tasks: &[Task], vm_configured: bool) -> String {
    match tasks.iter().max_by(|a, b| a.id.cmp(&b.id)) {
        Some(task) => task.backend.clone(),
        None if vm_configured => "vm".into(),
        None => "local".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn task(id: &str, kind: &str, project: &str, state: Value, created_by: Option<&str>) -> Task {
        serde_json::from_value(json!({
            "id": id, "backend": "local",
            "spec": { "argv": ["x"], "cwd": "/src", "project": project, "kind": kind, "created_by": created_by },
            "state": state, "pid": null,
        }))
        .unwrap()
    }

    #[test]
    fn the_project_coordinator_is_found_by_kind_and_project_and_a_destroyed_one_is_not() {
        let tasks = vec![
            task("01", "coordinator", "git:a", json!({ "state": "destroyed" }), None),
            task("02", "task", "git:a", json!({ "state": "running" }), None),
            task("03", "coordinator", "git:b", json!({ "state": "running" }), None),
        ];
        assert!(of_project(&tasks, "git:a").is_none(), "only a destroyed one: start the next");
        assert_eq!(of_project(&tasks, "git:b").map(|t| t.id.as_str()), Some("03"));
        let mut more = tasks.clone();
        more.push(task("04", "coordinator", "git:a", json!({ "state": "stopped", "clean": true, "reason": "" }), None));
        assert_eq!(of_project(&more, "git:a").map(|t| t.id.as_str()), Some("04"), "a stopped one is reused");
    }

    #[test]
    fn a_worker_names_its_coordinator_and_whether_it_still_runs() {
        let coordinator = task("c", "coordinator", "git:a", json!({ "state": "running" }), None);
        let worker = task("w", "task", "git:a", json!({ "state": "running" }), Some("c"));
        let mine = task("m", "task", "git:a", json!({ "state": "running" }), None);
        let mut tasks = vec![coordinator, worker.clone(), mine.clone()];
        assert_eq!(provenance(&mine, &tasks), None, "the user's own task says nothing");
        assert_eq!(provenance(&worker, &tasks).as_deref(), Some("started by coordinator"));
        tasks[0].state = State::Stopped { clean: true, reason: String::new() };
        assert_eq!(provenance(&worker, &tasks).as_deref(), Some("started by coordinator · stopped"));
        tasks[0].state = State::Destroyed;
        assert_eq!(provenance(&worker, &tasks).as_deref(), Some("started by coordinator · destroyed"));
        assert_eq!(provenance(&worker, &tasks[1..]).as_deref(), Some("started by coordinator · destroyed"));
    }

    #[test]
    fn the_header_chip_follows_the_record_then_the_live_session() {
        let running = task("c", "coordinator", "git:a", json!({ "state": "running" }), None);
        assert_eq!(status(&running, Some(&Status::Working), false), (LaneState::Working, "Coordinator working".into()));
        assert_eq!(status(&running, Some(&Status::Ready), true).0, LaneState::Waiting, "a question needs you");
        let stopped =
            task("c", "coordinator", "git:a", json!({ "state": "stopped", "clean": true, "reason": "" }), None);
        assert_eq!(status(&stopped, None, false).1, "Coordinator stopped");
        let failed = task(
            "c",
            "coordinator",
            "git:a",
            json!({ "state": "failed", "failure": { "kind": "start_failed", "detail": "the agent refused its session: small window" } }),
            None,
        );
        assert_eq!(status(&failed, None, false).1, "Coordinator failed to start");
        assert_eq!(down(&failed), "the agent refused its session: small window");
    }

    #[test]
    fn a_refused_session_is_told_from_a_broken_stream() {
        assert!(refused_session("session/new: optchat needs a 65536-token window (-32603)"));
        assert!(refused_session("session/load: no session (-32002)"));
        assert!(!refused_session("the agent closed the connection"));
    }

    #[test]
    fn a_new_coordinator_runs_where_deckd_ran_the_newest_task() {
        assert_eq!(backend(&[], false), "local");
        assert_eq!(backend(&[], true), "vm");
        let mut vm = task("02", "task", "git:a", json!({ "state": "running" }), None);
        vm.backend = "vm".into();
        let tasks = vec![task("01", "task", "git:a", json!({ "state": "running" }), None), vm];
        assert_eq!(backend(&tasks, false), "vm");
    }
}
