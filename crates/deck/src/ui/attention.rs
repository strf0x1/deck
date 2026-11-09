//! Which tasks need a person now, and when a task settled (ADR 0010 §6).
//!
//! The design choice: this is a pure fold over records plus each attachment's
//! last turn state, so the strip, the column's labels and the settled fold all
//! read one answer. `since` is the window's memory of when it first saw an
//! item, passed in and out, which is what makes the strip oldest-first.

use super::session::Entry;
use deck_acp::TurnState;
use deck_runtime::approvals::Approval;
use deck_runtime::pull_request::State as Published;
use deck_runtime::{State, Task};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Instant;

/// Why a task is in the strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// A host gateway asked for an external operation; `what` is its
    /// destination, such as a host or a repository.
    Approval { what: String },
    /// The agent waits for a permission answer; `what` is the question's
    /// title, such as a command.
    Question { what: String },
    /// A turn ended after the user last looked at the task.
    Finished { turn: u64, files: Option<usize> },
    /// The lifecycle record says the run ended without being asked to, or
    /// could not be shut down.
    Failed { why: String },
}

impl Reason {
    /// The few words the strip and the task card show.
    pub fn text(&self) -> String {
        match self {
            Reason::Approval { what } => format!("wants {what}"),
            Reason::Question { what } => format!("asks to run {what}"),
            Reason::Finished { files: Some(1), .. } => "finished: 1 file changed".into(),
            Reason::Finished { files: Some(n), .. } => format!("finished: {n} files changed"),
            Reason::Finished { files: None, .. } => "finished".into(),
            Reason::Failed { .. } => "stopped unexpectedly".into(),
        }
    }

    /// The one word the sidebar's Needs-you row and a task's own row show;
    /// the full [`Reason::text`] stays on hover and on the item it opens
    /// (ADR 0010 §7).
    pub fn word(&self) -> &'static str {
        match self {
            Reason::Approval { .. } => "wants",
            Reason::Question { .. } => "asks",
            Reason::Finished { .. } => "finished",
            Reason::Failed { .. } => "stopped",
        }
    }

    /// What makes two items the same reason for the same task. The turn
    /// number is part of a finish so a later turn is a new item.
    fn key(&self) -> String {
        match self {
            Reason::Approval { what } => format!("approval:{what}"),
            Reason::Question { what } => format!("question:{what}"),
            Reason::Finished { turn, .. } => format!("finished:{turn}"),
            Reason::Failed { why } => format!("failed:{why}"),
        }
    }
}

/// One entry in the "Needs you" strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub task: String,
    /// The task's project id, for selecting it.
    pub project: String,
    pub reason: Reason,
    pub since: Instant,
}

/// What one attachment knows, borrowed for the fold.
#[derive(Clone, Copy)]
pub struct Watched<'a> {
    pub turn: Option<(u64, &'a TurnState)>,
    pub question: Option<&'a str>,
    /// The attachment's pending boundary item, if any.
    pub approval: Option<&'a Approval>,
    /// Files the last turn changed, when the transcript can say.
    pub files: Option<usize>,
}

/// The strip's items, oldest first. `listed` holds `deckd`'s pending
/// approvals for each task; an attachment's own boundary item wins because the
/// watch connection is fresher.
///
/// `since` remembers when the window first saw each (task, reason). Entries
/// for reasons that are gone are dropped, so it stays as small as the strip.
pub fn items(
    tasks: &[Task],
    watched: &HashMap<String, Watched<'_>>,
    listed: &HashMap<String, Vec<Approval>>,
    since: &mut HashMap<String, Instant>,
    now: Instant,
) -> Vec<Item> {
    let empty = Vec::new();
    let mut out = Vec::new();
    let mut live = Vec::new();
    for task in tasks.iter().filter(|task| task.state != State::Destroyed) {
        let Some(reason) = reason(task, watched.get(&task.id), listed.get(&task.id).unwrap_or(&empty)) else {
            continue;
        };
        let key = format!("{}\u{1f}{}", task.id, reason.key());
        live.push(key.clone());
        let first = *since.entry(key).or_insert(now);
        out.push(Item { task: task.id.clone(), project: task.spec.project_key(), reason, since: first });
    }
    since.retain(|key, _| live.contains(key));
    out.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| a.task.cmp(&b.task)));
    out
}

/// The one reason a task has, in priority order: approval, question, an
/// unseen finished turn, then a lifecycle failure.
fn reason(task: &Task, watched: Option<&Watched<'_>>, listed: &[Approval]) -> Option<Reason> {
    if let Some(approval) = watched.and_then(|w| w.approval).or_else(|| listed.first()) {
        return Some(Reason::Approval { what: approval.destination.clone() });
    }
    if let Some(watched) = watched {
        if let Some((_, TurnState::Waiting)) = watched.turn
            && let Some(question) = watched.question
        {
            return Some(Reason::Question { what: question.to_string() });
        }
        if let Some((number, TurnState::Idle { stop_reason: Some(_) })) = watched.turn
            && number > task.seen
        {
            return Some(Reason::Finished { turn: number, files: watched.files });
        }
    }
    match &task.state {
        State::Stopped { clean: false, reason } => Some(Reason::Failed { why: reason.clone() }),
        State::Failed { failure } => Some(Reason::Failed { why: format!("{failure:?}") }),
        _ => None,
    }
}

/// The files the last turn changed, from the turn footer's own data.
pub fn changed_files(entries: &[Entry]) -> Option<usize> {
    let start = entries.iter().rposition(|entry| matches!(entry, Entry::You(_)))?;
    Some(super::transcript::touched(&entries[start..]).len())
}

/// When the task settled: destroyed, marked done, or its pull request merged
/// (ADR 0010 §6). A merge uses the last read's time, because GitHub's merge
/// time is not parsed here.
pub fn settled(task: &Task, pull: Option<&Published>) -> Option<u64> {
    if let Some(ms) = task.destroyed_ms {
        return Some(ms);
    }
    if let Some(ms) = task.done_ms {
        return Some(ms);
    }
    let live = pull?.live.as_ref()?;
    let merged = live.pull_request.as_ref()?.get("merged").and_then(Value::as_bool).unwrap_or(false);
    merged.then_some(live.read_at_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_runtime::approvals::State as ApprovalState;
    use serde_json::json;
    use std::time::Duration;

    fn task(id: &str, state: Value, seen: u64) -> Task {
        serde_json::from_value(json!({
            "id": id, "backend": "local",
            "spec": { "argv": ["agent"], "cwd": "/src/p", "project": "git:p" },
            "state": state, "pid": null, "seen": seen,
        }))
        .unwrap()
    }

    fn approval(id: &str, destination: &str) -> Approval {
        Approval {
            id: id.into(),
            task: "t".into(),
            project: "git:p".into(),
            session: None,
            gateway: "web".into(),
            operation: "connect".into(),
            destination: destination.into(),
            credential: None,
            details: Default::default(),
            reason: None,
            created_ms: 0,
            expires_ms: 0,
            grant_expires_ms: None,
            policy: 0,
            state: ApprovalState::Pending,
            waiters: 0,
        }
    }

    fn watched<'a>(
        turn: Option<(u64, &'a TurnState)>,
        question: Option<&'a str>,
        approval: Option<&'a Approval>,
    ) -> Watched<'a> {
        Watched { turn, question, approval, files: Some(2) }
    }

    fn at(now: Instant, ms: u64) -> Instant {
        now.checked_add(Duration::from_millis(ms)).unwrap()
    }

    #[test]
    fn each_reason_says_what_a_person_needs_in_a_few_words() {
        let mut since = HashMap::new();
        let running = task("0001", json!({ "state": "running" }), 1);
        let listed = HashMap::from([("0001".to_string(), vec![approval("a", "api.openai.com")])]);
        let now = Instant::now();
        let approval_items = items(std::slice::from_ref(&running), &HashMap::new(), &listed, &mut since, now);
        assert_eq!(approval_items[0].reason, Reason::Approval { what: "api.openai.com".into() });
        assert_eq!(approval_items[0].reason.text(), "wants api.openai.com");
        assert_eq!(approval_items[0].project, "git:p");

        let waiting = TurnState::Waiting;
        let attached = HashMap::from([("0001".into(), watched(Some((1, &waiting)), Some("shell rm -rf build"), None))]);
        let question = items(std::slice::from_ref(&running), &attached, &HashMap::new(), &mut since, now);
        assert_eq!(question[0].reason.text(), "asks to run shell rm -rf build");

        let idle = TurnState::Idle { stop_reason: Some("end_turn".into()) };
        let attached = HashMap::from([("0001".into(), watched(Some((2, &idle)), None, None))]);
        let finished = items(std::slice::from_ref(&running), &attached, &HashMap::new(), &mut since, now);
        assert_eq!(finished[0].reason, Reason::Finished { turn: 2, files: Some(2) });
        assert_eq!(finished[0].reason.text(), "finished: 2 files changed");
        let mut unknown = attached["0001"];
        unknown.files = None;
        let attached = HashMap::from([("0001".into(), unknown)]);
        let unknown_files = items(std::slice::from_ref(&running), &attached, &HashMap::new(), &mut since, now);
        assert_eq!(unknown_files[0].reason.text(), "finished");

        let crashed = task("0002", json!({ "state": "stopped", "clean": false, "reason": "exit 1" }), 0);
        let failed = items(std::slice::from_ref(&crashed), &HashMap::new(), &HashMap::new(), &mut since, now);
        assert_eq!(failed[0].reason.text(), "stopped unexpectedly");
    }

    #[test]
    fn each_reason_has_a_one_word_form() {
        assert_eq!(Reason::Approval { what: "api.openai.com".into() }.word(), "wants");
        assert_eq!(Reason::Question { what: "shell rm -rf build".into() }.word(), "asks");
        assert_eq!(Reason::Finished { turn: 4, files: None }.word(), "finished");
        assert_eq!(Reason::Failed { why: "exit 1".into() }.word(), "stopped");
    }

    #[test]
    fn a_reason_the_window_already_saw_keeps_its_place_and_new_ones_follow() {
        let mut since = HashMap::new();
        let a = task("0001", json!({ "state": "stopped", "clean": false, "reason": "exit 1" }), 0);
        let b = task("0002", json!({ "state": "stopped", "clean": false, "reason": "exit 1" }), 0);
        let now = Instant::now();
        let first = items(std::slice::from_ref(&a), &HashMap::new(), &HashMap::new(), &mut since, now);
        assert_eq!(first[0].since, now);
        let both = items(&[a.clone(), b.clone()], &HashMap::new(), &HashMap::new(), &mut since, at(now, 5_000));
        assert_eq!(both.iter().map(|item| item.task.as_str()).collect::<Vec<_>>(), ["0001", "0002"]);
        assert_eq!(both[0].since, now, "the first item keeps the time it was first seen");
        assert_eq!(both[1].since, at(now, 5_000));
        // Nothing left: the remembered keys go with it.
        items(&[], &HashMap::new(), &HashMap::new(), &mut since, at(now, 10_000));
        assert!(since.is_empty());
    }

    #[test]
    fn a_seen_turn_is_not_finished_and_destroyed_tasks_need_nothing() {
        let mut since = HashMap::new();
        let idle = TurnState::Idle { stop_reason: Some("end_turn".into()) };
        let seen = task("0001", json!({ "state": "running" }), 3);
        let attached = HashMap::from([("0001".into(), watched(Some((3, &idle)), None, None))]);
        let seen_items = items(std::slice::from_ref(&seen), &attached, &HashMap::new(), &mut since, Instant::now());
        assert!(seen_items.is_empty(), "seen == the turn number");
        let later = task("0002", json!({ "state": "running" }), 2);
        let attached = HashMap::from([("0002".into(), watched(Some((3, &idle)), None, None))]);
        let unseen = items(std::slice::from_ref(&later), &attached, &HashMap::new(), &mut since, Instant::now());
        assert_eq!(unseen.len(), 1);

        let destroyed = task("0003", json!({ "state": "destroyed" }), 0);
        let gone =
            items(std::slice::from_ref(&destroyed), &HashMap::new(), &HashMap::new(), &mut since, Instant::now());
        assert!(gone.is_empty());
    }

    #[test]
    fn settling_is_destroy_then_done_then_merge_read_time() {
        let mut destroyed = task("0001", json!({ "state": "destroyed" }), 0);
        destroyed.destroyed_ms = Some(10);
        destroyed.done_ms = Some(20);
        assert_eq!(settled(&destroyed, None), Some(10), "destroy is the latest fact");

        let mut done = task("0002", json!({ "state": "running" }), 0);
        done.done_ms = Some(20);
        assert_eq!(settled(&done, None), Some(20));
        assert_eq!(settled(&task("0003", json!({ "state": "running" }), 0), None), None);

        let running = task("0004", json!({ "state": "running" }), 0);
        let merged = Published {
            publication: None,
            live: Some(deck_runtime::pull_request::Live {
                read_at_ms: 42,
                pull_request: Some(json!({ "merged": true })),
                ..Default::default()
            }),
            fresh: false,
        };
        assert_eq!(settled(&running, Some(&merged)), Some(42));
        let open = Published {
            publication: None,
            live: Some(deck_runtime::pull_request::Live {
                read_at_ms: 43,
                pull_request: Some(json!({ "merged": false })),
                ..Default::default()
            }),
            fresh: false,
        };
        assert_eq!(settled(&running, Some(&open)), None);
    }
}
