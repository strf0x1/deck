//! The window's connection to a task owned by `deckd`. Dropping this handle
//! disconnects the window; only an explicit restart stops the agent.

use anyhow::Result;
use deck_acp::{Client, Inbound};
use deck_runtime::approvals::{Approval, Decision, Grant};
use deck_runtime::broker::Listed;
use deck_runtime::{DaemonClient, Spec, State, Watch};
use futures::channel::mpsc;
use std::path::{Path, PathBuf};

pub type Connection = (Client, mpsc::UnboundedReceiver<Inbound>);

#[derive(Clone)]
pub struct WindowTask {
    daemon: DaemonClient,
    id: String,
    agent_cwd: PathBuf,
    boundary_enforced: bool,
}

impl WindowTask {
    /// Find or start a task, rebuilding an existing live session when supported.
    pub fn run(data: &Path, argv: Vec<String>, cwd: PathBuf) -> Result<(WindowTask, Option<Connection>)> {
        let daemon = DaemonClient::connect_or_start(data)?;
        let run = daemon.run(Spec { argv, cwd })?;
        // ACP addresses the agent filesystem; task and project identity stay
        // in the host filesystem recorded by the daemon.
        let agent_cwd = if run.task.backend == "vm" { "/workspace".into() } else { run.task.spec.cwd.clone() };
        let task = WindowTask { daemon, boundary_enforced: run.task.backend == "vm", id: run.task.id, agent_cwd };
        let connection = if run.started {
            Some(task.connect(run.task.session_id)?)
        } else if let Some(id) = run.task.session_id {
            let stream = task.daemon.attach(&task.id)?;
            Some(Client::attach(task.agent_cwd.clone(), stream.reader, stream.writer, id)?)
        } else {
            None
        };
        Ok((task, connection))
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn boundary_enforced(&self) -> bool {
        self.boundary_enforced
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

    pub fn grants(&self) -> Result<Vec<Grant>> {
        self.daemon.access_grants(&self.id)
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

    fn connect(&self, session_id: Option<String>) -> Result<Connection> {
        let stream = self.daemon.attach(&self.id)?;
        match session_id {
            Some(id) => Client::load(self.agent_cwd.clone(), stream.reader, stream.writer, id),
            None => Client::over(self.agent_cwd.clone(), stream.reader, stream.writer),
        }
    }
}
