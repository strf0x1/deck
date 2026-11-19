//! A coordinator's task-control operations as `deckd` decides them (ADR 0011
//! §4–§5, contract 1.4.0 §13): the typed operation the gateway forwards, the
//! scope, provenance, budget and cap rules, and the reply or refusal. Pure:
//! the daemon reads the records and this module decides.
//!
//! The design choice: what the coordinator may not see does not exist for
//! it. A task outside its project set, or the coordinator itself, is
//! `deck_control_unknown` exactly like a missing id; a project outside the
//! set is `deck_control_scope`. Driving a task (`prompt`, `stop`) needs the
//! host-recorded `created_by` to name the caller, and never reaches a
//! coordinator. Caps are refusals with a code, never an approval card.

pub mod summary;

use crate::coordinator::{Budget, Policy};
use crate::lifecycle::{State, Task, TaskKind};
use serde::{Deserialize, Serialize};

/// The longest `wait` (contract 1.4.0 §13.3); a larger one is refused.
pub const WAIT_LIMIT_MS: u64 = 300_000;
/// The most summaries one `tasks` returns.
pub const TASKS_LIMIT: usize = 200;
/// How long a `prompt` waits for a just-created task's session.
pub const SESSION_WAIT_MS: u64 = 60_000;
/// The rolling window of the creation rate.
pub const HOUR_MS: u64 = 60 * 60 * 1000;

/// The worker budget a `create` may ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requested {
    #[serde(default)]
    pub requests: Option<u64>,
    #[serde(default)]
    pub tokens: Option<u64>,
}

/// One operation, as the gateway forwards it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Tasks {
        #[serde(default)]
        project: Option<String>,
    },
    Task {
        id: String,
    },
    Create {
        /// Optional since contract 1.5.0: the coordinator's only project.
        #[serde(default)]
        project: Option<String>,
        brief: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        budget: Option<Requested>,
    },
    Integrate {
        #[serde(default)]
        project: Option<String>,
        destination: crate::integration::Destination,
        contributors: Vec<String>,
        brief: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        budget: Option<Requested>,
    },
    Prompt {
        id: String,
        text: String,
    },
    Wait {
        id: String,
        #[serde(default)]
        after_turn: Option<u64>,
        wait_ms: u64,
    },
    Stop {
        id: String,
    },
}

impl Operation {
    pub fn name(&self) -> &'static str {
        match self {
            Operation::Tasks { .. } => "tasks",
            Operation::Task { .. } => "task",
            Operation::Create { .. } => "create",
            Operation::Integrate { .. } => "integrate",
            Operation::Prompt { .. } => "prompt",
            Operation::Wait { .. } => "wait",
            Operation::Stop { .. } => "stop",
        }
    }

    /// The task it names, for the audit row.
    pub fn target(&self) -> Option<&str> {
        match self {
            Operation::Task { id }
            | Operation::Prompt { id, .. }
            | Operation::Wait { id, .. }
            | Operation::Stop { id } => Some(id),
            Operation::Tasks { project } | Operation::Create { project, .. } | Operation::Integrate { project, .. } => {
                project.as_deref()
            }
        }
    }

    /// The bounds `deckd` checks again: anything with the socket can send
    /// this, not only the gateway.
    pub fn validate(&self) -> Result<(), Refusal> {
        let bad = |message: &str| Err(Refusal::new(Code::BadRequest, message));
        if let Operation::Integrate { destination, contributors, .. } = self {
            let ids: std::collections::HashSet<_> = contributors.iter().collect();
            if !destination.validate() {
                return bad("invalid integration destination");
            }
            if !(1..=crate::integration::MAX_INPUTS).contains(&contributors.len())
                || ids.len() != contributors.len()
                || ids.iter().any(|id| !(1..=128).contains(&id.chars().count()))
            {
                return bad("integration requires 1 to 16 distinct contributor task IDs");
            }
        }
        match self {
            Operation::Wait { wait_ms, .. } if *wait_ms > WAIT_LIMIT_MS => bad("wait_ms is at most 300000"),
            Operation::Prompt { text, .. } if text.trim().is_empty() => bad("a prompt cannot be empty"),
            Operation::Create { brief, .. } | Operation::Integrate { brief, .. } if brief.trim().is_empty() => {
                bad("a brief cannot be empty")
            }
            Operation::Create { title: Some(title), .. } | Operation::Integrate { title: Some(title), .. }
                if title.trim().is_empty() || title.chars().count() > 80 =>
            {
                bad("`title` is 1 to 80 characters")
            }
            Operation::Create { budget: Some(budget), .. } | Operation::Integrate { budget: Some(budget), .. }
                if budget.requests == Some(0) || budget.tokens == Some(0) || *budget == Requested::none() =>
            {
                bad("a budget names requests or tokens, each at least 1")
            }
            _ => Ok(()),
        }
    }
}

impl Requested {
    fn none() -> Self {
        Requested { requests: None, tokens: None }
    }
}

/// A refusal code (contract 1.4.0 §13.5). The gateway owns the statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Code {
    #[serde(rename = "deck_control_bad_request")]
    BadRequest,
    #[serde(rename = "deck_control_scope")]
    Scope,
    #[serde(rename = "deck_control_cap")]
    Cap,
    #[serde(rename = "deck_control_refused")]
    Refused,
    #[serde(rename = "deck_control_unknown")]
    Unknown,
    #[serde(rename = "deck_control_busy")]
    Busy,
    #[serde(rename = "deck_control_unavailable")]
    Unavailable,
}

impl Code {
    /// The audit row's outcome.
    pub fn name(self) -> &'static str {
        match self {
            Code::BadRequest => "deck_control_bad_request",
            Code::Scope => "deck_control_scope",
            Code::Cap => "deck_control_cap",
            Code::Refused => "deck_control_refused",
            Code::Unknown => "deck_control_unknown",
            Code::Busy => "deck_control_busy",
            Code::Unavailable => "deck_control_unavailable",
        }
    }
}

/// Which cap a `create` met.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    Concurrent,
    Rate,
}

impl Cap {
    /// The policy field the user raises.
    pub fn field(self) -> &'static str {
        match self {
            Cap::Concurrent => "max_concurrent",
            Cap::Rate => "creates_per_hour",
        }
    }
}

/// A cap refusal as the notice stream announces it, so the window can offer
/// to raise it (deck#152 R4, deck#153).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapNotice {
    pub coordinator: String,
    /// `max_concurrent` or `creates_per_hour`.
    pub cap: String,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub code: Code,
    pub message: String,
    /// The cap met, for the notice; never on the wire.
    #[serde(skip)]
    pub cap: Option<(Cap, u32)>,
}

impl Refusal {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), cap: None }
    }
}

/// What `deckd` answers the gateway.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Reply(serde_json::Value),
    Refused(Refusal),
}

/// `no such task`, for a missing task and one the coordinator may not see.
pub fn unknown() -> Refusal {
    Refusal::new(Code::Unknown, "no such task")
}

/// Whether `caller` (with `policy`) may see `task`: in its project set, and
/// never itself.
pub fn visible(policy: &Policy, caller: &str, task: &Task) -> bool {
    task.id != caller && task.spec.project.as_ref().is_some_and(|project| policy.projects.contains(project))
}

/// Whether `caller` may prompt or stop a task it can see: no coordinator, and
/// only a task it created.
pub fn may_drive(caller: &str, task: &Task) -> Result<(), Refusal> {
    if task.spec.kind == TaskKind::Coordinator {
        return Err(Refusal::new(Code::Refused, format!("task {} is a coordinator", task.id)));
    }
    if task.spec.created_by.as_deref() != Some(caller) {
        return Err(Refusal::new(Code::Scope, format!("task {} was not created by this coordinator", task.id)));
    }
    Ok(())
}

/// Whether a target's lifecycle lets it be prompted at all.
pub fn promptable(task: &Task) -> Result<(), Refusal> {
    match task.state {
        State::Created | State::Running => Ok(()),
        _ => Err(Refusal::new(
            Code::Refused,
            format!("task {} is {}; this surface does not resume tasks", task.id, summary::state_name(&task.state)),
        )),
    }
}

/// A project the caller named, in its scope. The refusal names the set in
/// full, so the model can correct itself (contract 1.5.0 §13.3).
pub fn project_in_scope(policy: &Policy, project: &str) -> Result<(), Refusal> {
    if policy.projects.iter().any(|p| p == project) {
        return Ok(());
    }
    Err(Refusal::new(
        Code::Scope,
        format!("project {} is not in this coordinator's project set; {}", short(project), named(policy)),
    ))
}

/// The project a `create` records its worker in: the one named, or the
/// coordinator's only project when none is (contract 1.5.0 §13.3).
pub fn create_project(policy: &Policy, project: Option<&str>) -> Result<String, Refusal> {
    match (project, policy.projects.as_slice()) {
        (Some(project), _) => project_in_scope(policy, project).map(|()| project.to_owned()),
        (None, [only]) => Ok(only.clone()),
        (None, _) => Err(Refusal::new(Code::BadRequest, format!("name a project; {}", named(policy)))),
    }
}

fn named(policy: &Policy) -> String {
    match policy.projects.as_slice() {
        [] => "this coordinator has no projects".into(),
        projects => format!("its projects are {}", projects.join(", ")),
    }
}

/// Everything a `create` checks after the scope, in order: the project's
/// agent command, the budget ceiling, then the caps. `workers` is each task
/// the caller created, with its creation time. `in_place` is `Some` when the
/// backend runs a worker in the project's checkout itself (local), with the
/// live workers already there; a backend that copies the project (VM) passes
/// `None`. Returns the worker's budget.
pub fn admit_create(
    policy: &Policy,
    argv: Option<&[String]>,
    requested: Option<Requested>,
    workers: &[(State, u64)],
    in_place: Option<usize>,
    now_ms: u64,
) -> Result<Budget, Refusal> {
    if argv.is_none_or(|argv| argv.first().is_none_or(|program| program.trim().is_empty())) {
        return Err(Refusal::new(Code::Refused, "the project has no recorded agent command; set one in Deck"));
    }
    let ceiling = policy.worker_budget;
    let budget = match requested {
        None => ceiling,
        Some(requested) => {
            let above = |asked: Option<u64>, limit: Option<u64>| matches!((asked, limit), (Some(a), Some(l)) if a > l);
            if above(requested.requests, ceiling.requests) || above(requested.tokens, ceiling.tokens) {
                return Err(Refusal::new(
                    Code::Refused,
                    "the requested budget is above this coordinator's worker budget",
                ));
            }
            Budget { requests: requested.requests.or(ceiling.requests), tokens: requested.tokens.or(ceiling.tokens) }
        }
    };
    let live = workers.iter().filter(|(state, _)| matches!(state, State::Created | State::Running)).count();
    if live >= policy.max_concurrent as usize {
        let cap = policy.max_concurrent;
        let mut refusal =
            Refusal::new(Code::Cap, format!("this coordinator already has {live} live workers (cap {cap})"));
        refusal.cap = Some((Cap::Concurrent, cap));
        return Err(refusal);
    }
    let recent = workers.iter().filter(|(_, created)| now_ms.saturating_sub(*created) < HOUR_MS).count();
    if recent >= policy.creates_per_hour as usize {
        let cap = policy.creates_per_hour;
        let mut refusal =
            Refusal::new(Code::Cap, format!("this coordinator created {recent} workers in the last hour (cap {cap})"));
        refusal.cap = Some((Cap::Rate, cap));
        return Err(refusal);
    }
    // Workers that run in the project's own checkout would edit, test and
    // commit over each other (deck#200). No Limits field raises this, so the
    // refusal carries no cap for the window's notice.
    if in_place.is_some_and(|live| live > 0) {
        return Err(Refusal::new(
            Code::Cap,
            "a worker is already running in this project's checkout; local workers share it, \
             so one runs at a time (a VM backend gives each its own copy)",
        ));
    }
    Ok(budget)
}

/// A project id shortened for a message, as the contract's example does.
fn short(project: &str) -> String {
    match project.split_once(':') {
        Some((scheme, rest)) if rest.chars().count() > 12 => {
            let head: String = rest.chars().take(4).collect();
            let tail: String = rest.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
            format!("{scheme}:{head}…{tail}")
        }
        _ => project.to_owned(),
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
