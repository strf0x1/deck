//! Private daemon control grammar. Bound each line before parsing so an
//! incomplete or oversized request cannot grow a control worker indefinitely.

use crate::approvals::{Ask, Decision};
use crate::{Spec, Task};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::PathBuf;

pub(crate) const LIMIT: usize = 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Request {
    Create {
        spec: Spec,
    },
    Start {
        id: String,
    },
    Resume {
        id: String,
    },
    Stop {
        id: String,
    },
    Destroy {
        id: String,
    },
    Task {
        id: String,
    },
    Tasks,
    /// The projects a window may show.
    Projects,
    /// Resolve a path into its project, adding the checkout.
    OpenProject {
        path: PathBuf,
        argv: Vec<String>,
    },
    /// Hide a project; its tasks stay.
    ForgetProject {
        id: String,
    },
    Run {
        spec: Spec,
    },
    Session {
        id: String,
        session_id: String,
    },
    /// The gateway asks for a scope; `deckd` may hold until a window decides.
    Approval {
        task: String,
        #[serde(flatten)]
        ask: Ask,
    },
    /// Pending items for a task.
    Approvals {
        task: String,
    },
    /// One item's state, for the gateway's boundary status endpoint.
    ApprovalStatus {
        task: String,
        id: String,
    },
    /// Long-poll one item for the gateway's boundary status endpoint.
    ApprovalWait {
        task: String,
        id: String,
        wait_ms: u64,
    },
    /// The agent no longer needs a pending item.
    WithdrawApproval {
        task: String,
        id: String,
    },
    /// The gateway's latest model totals for a task.
    ModelUsage {
        task: String,
        requests: u64,
        tokens: u64,
    },
    /// A successful GitHub publication reported by the task's gateway.
    Publication {
        task: String,
        publication: crate::lifecycle::Publication,
    },
    /// The turn number last shown to the user for a task (ADR 0010 §6).
    Seen {
        id: String,
        turn: u64,
    },
    /// Mark a task done (`done: true`) or clear the mark.
    Mark {
        id: String,
        done: bool,
    },
    /// Record the task's human title, from the first line of its first
    /// prompt (ADR 0010 §9).
    Title {
        id: String,
        title: String,
    },
    /// The task's publication and its live pull request state, read through
    /// its gateway when due (ADR 0008 §8). Reads only.
    PullState {
        task: String,
        #[serde(default)]
        refresh: bool,
    },
    /// Approve a whole web profile for a project: one decision instead of one
    /// per destination. Expands to project rules before the reply.
    WebProfile {
        task: String,
        #[serde(default)]
        project: Option<String>,
        profile: String,
        destinations: Vec<String>,
        #[serde(default)]
        revoke: bool,
    },
    /// Always-allow the GitHub operations for a project: one decision instead
    /// of one per publish (ADR 0007 §3).
    GitHubProfile {
        task: String,
        #[serde(default)]
        project: Option<String>,
        repository: String,
        operations: Vec<String>,
        #[serde(default)]
        revoke: bool,
    },
    /// Replace a task's live model budget; the gateway re-reads the file.
    SetBudget {
        task: String,
        requests: Option<u64>,
        tokens: Option<u64>,
    },
    DecideApproval {
        id: String,
        decision: Decision,
    },
    Grants {
        task: String,
    },
    RevokeGrant {
        task: String,
        id: String,
    },
    /// Live task grants and the task's saved project rules for the panel.
    AccessGrants {
        task: String,
    },
    /// The task's effective GitHub approval mode, re-read from its project
    /// file so the panel reflects a switch before the next write.
    GithubApprovals {
        task: String,
    },
    /// A confirmed removal. Project rules also remove matching task grants.
    RevokeAccessGrant {
        task: String,
        id: String,
    },
    /// The gateway asks for one credential value for one operation
    /// (ADR 0004 §4.4). Answered only for a name in the task's set, while the
    /// task runs; never logged.
    Credential {
        task: String,
        name: String,
    },
    /// A gateway's revocation feed for its task's set, held for the
    /// connection's lifetime (§4.5).
    CredentialWatch {
        task: String,
    },
    /// The names in a task's credential set with their fingerprints and
    /// revoked state, for the access panel. Never a value (§4.2).
    Credentials {
        task: String,
    },
    /// All stored names for the credential CLI, through the daemon's broker.
    ListCredentials,
    /// Clear a cancelled/refused OS read only when the host explicitly retries.
    RetryCredential {
        name: String,
    },
    /// Revoke a credential's current value, from the CLI or the access panel.
    RevokeCredential {
        name: String,
    },
    /// The stored settings, or the implicit provider, with their revision
    /// (ADR 0012 §1).
    Settings,
    /// Replace the settings if `revision` is still the stored one.
    SetSettings {
        settings: crate::settings::Settings,
        revision: u64,
    },
    /// Store a value from a window (ADR 0012 §3). Replies with the name's
    /// listing; the value is in this request only and is never logged.
    SetCredential {
        name: String,
        value: crate::credentials::Secret,
    },
    /// Remove a name from the store.
    DeleteCredential {
        name: String,
    },
    /// A capable window attaches to this task for the connection's lifetime.
    Watch {
        task: String,
    },
    Subscribe,
}

impl Request {
    pub fn validate(&self) -> Result<()> {
        let id = match self {
            Self::Start { id }
            | Self::Resume { id }
            | Self::Stop { id }
            | Self::Destroy { id }
            | Self::Task { id }
            | Self::Session { id, .. }
            | Self::Approval { task: id, .. }
            | Self::Approvals { task: id }
            | Self::ApprovalStatus { task: id, .. }
            | Self::ApprovalWait { task: id, .. }
            | Self::WithdrawApproval { task: id, .. }
            | Self::ModelUsage { task: id, .. }
            | Self::Publication { task: id, .. }
            | Self::Seen { id, .. }
            | Self::Mark { id, .. }
            | Self::Title { id, .. }
            | Self::PullState { task: id, .. }
            | Self::WebProfile { task: id, .. }
            | Self::GitHubProfile { task: id, .. }
            | Self::SetBudget { task: id, .. }
            | Self::Grants { task: id }
            | Self::RevokeGrant { task: id, .. }
            | Self::AccessGrants { task: id }
            | Self::GithubApprovals { task: id }
            | Self::RevokeAccessGrant { task: id, .. }
            | Self::Credential { task: id, .. }
            | Self::CredentialWatch { task: id }
            | Self::Credentials { task: id }
            | Self::Watch { task: id } => Some(id),
            _ => None,
        };
        if let Some(id) = id
            && !valid(id)
        {
            bail!("invalid task id");
        }
        let approval = match self {
            Self::DecideApproval { id, .. }
            | Self::RevokeGrant { id, .. }
            | Self::RevokeAccessGrant { id, .. }
            | Self::ApprovalStatus { id, .. }
            | Self::ApprovalWait { id, .. }
            | Self::WithdrawApproval { id, .. } => Some(id),
            _ => None,
        };
        if let Some(id) = approval
            && !valid(id)
        {
            bail!("invalid approval id");
        }
        if let Self::Credential { name, .. }
        | Self::RevokeCredential { name }
        | Self::RetryCredential { name }
        | Self::SetCredential { name, .. }
        | Self::DeleteCredential { name } = self
            && !crate::credentials::valid_name(name)
        {
            bail!("invalid credential name");
        }
        if let Self::ForgetProject { id } = self
            && id.is_empty()
        {
            bail!("invalid project id");
        }
        Ok(())
    }
}

fn valid(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub task: Task,
    /// False means the agent was already running. B1 must not initialize it.
    pub started: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Reply {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub(crate) fn read(input: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let part = input.fill_buf()?;
        if part.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            bail!("control connection ended halfway through a line");
        }
        let n = part.iter().position(|&b| b == b'\n').map_or(part.len(), |i| i + 1);
        if line.len() + n > LIMIT {
            bail!("control line exceeds {LIMIT} bytes");
        }
        line.extend_from_slice(&part[..n]);
        input.consume(n);
        if line.last() == Some(&b'\n') {
            return Ok(Some(line));
        }
    }
}

pub(crate) fn write(out: &mut impl Write, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
