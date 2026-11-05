//! Identity of a reviewed GitHub write. Only validated, effective request
//! fields enter the hash, so another bundle for the same commit can retry,
//! while changed commits or pull request edits require another Once decision.

use super::bundle::Bundle;
use super::client::{Settings, check_text};
use super::policy::{Ask, Operation};
use super::problem::Problem;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

pub(super) fn review(
    ask: &Ask,
    operation: Operation,
    bundle: Option<&[u8]>,
    settings: &Settings,
) -> Result<Option<String>, Problem> {
    let write = match operation {
        Operation::Publish => {
            writable(&ask.branch, settings)?;
            let bytes = bundle.ok_or_else(|| Problem::invalid("publish needs a bundle"))?;
            let bundle = Bundle::parse(bytes).map_err(Problem::invalid)?;
            bundle.branch(&ask.branch).map_err(Problem::invalid)?;
            let [(_, commit)] = bundle.refs.as_slice() else {
                return Err(Problem::invalid("publish needs exactly one branch"));
            };
            json!({ "branch": ask.branch, "commit": commit.to_ascii_lowercase() })
        }
        Operation::PullRequest => {
            writable(&ask.head, settings)?;
            base(&ask.base, settings)?;
            check_text(&ask.title, &ask.body)?;
            json!({
                "head": ask.head, "base": ask.base, "title": ask.title,
                "body": ask.body, "draft": ask.draft.unwrap_or(true),
            })
        }
        Operation::UpdatePullRequest => {
            let number = ask
                .number
                .filter(|number| *number > 0)
                .ok_or_else(|| Problem::invalid("update_pr needs the pull request number"))?;
            let mut edits = serde_json::Map::new();
            if !ask.base.is_empty() {
                base(&ask.base, settings)?;
                edits.insert("base".into(), json!(ask.base));
            }
            if !ask.title.is_empty() {
                edits.insert("title".into(), json!(ask.title));
            }
            if ask.empty_body || !ask.body.is_empty() {
                edits.insert("body".into(), json!(ask.body));
            }
            if let Some(draft) = ask.draft {
                edits.insert("draft".into(), json!(draft));
            }
            if edits.is_empty() {
                return Err(Problem::invalid("an update names a title, a body, a base or the draft state"));
            }
            check_text(if ask.title.is_empty() { "-" } else { &ask.title }, &ask.body)?;
            json!({ "number": number, "edits": edits })
        }
        Operation::Refs | Operation::Review => return Ok(None),
    };
    let identity = json!({
        "repository": settings.policy.repository(), "operation": operation.name(), "write": write,
    });
    fingerprint(&identity).map(Some)
}

fn writable(branch: &str, settings: &Settings) -> Result<(), Problem> {
    if !settings.policy.writable(branch, &settings.task) {
        return Err(Problem::refused(format!("{branch:?} is not a branch this task may write")));
    }
    Ok(())
}

fn base(branch: &str, settings: &Settings) -> Result<(), Problem> {
    if !settings.policy.accepts_base(branch) {
        return Err(Problem::refused(format!("{branch:?} is not a base this task may target")));
    }
    Ok(())
}

fn fingerprint(value: &Value) -> Result<String, Problem> {
    let bytes = serde_json::to_vec(value).map_err(|_| Problem::invalid("the reviewed write could not be encoded"))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
