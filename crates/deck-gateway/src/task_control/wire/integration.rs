//! Integration selectors on the coordinator wire. The gateway validates
//! the request shape before deckd checks scope and resolves Git objects.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const MAX_CONTRIBUTORS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Destination {
    LocalBranch { branch: String },
    NewPullRequest {},
    ExistingPullRequest { number: u64 },
}

pub(super) fn check(destination: &Destination, contributors: &[String]) -> Result<(), String> {
    match destination {
        Destination::LocalBranch { branch }
            if branch.starts_with("refs/heads/")
                || crate::github::policy::branch_name(branch).as_deref() != Some(branch) =>
        {
            return Err("A local destination requires a branch name without refs/heads/.".into());
        }
        Destination::ExistingPullRequest { number: 0 } => {
            return Err("An existing pull request requires a positive number.".into());
        }
        _ => {}
    }
    if !(1..=MAX_CONTRIBUTORS).contains(&contributors.len()) {
        return Err(format!("Integration requires 1 to {MAX_CONTRIBUTORS} contributor task IDs."));
    }
    let mut seen = HashSet::with_capacity(contributors.len());
    for id in contributors {
        if !(1..=128).contains(&id.chars().count()) {
            return Err("A contributor task ID is 1 to 128 characters.".into());
        }
        if !seen.insert(id) {
            return Err("Contributors must be distinct task IDs.".into());
        }
    }
    Ok(())
}
