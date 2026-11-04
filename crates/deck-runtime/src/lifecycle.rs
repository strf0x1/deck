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

/// What the task's model gateway has spent. The gateway owns the live
/// counters and reports totals here; recording them on the task is what lets
/// a resumed gateway continue a budget instead of resetting it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub requests: u64,
    pub tokens: u64,
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

/// A detached holder's identity and ACP endpoint. The task pid remains the
/// agent process-group leader, so teardown can be confirmed after holder death.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    pub socket: PathBuf,
}

/// One line of a task's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Created {
        backend: String,
        spec: Spec,
    },
    Started {
        pid: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        holder: Option<Holder>,
    },
    Adopted,
    Session {
        id: String,
    },
    /// The model gateway's latest totals. This is accounting, not a
    /// lifecycle transition.
    Usage {
        requests: u64,
        tokens: u64,
    },
    /// The first time a run's gateway used a credential value: the name and
    /// its fingerprint, never the value (ADR 0004 §4.4). Accounting, not a
    /// lifecycle transition.
    CredentialUsed {
        name: String,
        fingerprint: String,
    },
    Stopped {
        clean: bool,
        reason: String,
    },
    Failed {
        failure: Failure,
    },
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
    #[serde(default)]
    pub holder: Option<Holder>,
    /// The agent's session pointer, retained when its process stops.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The model gateway's latest totals, retained across runs.
    #[serde(default)]
    pub usage: Usage,
}

impl Task {
    /// The task a record's first event creates.
    pub fn new(id: &str, first: &Event) -> Result<Task> {
        let Event::Created { backend, spec } = first else {
            bail!("a task record must start with `created`, not {}", name(first));
        };
        Ok(Task {
            id: id.into(),
            backend: backend.clone(),
            spec: spec.clone(),
            state: State::Created,
            pid: None,
            holder: None,
            session_id: None,
            usage: Usage::default(),
        })
    }

    /// Apply `ev`, or refuse it if it cannot follow the current state.
    pub fn apply(&mut self, ev: &Event) -> Result<()> {
        if matches!(ev, Event::Adopted) {
            if self.state != State::Running || self.holder.is_none() {
                bail!("task {} has no running holder to adopt", self.id);
            }
            return Ok(());
        }
        if let Event::Session { id } = ev {
            if self.state != State::Running {
                bail!("task {}: `session` cannot follow {}", self.id, describe(&self.state));
            }
            self.session_id = Some(id.clone());
            return Ok(());
        }
        if let Event::Usage { requests, tokens } = ev {
            if self.state == State::Destroyed {
                bail!("task {}: `usage` cannot follow destroyed", self.id);
            }
            // Totals only move forward. A late report from a previous gateway
            // is ignored rather than rewinding the record.
            if *requests >= self.usage.requests && *tokens >= self.usage.tokens {
                self.usage = Usage { requests: *requests, tokens: *tokens };
            }
            return Ok(());
        }
        if let Event::CredentialUsed { .. } = ev {
            if self.state != State::Running {
                bail!("task {}: `credential_used` cannot follow {}", self.id, describe(&self.state));
            }
            return Ok(());
        }
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
        let unconfirmed = matches!(ev, Event::Failed { failure: Failure::ShutdownUnconfirmed(_) });
        self.pid = match ev {
            Event::Started { pid, .. } => *pid,
            _ if unconfirmed => self.pid,
            _ => None,
        };
        self.holder = match ev {
            Event::Started { holder, .. } => holder.clone(),
            _ if unconfirmed => self.holder.clone(),
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
        Event::Adopted => "adopted",
        Event::Session { .. } => "session",
        Event::Usage { .. } => "usage",
        Event::CredentialUsed { .. } => "credential_used",
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
        Event::Started { pid: Some(7), holder: None }
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
    fn a_credential_use_is_recorded_only_while_running() {
        let used = Event::CredentialUsed { name: "OPENAI_API_KEY".into(), fingerprint: "sha256:aaaaaaaaaaaa".into() };
        let t = fold(&[created(), started(), used.clone()]).unwrap();
        assert_eq!(t.state, State::Running, "accounting changes no state");
        assert!(fold(&[created(), used]).is_err());
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
    fn a_session_is_recorded_only_while_running_and_survives_restarts() {
        let session = Event::Session { id: "session-7".into() };
        assert!(fold(&[created(), session.clone()]).is_err());
        let t = fold(&[created(), started(), session.clone()]).unwrap();
        assert_eq!((t.state, t.pid, t.session_id), (State::Running, Some(7), Some("session-7".into())));
        let t = fold(&[created(), started(), session.clone(), stopped(false), started()]).unwrap();
        assert_eq!((t.state, t.pid, t.session_id), (State::Running, Some(7), Some("session-7".into())));
        assert!(fold(&[created(), started(), stopped(true), session.clone()]).is_err());
        assert!(fold(&[created(), start_failed(), session.clone()]).is_err());
        assert!(fold(&[created(), started(), unconfirmed(), session.clone()]).is_err());
        assert!(fold(&[created(), Event::Destroyed, session]).is_err());
    }

    #[test]
    fn adoption_preserves_the_running_identity_and_failed_shutdown_retains_it() {
        let holder = Holder { pid: 8, socket: "/tmp/task.sock".into() };
        let start = Event::Started { pid: Some(7), holder: Some(holder.clone()) };
        let before = fold(&[created(), start.clone()]).unwrap();
        let after = fold(&[created(), start.clone(), Event::Adopted]).unwrap();
        assert_eq!(after, before);
        assert!(fold(&[created(), Event::Adopted]).is_err());
        assert!(fold(&[created(), started(), Event::Adopted]).is_err());
        assert!(fold(&[created(), start.clone(), stopped(false), Event::Adopted]).is_err());
        let failed = fold(&[created(), start, unconfirmed()]).unwrap();
        assert_eq!(failed.pid, Some(7));
        assert_eq!(failed.holder, Some(holder));
        assert!(!failed.startable() && !failed.resumable());
    }

    #[test]
    fn old_snapshots_without_a_session_pointer_still_load() {
        let value = serde_json::json!({
            "id": "t", "backend": "local", "spec": { "argv": ["agent"], "cwd": "/w" },
            "state": { "state": "running" }, "pid": 7
        });
        let task: Task = serde_json::from_value(value).unwrap();
        assert_eq!(task.session_id, None);
        assert_eq!(task.usage, Usage::default());
    }

    #[test]
    fn usage_is_accounting_not_a_transition() {
        let reported = Event::Usage { requests: 1, tokens: 9 };
        let t = fold(&[created(), started(), reported.clone()]).unwrap();
        assert_eq!((t.state, t.usage), (State::Running, Usage { requests: 1, tokens: 9 }));

        // Usage survives the stop that ends the run.
        let t = fold(&[created(), started(), reported, stopped(true)]).unwrap();
        assert_eq!(t.usage, Usage { requests: 1, tokens: 9 });

        // A later report with smaller totals is stale, not a rewind.
        let t = fold(&[
            created(),
            started(),
            Event::Usage { requests: 2, tokens: 12 },
            Event::Usage { requests: 1, tokens: 4 },
        ])
        .unwrap();
        assert_eq!(t.usage, Usage { requests: 2, tokens: 12 });

        assert!(
            fold(&[created(), started(), Event::Destroyed, Event::Usage { requests: 1, tokens: 1 }]).is_err(),
            "a destroyed task has no gateway to report usage"
        );
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
        for e in [
            created(),
            started(),
            Event::Session { id: "session-7".into() },
            Event::Usage { requests: 2, tokens: 12 },
            stopped(false),
            start_failed(),
            unconfirmed(),
            Event::Destroyed,
        ] {
            let line = serde_json::to_string(&e).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), e, "{line}");
        }
        assert_eq!(serde_json::to_string(&started()).unwrap(), r#"{"event":"started","pid":7}"#);
    }
}
