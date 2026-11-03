//! Private daemon control grammar. Bound each line before parsing so an
//! incomplete or oversized request cannot grow a control worker indefinitely.

use crate::approvals::{Ask, Decision};
use crate::{Spec, Task};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

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
            | Self::Grants { task: id }
            | Self::RevokeGrant { task: id, .. }
            | Self::Watch { task: id } => Some(id),
            _ => None,
        };
        if let Some(id) = id
            && !valid(id)
        {
            bail!("invalid task id");
        }
        let approval = match self {
            Self::DecideApproval { id, .. } | Self::RevokeGrant { id, .. } => Some(id),
            _ => None,
        };
        if let Some(id) = approval
            && !valid(id)
        {
            bail!("invalid approval id");
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
