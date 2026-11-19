//! Integration metadata is prepared before an ordinary worker can launch.
//! The first task event reserves the destination and fixes contributor order.
//! Later events can fill missing pins or record a preparation blocker.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) mod remote;

pub const MAX_INPUTS: usize = 16;
pub const MAX_REASON: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Destination {
    LocalBranch { branch: String },
    NewPullRequest {},
    ExistingPullRequest { number: u64 },
}

impl Destination {
    pub(crate) fn validate(&self) -> bool {
        match self {
            Self::LocalBranch { branch } => {
                !branch.starts_with("refs/heads/") && crate::github::branch_name(branch).as_deref() == Some(branch)
            }
            Self::ExistingPullRequest { number } => *number > 0,
            Self::NewPullRequest {} => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Target {
    // Preserve the on-disk shape of earlier VM plans.
    Github { repository: String, reference: Reference },
    Local { local: crate::worktree::Repository, branch: String },
}

impl Target {
    pub(crate) fn github(&self) -> Result<(&str, &Reference)> {
        match self {
            Self::Github { repository, reference } => Ok((repository, reference)),
            Self::Local { .. } => bail!("expected a GitHub integration destination"),
        }
    }

    pub(crate) fn opens_draft(&self) -> bool {
        matches!(self, Self::Github { reference: Reference::Base { .. }, .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Reference {
    Base { branch: String },
    Pull { number: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Input {
    pub(crate) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Phase {
    Preparing,
    Waiting { reason: String },
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub(crate) target: Target,
    pub(crate) brief: String,
    pub(crate) inputs: Vec<Input>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) base_commit: Option<String>,
    pub(crate) phase: Phase,
}

pub(crate) fn is_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Plan {
    pub(crate) fn update(&mut self, next: &Self) -> Result<()> {
        let extends = |old: &Option<String>, new: &Option<String>| {
            old.as_ref().is_none_or(|old| new.as_ref() == Some(old)) && new.as_ref().is_none_or(|new| is_commit(new))
        };
        if self.target != next.target
            || self.brief != next.brief
            || self.inputs.len() != next.inputs.len()
            || !extends(&self.base_commit, &next.base_commit)
            || !self.inputs.iter().zip(&next.inputs).all(|(a, b)| a.id == b.id && extends(&a.commit, &b.commit))
        {
            bail!("integration update changed its reservation or a recorded commit");
        }
        match &next.phase {
            Phase::Waiting { reason } if reason.is_empty() || reason.chars().count() > MAX_REASON => {
                bail!("invalid integration waiting reason")
            }
            Phase::Ready if next.base_commit.is_none() || next.inputs.iter().any(|input| input.commit.is_none()) => {
                bail!("an integration checkout needs every commit pinned")
            }
            _ => {}
        }
        *self = next.clone();
        Ok(())
    }

    pub(crate) fn waiting(&mut self, reason: &str) {
        self.phase = Phase::Waiting { reason: reason.chars().take(MAX_REASON).collect() };
    }

    pub(crate) fn reason(&self) -> Option<&str> {
        match &self.phase {
            Phase::Waiting { reason } => Some(reason),
            _ => None,
        }
    }

    pub(crate) fn summary(&self, id: &str) -> Value {
        let destination = match &self.target {
            Target::Github { reference: Reference::Base { .. }, .. } => Destination::NewPullRequest {},
            Target::Github { reference: Reference::Pull { number }, .. } => {
                Destination::ExistingPullRequest { number: *number }
            }
            Target::Local { branch, .. } => Destination::LocalBranch { branch: branch.clone() },
        };
        let mut summary = json!({
            "destination": destination,
            "branch": crate::github::task_branch(id), "contributors": self.inputs,
        });
        if let Target::Github { repository, reference } = &self.target {
            summary["repository"] = json!(repository);
            if let Reference::Base { branch } = reference {
                summary["base_branch"] = json!(branch);
            }
        }
        if let Some(commit) = &self.base_commit {
            summary["base_commit"] = json!(commit);
        }
        summary["phase"] = json!(match &self.phase {
            Phase::Preparing => "preparing",
            Phase::Ready => "ready",
            Phase::Waiting { reason } => {
                summary["reason"] = json!(reason);
                "waiting"
            }
        });
        summary
    }
}

pub(crate) fn reserves(state: &crate::State) -> bool {
    matches!(
        state,
        crate::State::Created
            | crate::State::Running
            | crate::State::Failed { failure: crate::lifecycle::Failure::ShutdownUnconfirmed(_) }
    )
}
