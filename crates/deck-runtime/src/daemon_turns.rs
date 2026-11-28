//! `deckd`'s turn operations (ADR 0011 §3): prompt a task through its
//! holder, and long-poll its turn and state. Both can take seconds, so they
//! run on the client's connection without the service lock, through a
//! [`Runtime`] clone.
//!
//! The design choice: the holder decides whether a prompt can start, because
//! only it sees the stream; `deckd` records what it reports. A prompt's turn
//! comes from the turn the agent started, never from a response a later
//! window attachment might retire.

use crate::holder::{Reply, Request};
use crate::holder_acp::{Prompted, START};
use crate::wire::{PromptReply, WAIT_LIMIT_MS, Waited};
use crate::{Runtime, State, Task};
use anyhow::{Context as _, Result, bail};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(20);

/// Prompt `id` through its holder, recording the session the holder set up
/// and the prompt itself.
pub(crate) fn prompt(runtime: &Runtime, id: &str, text: &str, from: Option<&str>) -> Result<PromptReply> {
    ordinary(prompt_with_key(runtime, id, text, from, None)?)
}

/// The result of an explicitly keyed prompt can settle without a new turn:
/// the agent may answer a retry from its journal.
pub(crate) enum Delivery {
    Started { turn: u64 },
    Settled,
    Busy,
}

pub(crate) fn prompt_with_key(
    runtime: &Runtime,
    id: &str,
    text: &str,
    from: Option<&str>,
    key: Option<&str>,
) -> Result<Delivery> {
    let task = if key.is_some() { runtime.running_for_recovery(id)? } else { runtime.running_for_prompt(id)? };
    prompt_task(runtime, task, text, from, key)
}

/// A new worker's initial brief must not restart it if it was stopped while
/// its launch was in progress.
pub(crate) fn prompt_running(runtime: &Runtime, id: &str, text: &str, from: Option<&str>) -> Result<PromptReply> {
    runtime.ensure_no_recovery(id)?;
    let task = runtime.task(id)?;
    if task.state != State::Running {
        bail!("task {id} is not running");
    }
    ordinary(prompt_task(runtime, task, text, from, None)?)
}

fn ordinary(delivery: Delivery) -> Result<PromptReply> {
    match delivery {
        Delivery::Started { turn } => Ok(PromptReply::Started { turn }),
        Delivery::Busy => Ok(PromptReply::Busy),
        Delivery::Settled => bail!("an unkeyed prompt unexpectedly settled without starting"),
    }
}

fn prompt_task(runtime: &Runtime, task: Task, text: &str, from: Option<&str>, key: Option<&str>) -> Result<Delivery> {
    let id = &task.id;
    let holder = task.holder.as_ref().context("the task has no holder to prompt through")?;
    // The agent's directory as the window would name it (`deck/src/task.rs`).
    let cwd = if task.backend == "vm" { "/workspace".into() } else { task.spec.cwd.display().to_string() };
    let request = match key {
        Some(key) => Request::PromptKeyed { text: text.into(), cwd, session: task.session_id.clone(), key: key.into() },
        None => Request::Prompt { text: text.into(), cwd, session: task.session_id.clone() },
    };
    let reply = match crate::detached::exchange(holder, &request, START + Duration::from_secs(5)) {
        Ok(reply) => reply,
        Err(error) => {
            let error = format!("{error:#}");
            // An agent that refuses its session will refuse every prompt; the
            // record says so as a failed start (deck#151 R6).
            if refused_session(&error) {
                runtime.session_refused(id, &error)?;
            }
            bail!(error);
        }
    };
    match reply {
        Reply::Prompted(Prompted::Started { turn, session }) => {
            if let Some(session) = session {
                runtime.session(id, &session)?;
            }
            runtime.prompted(id, turn, from, text)?;
            Ok(Delivery::Started { turn })
        }
        Reply::Prompted(Prompted::Settled { session }) => {
            if let Some(session) = session {
                runtime.session(id, &session)?;
            }
            Ok(Delivery::Settled)
        }
        Reply::Prompted(Prompted::Busy) => Ok(Delivery::Busy),
        _ => bail!("the holder did not answer the prompt"),
    }
}

/// Whether the holder's error is the agent refusing to set its session up,
/// as the observer names its own failed requests.
fn refused_session(error: &str) -> bool {
    ["initialize:", "session/new:", "session/load:"].iter().any(|method| error.starts_with(method))
}

/// Wait until `id`'s turn or state changes, at most `wait_ms` (capped). With
/// `after_turn`, wait instead for a turn after that one to stop working (idle,
/// or waiting on a human) or for the task to stop running.
pub(crate) fn wait(runtime: &Runtime, id: &str, after_turn: Option<u64>, wait_ms: u64) -> Result<Waited> {
    let mut notices = runtime.subscribe();
    let start = runtime.task(id)?;
    if done(&start, &start, after_turn) {
        return Ok(Waited { task: start, timed_out: false });
    }
    let deadline = Instant::now() + Duration::from_millis(wait_ms.min(WAIT_LIMIT_MS));
    loop {
        let mut touched = false;
        while let Ok(notice) = notices.try_recv() {
            touched |= notice.task == id;
        }
        if touched {
            let now = runtime.task(id)?;
            if done(&start, &now, after_turn) {
                return Ok(Waited { task: now, timed_out: false });
            }
        }
        if Instant::now() >= deadline {
            return Ok(Waited { task: runtime.task(id)?, timed_out: true });
        }
        std::thread::sleep(POLL);
    }
}

fn done(start: &Task, now: &Task, after_turn: Option<u64>) -> bool {
    match after_turn {
        Some(after) => {
            now.state != State::Running
                || now.turn.as_ref().is_some_and(|t| t.number > after && t.state != crate::Phase::Working)
        }
        None => now.state != start.state || now.turn != start.turn,
    }
}

#[cfg(test)]
mod tests {
    use super::done;
    use crate::{Phase, State, Task, Turn};

    #[test]
    fn keyed_holder_request_is_not_an_old_unkeyed_request() {
        #[derive(serde::Deserialize)]
        enum OldHolderRequest {
            Prompt(serde_json::Value),
        }
        let old = super::Request::Prompt { text: "answer".into(), cwd: "/w".into(), session: Some("s".into()) };
        let Ok(OldHolderRequest::Prompt(value)) = serde_json::from_value(serde_json::to_value(old).unwrap()) else {
            panic!("an old holder must accept the existing prompt form");
        };
        assert_eq!(value["text"], "answer");
        let keyed = super::Request::PromptKeyed {
            text: "answer".into(),
            cwd: "/w".into(),
            session: Some("s".into()),
            key: "saved-key".into(),
        };
        assert!(serde_json::from_value::<OldHolderRequest>(serde_json::to_value(keyed).unwrap()).is_err());
    }

    fn task(state: State, turn: Option<(u64, Phase)>) -> Task {
        let mut task: Task = serde_json::from_value(serde_json::json!({
            "id": "t", "backend": "local", "spec": { "argv": ["a"], "cwd": "/w" },
            "state": { "state": "running" }, "pid": null,
        }))
        .unwrap();
        task.state = state;
        task.turn = turn.map(|(number, state)| Turn { number, state, stop_reason: None });
        task
    }

    #[test]
    fn after_a_turn_waits_for_a_later_one_to_stop_working() {
        let start = task(State::Running, Some((3, Phase::Idle)));
        assert!(!done(&start, &start, Some(3)));
        assert!(!done(&start, &task(State::Running, Some((4, Phase::Working))), Some(3)));
        assert!(done(&start, &task(State::Running, Some((4, Phase::Waiting))), Some(3)), "a question wakes it");
        assert!(done(&start, &task(State::Running, Some((4, Phase::Idle))), Some(3)));
        let stopped = State::Stopped { clean: true, reason: "stop".into() };
        assert!(done(&start, &task(stopped, Some((3, Phase::Idle))), Some(3)));
    }

    #[test]
    fn only_a_refused_setup_is_a_session_refusal() {
        use super::refused_session;
        let floor = "session/new: context.optchat needs a context window of at least 65536 tokens; m has 32768";
        assert!(refused_session(floor));
        assert!(refused_session("session/load: no such session"));
        assert!(!refused_session("session/prompt: model unavailable"));
        assert!(!refused_session("the holder did not answer the prompt"));
    }

    #[test]
    fn without_a_turn_any_change_wakes_it() {
        let start = task(State::Running, Some((3, Phase::Idle)));
        assert!(!done(&start, &start, None));
        assert!(done(&start, &task(State::Running, Some((4, Phase::Working))), None));
    }
}
