//! A task as a coordinator sees it (contract 1.4.0 §13.4), and when a `wait`
//! on it ends (§13.3). Pure: the daemon gathers the facts.
//!
//! The design choice: a summary is built from host records only (the task
//! record, the holder's turn and last-message files, the approval queue's
//! pending count, the pull request reader's last read) and carries nothing a
//! coordinator could use to reach past its surface: no transcript, no prompt
//! but the title, no credential.

use crate::lifecycle::{Failure, Phase, State, Task};
use crate::message::LastMessage;
use serde_json::{Value, json};

/// The most characters of a title a summary carries.
const TITLE_CHARS: usize = 80;
const REPLY_BYTES: usize = 1024 * 1024;

/// What the daemon reads beside the record.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub pending_approvals: usize,
    /// The pull request reader's last `github-review-response`, if any.
    pub pull_request: Option<Value>,
    pub message: Option<LastMessage>,
}

/// The lifecycle state's wire name.
pub fn state_name(state: &State) -> &'static str {
    match state {
        State::Created => "created",
        State::Running => "running",
        State::Stopped { .. } => "stopped",
        State::Failed { .. } => "failed",
        State::Destroyed => "destroyed",
    }
}

/// Why the task needs a person now (ADR 0010 §6), as the window would say.
pub fn attention(task: &Task, pending_approvals: usize) -> Option<&'static str> {
    if matches!(task.state, State::Failed { .. }) || matches!(&task.state, State::Stopped { clean: false, .. }) {
        return Some("failed");
    }
    if task.state != State::Running {
        return None;
    }
    if pending_approvals > 0 {
        return Some("approval");
    }
    match &task.turn {
        Some(turn) if turn.state == Phase::Waiting => Some("question"),
        Some(turn) if turn.state == Phase::Idle && turn.number > task.seen && turn.number > 0 => Some("finished"),
        _ => None,
    }
}

/// One `control-task-summary`.
pub fn summary(task: &Task, facts: &Facts) -> Value {
    let turn = task.turn.as_ref().map(|turn| {
        let state = match turn.state {
            Phase::Working => "working",
            Phase::Waiting => "waiting",
            Phase::Idle => "idle",
        };
        let mut value = json!({ "number": turn.number, "state": state });
        if let (Phase::Idle, Some(reason)) = (turn.state, &turn.stop_reason) {
            value["stopReason"] = json!(reason);
        }
        value
    });
    let title = task.title.as_ref().map(|title| title.chars().take(TITLE_CHARS).collect::<String>());
    let mut value = json!({
        "id": task.id,
        "project": task.spec.project_key(),
        "title": title,
        "state": state_name(&task.state),
        "turn": turn,
        "attention": attention(task, facts.pending_approvals),
        "pending_approvals": facts.pending_approvals,
        "publication": task.publication,
        "pull_request": facts.pull_request.as_ref().and_then(pull_request),
        "created_by": task.spec.created_by,
        "last_message": facts.message.as_ref().map(|m| m.text.clone()),
    });
    if facts.message.as_ref().is_some_and(|m| m.truncated) {
        value["last_message_truncated"] = json!(true);
    }
    if let State::Failed { failure: Failure::StartFailed(why) | Failure::ShutdownUnconfirmed(why) } = &task.state {
        // Open replies: a reader that knows the field learns why.
        value["failure"] = json!(why.chars().take(1024).collect::<String>());
    }
    value
}

/// Preserve newest-first order and the whole project set within the control
/// reply limit, even when there is room for fewer than 200 task summaries.
pub fn listing(projects: Vec<Value>, tasks: impl Iterator<Item = Value>, mut truncated: bool) -> anyhow::Result<Value> {
    let mut reply = json!({ "tasks": [], "truncated": false, "projects": projects });
    let mut bytes = reply.to_string().len();
    if bytes > REPLY_BYTES {
        anyhow::bail!("the coordinator's project set exceeds the control reply limit");
    }
    let mut included = Vec::new();
    for task in tasks {
        let separator = usize::from(!included.is_empty());
        let size = task.to_string().len() + separator;
        if size > REPLY_BYTES - bytes {
            truncated = true;
            break;
        }
        included.push(task);
        bytes += size;
    }
    reply["tasks"] = json!(included);
    reply["truncated"] = json!(truncated);
    Ok(reply)
}

/// The contract's `pull_request` from a review reply, or `None` when the
/// reply lacks a field the contract requires.
fn pull_request(review: &Value) -> Option<Value> {
    Some(json!({
        "number": review["number"].as_u64().filter(|n| *n > 0)?,
        "state": review["state"].as_str().filter(|s| !s.is_empty())?,
        "draft": review["draft"].as_bool()?,
        "merged": review["merged"].as_bool()?,
        "html_url": review["html_url"].as_str().filter(|s| !s.is_empty())?,
    }))
}

/// What a `wait` compares: the task's state and turn and its pending count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub state: State,
    pub turn: Option<crate::Turn>,
    pub pending_approvals: usize,
}

impl Snapshot {
    pub fn of(task: &Task, pending_approvals: usize) -> Self {
        Snapshot { state: task.state.clone(), turn: task.turn.clone(), pending_approvals }
    }
}

/// Whether a `wait` that began at `start` ends at `now` (§13.3): the task
/// stopped, failed or was destroyed; a turn after `after_turn` is idle; or a
/// pending approval or a waiting question appeared since it began. A task
/// that has not started yet waits.
pub fn woke(start: &Snapshot, now: &Snapshot, after_turn: u64) -> bool {
    if matches!(now.state, State::Stopped { .. } | State::Failed { .. } | State::Destroyed) {
        return true;
    }
    if now.turn.as_ref().is_some_and(|t| t.state == Phase::Idle && t.number > after_turn) {
        return true;
    }
    let waiting = |s: &Snapshot| s.turn.as_ref().is_some_and(|t| t.state == Phase::Waiting);
    let new_question = waiting(now)
        && (!waiting(start) || now.turn.as_ref().map(|t| t.number) != start.turn.as_ref().map(|t| t.number));
    new_question || now.pending_approvals > start.pending_approvals
}

#[cfg(test)]
#[path = "summary_tests.rs"]
mod tests;
