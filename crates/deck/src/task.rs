//! The window's connection to a task owned by `deckd`. Dropping this handle
//! disconnects the window; only an explicit restart stops the agent.

use anyhow::{Result, bail};
use deck_acp::{Client, Inbound};
use deck_runtime::approvals::{Approval, Decision, Grant};
use deck_runtime::broker::Listed;
use deck_runtime::{DaemonClient, Spec, State, Task, Watch};
use futures::channel::mpsc;
use std::path::{Path, PathBuf};

pub type Connection = (Client, mpsc::UnboundedReceiver<Inbound>);

#[derive(Clone)]
pub struct WindowTask {
    daemon: DaemonClient,
    id: String,
    agent_cwd: PathBuf,
    boundary_enforced: bool,
    /// A VM task whose project names a repository: it has a GitHub gateway.
    publishes: bool,
}

impl WindowTask {
    /// Find or start a task, rebuilding an existing live session when supported.
    pub fn run(data: &Path, argv: Vec<String>, cwd: PathBuf) -> Result<(WindowTask, Option<Connection>)> {
        let daemon = DaemonClient::connect_or_start(data)?;
        let run = daemon.run(Spec { argv, cwd, project: None, github: None })?;
        let task = WindowTask::of(daemon, &run.task);
        let connection = task.connection(run.started, run.task.session_id)?;
        Ok((task, connection))
    }

    /// The window's handle on task `record`, over another task's daemon
    /// connection. Nothing is asked of the daemon until [`WindowTask::open`].
    pub fn sibling(&self, record: &Task) -> WindowTask {
        WindowTask::of(self.daemon.clone(), record)
    }

    /// Create another task with this one's command and directory, without
    /// starting it; [`WindowTask::open`] on its sibling starts it. It asks for
    /// `create`, not `run`: `run` would find this task again. The GitHub
    /// policy is resolved afresh, as at launch.
    pub fn create_sibling(&self) -> Result<Task> {
        let spec = self.daemon.task(&self.id)?.spec;
        self.daemon.create(Spec { github: None, ..spec })
    }

    fn of(daemon: DaemonClient, record: &Task) -> WindowTask {
        // ACP addresses the agent filesystem; task and project identity stay
        // in the host filesystem recorded by the daemon.
        let vm = record.backend == "vm";
        let agent_cwd = if vm { "/workspace".into() } else { record.spec.cwd.clone() };
        let publishes = vm && record.spec.github.is_some();
        WindowTask { daemon, boundary_enforced: vm, publishes, id: record.id.clone(), agent_cwd }
    }

    /// Connect to this task by the rules launch uses: a running agent is
    /// live-attached when it has a recorded session (and stays detached
    /// otherwise); a stopped or created one is resumed or started first.
    pub fn open(&self) -> Result<Option<Connection>> {
        let record = self.daemon.task(&self.id)?;
        if record.state == State::Running {
            return self.connection(false, record.session_id);
        }
        let record = if record.resumable() {
            self.daemon.resume(&self.id)?
        } else if record.startable() {
            self.daemon.start(&self.id)?
        } else {
            bail!("task {} cannot run again: {:?}", self.id, record.state);
        };
        self.connection(true, record.session_id)
    }

    fn connection(&self, started: bool, session_id: Option<String>) -> Result<Option<Connection>> {
        if started {
            return self.connect(session_id).map(Some);
        }
        let Some(id) = session_id else { return Ok(None) };
        let stream = self.daemon.attach(&self.id)?;
        Ok(Some(Client::attach(self.agent_cwd.clone(), stream.reader, stream.writer, id)?))
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn boundary_enforced(&self) -> bool {
        self.boundary_enforced
    }

    /// Whether this task has a GitHub gateway to publish through.
    pub fn publishes(&self) -> bool {
        self.publishes
    }

    /// Every task the daemon knows, for the sessions column.
    pub fn tasks(&self) -> Result<Vec<Task>> {
        self.daemon.tasks()
    }

    /// A task's publication and live pull request state. The daemon bounds
    /// how often this reaches GitHub.
    pub fn pull_request(&self, id: &str, refresh: bool) -> Result<deck_runtime::pull_request::State> {
        self.daemon.pull_request(id, refresh)
    }

    /// Destroy task `id`: offered only once its pull request is merged.
    pub fn destroy(&self, id: &str) -> Result<()> {
        self.daemon.destroy(id)
    }

    /// Persist a pointer to the conversation in the agent's own journal.
    pub fn record_session(&self, session_id: &str) -> Result<()> {
        self.daemon.session(&self.id, session_id)
    }

    /// Attach this window's boundary card to its task. The lease ends when
    /// the receiver (and so the daemon connection) is dropped.
    pub fn watch(&self) -> Result<(Watch, mpsc::UnboundedReceiver<Approval>)> {
        self.daemon.watch(&self.id)
    }

    pub fn decide(&self, id: &str, decision: Decision) -> Result<Approval> {
        self.daemon.decide(id, decision)
    }

    /// This task's credential names with fingerprints and revoked state.
    /// The daemon never sends a value.
    pub fn credentials(&self) -> Result<Vec<Listed>> {
        self.daemon.credentials(&self.id)
    }

    pub fn revoke_credential(&self, name: &str) -> Result<()> {
        self.daemon.revoke_credential(name).map(|_| ())
    }

    /// Permit another OS read after a cancelled or refused one. False when
    /// there was no completed denial to clear.
    pub fn retry_credential(&self, name: &str) -> Result<bool> {
        self.daemon.retry_credential(name)
    }

    pub fn grants(&self) -> Result<Vec<Grant>> {
        self.daemon.access_grants(&self.id)
    }

    /// This task's effective GitHub approval mode, when its project names a
    /// repository. The daemon re-reads the project file, so a switch shows up
    /// before the next write.
    pub fn github_approvals(&self) -> Result<Option<deck_runtime::github::Approvals>> {
        self.daemon.github_approvals(&self.id)
    }

    pub fn revoke_grant(&self, id: &str) -> Result<()> {
        self.daemon.revoke_access_grant(&self.id, id)
    }

    /// Stop and start the agent, then rebuild its conversation by loading
    /// the recorded session. It never asks an existing process to reattach.
    pub fn restart(&self) -> Result<Connection> {
        if self.daemon.task(&self.id)?.state == State::Running {
            self.daemon.stop(&self.id)?;
        }
        let task = self.daemon.resume(&self.id)?;
        self.connect(task.session_id)
    }

    /// Stop the agent if it is running; a no-op otherwise.
    pub fn stop(&self) -> Result<()> {
        if self.daemon.task(&self.id)?.state == State::Running {
            self.daemon.stop(&self.id)?;
        }
        Ok(())
    }

    fn connect(&self, session_id: Option<String>) -> Result<Connection> {
        let stream = self.daemon.attach(&self.id)?;
        match session_id {
            Some(id) => Client::load(self.agent_cwd.clone(), stream.reader, stream.writer, id),
            None => Client::over(self.agent_cwd.clone(), stream.reader, stream.writer),
        }
    }
}
