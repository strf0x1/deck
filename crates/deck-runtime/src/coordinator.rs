//! A coordinator's policy (ADR 0011 §4): the project set it may act in, its
//! caps and its budgets, kept in `tasks/<id>/coordinator.json`.
//!
//! The design choice: the file is the host's. `deckd` writes it at create and
//! when the user edits it through a window (`coordinator_policy`), and only
//! reads it on behalf of the coordinator, so a coordinator never raises its
//! own limits. A VM guest cannot reach the file at all; a local coordinator
//! runs as the user, so there the limits are as strong as the agent's own
//! sandbox, which ADR 0011 §2 states.

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The environment variable that selects Construct's context strategy.
pub const STRATEGY_ENV: &str = "CONSTRUCT_CONTEXT_STRATEGY";
/// A coordinator's strategy: every message kept at some resolution
/// (construct ADR 0043).
pub const STRATEGY: &str = "optchat";
/// How many workers may run at once, unless the user raises it.
pub const MAX_CONCURRENT: u32 = 4;
/// How many workers may be created in a rolling hour, unless the user raises it.
pub const CREATES_PER_HOUR: u32 = 20;
/// The highest either cap may be set to: a cap is a guard, not a quota.
pub const CAP_LIMIT: u32 = 1000;

const FILE: &str = "coordinator.json";

/// The coordinator command when a project records none.
pub fn default_argv() -> Vec<String> {
    vec!["construct".into(), "acp".into()]
}

/// A model budget: requests and tokens, `None` unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub requests: Option<u64>,
    pub tokens: Option<u64>,
}

impl Budget {
    fn validate(&self, what: &str) -> Result<()> {
        if self.requests == Some(0) || self.tokens == Some(0) {
            bail!("{what} must be at least 1 or unlimited");
        }
        Ok(())
    }
}

/// What `coordinator.json` holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// The scope: project ids, sorted and without repeats. Fixed at create.
    pub projects: Vec<String>,
    pub max_concurrent: u32,
    pub creates_per_hour: u32,
    /// The model budget each worker gets, and the most a `create` may ask for.
    #[serde(default)]
    pub worker_budget: Budget,
    /// The coordinator's own model budget; `deckd` writes it to the task's
    /// `budget.json`, which its gateway re-reads.
    #[serde(default)]
    pub model_budget: Budget,
}

impl Policy {
    /// The default policy for a scope.
    pub fn new(projects: Vec<String>) -> Result<Policy> {
        let mut policy = Policy {
            projects,
            max_concurrent: MAX_CONCURRENT,
            creates_per_hour: CREATES_PER_HOUR,
            worker_budget: Budget::default(),
            model_budget: Budget::default(),
        };
        policy.validate()?;
        Ok(policy)
    }

    /// Normalize the scope and check every bound; an invalid policy is never
    /// written.
    pub fn validate(&mut self) -> Result<()> {
        self.projects = scope(&self.projects)?;
        for (name, cap) in [("max_concurrent", self.max_concurrent), ("creates_per_hour", self.creates_per_hour)] {
            if !(1..=CAP_LIMIT).contains(&cap) {
                bail!("{name} must be between 1 and {CAP_LIMIT}, not {cap}");
            }
        }
        self.worker_budget.validate("worker_budget")?;
        self.model_budget.validate("model_budget")
    }
}

/// A scope as the policy keeps it: sorted, without repeats, never empty.
pub fn scope(projects: &[String]) -> Result<Vec<String>> {
    let mut projects: Vec<String> = projects.iter().map(|p| p.trim().to_owned()).collect();
    if projects.iter().any(String::is_empty) {
        bail!("a coordinator's project id cannot be empty");
    }
    projects.sort();
    projects.dedup();
    if projects.is_empty() {
        bail!("a coordinator needs at least one project");
    }
    Ok(projects)
}

/// A coordinator command as a project records it: `None` is the default,
/// and a recorded one names a program.
pub fn validate_argv(argv: &Option<Vec<String>>) -> Result<()> {
    if let Some(argv) = argv
        && argv.first().is_none_or(|program| program.trim().is_empty())
    {
        bail!("a coordinator command needs a program");
    }
    Ok(())
}

fn path(data: &Path, id: &str) -> PathBuf {
    data.join("tasks").join(id).join(FILE)
}

/// The coordinator's policy, or `None` when the task has none.
pub fn read(data: &Path, id: &str) -> Result<Option<Policy>> {
    let path = path(data, id);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Validate `policy` and replace the file atomically.
pub fn write(data: &Path, id: &str, mut policy: Policy) -> Result<Policy> {
    policy.validate()?;
    crate::approvals::store::write_json(&path(data, id), &policy)?;
    Ok(policy)
}

/// What `coordinator_policy` reads and replaces: the policy and the
/// project's coordinator command (`None` is [`default_argv`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub policy: Policy,
    #[serde(default)]
    pub coordinator_argv: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deck-coordinator-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_default_policy_has_the_adr_caps_and_a_sorted_scope() {
        let policy = Policy::new(vec!["git:b".into(), "git:a".into(), "git:b".into()]).unwrap();
        assert_eq!(policy.projects, vec!["git:a".to_string(), "git:b".to_string()]);
        assert_eq!((policy.max_concurrent, policy.creates_per_hour), (4, 20));
        assert_eq!(policy.worker_budget, Budget::default());
    }

    #[test]
    fn an_empty_scope_zero_caps_and_zero_budgets_are_refused() {
        assert!(Policy::new(vec![]).is_err());
        assert!(Policy::new(vec![" ".into()]).is_err());
        let good = Policy::new(vec!["git:a".into()]).unwrap();
        for bad in [
            Policy { max_concurrent: 0, ..good.clone() },
            Policy { creates_per_hour: CAP_LIMIT + 1, ..good.clone() },
            Policy { worker_budget: Budget { requests: Some(0), tokens: None }, ..good.clone() },
            Policy { model_budget: Budget { requests: None, tokens: Some(0) }, ..good.clone() },
        ] {
            let mut bad = bad;
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_policy_round_trips_and_an_invalid_write_leaves_the_file() {
        let data = scratch("round-trip");
        assert_eq!(read(&data, "t1").unwrap(), None);
        let policy = write(&data, "t1", Policy::new(vec!["git:a".into()]).unwrap()).unwrap();
        assert_eq!(read(&data, "t1").unwrap(), Some(policy.clone()));
        assert!(write(&data, "t1", Policy { max_concurrent: 0, ..policy.clone() }).is_err());
        assert_eq!(read(&data, "t1").unwrap(), Some(policy));
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn a_recorded_command_names_a_program() {
        assert!(validate_argv(&None).is_ok());
        assert!(validate_argv(&Some(vec!["construct".into(), "acp".into()])).is_ok());
        assert!(validate_argv(&Some(vec![])).is_err());
        assert!(validate_argv(&Some(vec![" ".into()])).is_err());
    }
}
