//! A task's lifecycle: the events its record holds and the state they fold
//! into. Pure: no files, no processes.
//!
//! The design choice: state is never stored on its own; it is the fold of the
//! record's events, and every event is checked against the state it would
//! follow before it is written. A record therefore cannot describe an
//! impossible history, and replaying one that does is an error, not a guess.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What to run: the agent command and the directory it runs in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
}

/// Why a task needs the user before it can run again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum Failure {
    /// The backend could not start it. Starting again is allowed.
    StartFailed(String),
    /// A stop could not confirm the process was gone, so nothing may reuse
    /// what it held. Only `destroy` leaves this state.
    ShutdownUnconfirmed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    Created,
    Running,
    /// `clean` is false when the process ended without being asked to.
    Stopped {
        clean: bool,
        reason: String,
    },
    Failed {
        failure: Failure,
    },
    Destroyed,
}

/// One line of a task's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Created { backend: String, spec: Spec },
    Started { pid: Option<u32> },
    Stopped { clean: bool, reason: String },
    Failed { failure: Failure },
    Destroyed,
}

/// A task as its record describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub backend: String,
    pub spec: Spec,
    pub state: State,
    /// The process the last `Started` named, while it runs.
    pub pid: Option<u32>,
}

impl Task {
    /// The task a record's first event creates.
    pub fn new(id: &str, first: &Event) -> Result<Task> {
        let Event::Created { backend, spec } = first else {
            bail!("a task record must start with `created`, not {}", name(first));
        };
        Ok(Task { id: id.into(), backend: backend.clone(), spec: spec.clone(), state: State::Created, pid: None })
    }

    /// Apply `ev`, or refuse it if it cannot follow the current state.
    pub fn apply(&mut self, ev: &Event) -> Result<()> {
        let next = match (&self.state, ev) {
            (
                State::Created | State::Stopped { .. } | State::Failed { failure: Failure::StartFailed(_) },
                Event::Started { .. },
            ) => State::Running,
            (State::Running, Event::Stopped { clean, reason }) => {
                State::Stopped { clean: *clean, reason: reason.clone() }
            }
            (
                State::Created | State::Stopped { .. } | State::Failed { failure: Failure::StartFailed(_) },
                Event::Failed { failure: f @ Failure::StartFailed(_) },
            )
            | (State::Running, Event::Failed { failure: f @ Failure::ShutdownUnconfirmed(_) }) => {
                State::Failed { failure: f.clone() }
            }
            (s, Event::Destroyed) if *s != State::Destroyed => State::Destroyed,
            (s, ev) => bail!("task {}: `{}` cannot follow {}", self.id, name(ev), describe(s)),
        };
        self.pid = match ev {
            Event::Started { pid } => *pid,
            _ => None,
        };
        self.state = next;
        Ok(())
    }

    /// Whether `start` applies: the task has never run.
    pub fn startable(&self) -> bool {
        self.state == State::Created
    }

    /// Whether `resume` applies: it ran before, or failed to start, and may
    /// run again.
    pub fn resumable(&self) -> bool {
        matches!(self.state, State::Stopped { .. } | State::Failed { failure: Failure::StartFailed(_) })
    }
}

fn name(ev: &Event) -> &'static str {
    match ev {
        Event::Created { .. } => "created",
        Event::Started { .. } => "started",
        Event::Stopped { .. } => "stopped",
        Event::Failed { .. } => "failed",
        Event::Destroyed => "destroyed",
    }
}

fn describe(s: &State) -> &'static str {
    match s {
        State::Created => "created",
        State::Running => "running",
        State::Stopped { .. } => "stopped",
        State::Failed { failure: Failure::StartFailed(_) } => "a failed start",
        State::Failed { failure: Failure::ShutdownUnconfirmed(_) } => "an unconfirmed shutdown",
        State::Destroyed => "destroyed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn created() -> Event {
        Event::Created { backend: "local".into(), spec: Spec { argv: vec!["agent".into()], cwd: "/w".into() } }
    }
    fn started() -> Event {
        Event::Started { pid: Some(7) }
    }
    fn stopped(clean: bool) -> Event {
        Event::Stopped { clean, reason: "why".into() }
    }
    fn start_failed() -> Event {
        Event::Failed { failure: Failure::StartFailed("no such file".into()) }
    }
    fn unconfirmed() -> Event {
        Event::Failed { failure: Failure::ShutdownUnconfirmed("pid 7 survived".into()) }
    }

    fn fold(events: &[Event]) -> Result<Task> {
        let mut t = Task::new("t", &events[0])?;
        for e in &events[1..] {
            t.apply(e)?;
        }
        Ok(t)
    }

    #[test]
    fn a_record_starts_with_created() {
        assert!(Task::new("t", &started()).is_err());
        let t = fold(&[created()]).unwrap();
        assert_eq!(t.state, State::Created);
        assert!(t.startable() && !t.resumable());
    }

    #[test]
    fn run_stop_resume_destroy() {
        let t = fold(&[created(), started()]).unwrap();
        assert_eq!((t.state.clone(), t.pid), (State::Running, Some(7)));
        let t = fold(&[created(), started(), stopped(false)]).unwrap();
        assert_eq!(t.state, State::Stopped { clean: false, reason: "why".into() });
        assert_eq!(t.pid, None);
        assert!(t.resumable() && !t.startable());
        let t = fold(&[created(), started(), stopped(true), started(), stopped(true), Event::Destroyed]).unwrap();
        assert_eq!(t.state, State::Destroyed);
    }

    #[test]
    fn a_failed_start_may_be_retried() {
        let t = fold(&[created(), start_failed()]).unwrap();
        assert!(t.resumable());
        assert_eq!(fold(&[created(), start_failed(), start_failed(), started()]).unwrap().state, State::Running);
    }

    #[test]
    fn an_unconfirmed_shutdown_only_leaves_by_destroy() {
        let t = fold(&[created(), started(), unconfirmed()]).unwrap();
        assert!(!t.resumable() && !t.startable());
        for next in [started(), stopped(true), start_failed()] {
            assert!(fold(&[created(), started(), unconfirmed(), next]).is_err());
        }
        assert_eq!(fold(&[created(), started(), unconfirmed(), Event::Destroyed]).unwrap().state, State::Destroyed);
    }

    #[test]
    fn impossible_histories_are_refused() {
        let bad: &[&[Event]] = &[
            &[created(), stopped(true)],             // never ran
            &[created(), started(), started()],      // already running
            &[created(), started(), start_failed()], // a running task did start
            &[created(), unconfirmed()],             // nothing to shut down
            &[created(), created()],                 // created twice
            &[created(), Event::Destroyed, started()],
            &[created(), Event::Destroyed, Event::Destroyed],
        ];
        for events in bad {
            assert!(fold(events).is_err(), "{events:?}");
        }
    }

    #[test]
    fn events_round_trip_as_tagged_json() {
        for e in [created(), started(), stopped(false), start_failed(), unconfirmed(), Event::Destroyed] {
            let line = serde_json::to_string(&e).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), e, "{line}");
        }
        assert_eq!(serde_json::to_string(&started()).unwrap(), r#"{"event":"started","pid":7}"#);
    }
}
