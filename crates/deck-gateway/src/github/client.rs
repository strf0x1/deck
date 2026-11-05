//! The GitHub REST calls the gateway makes, and none it does not (ADR 0007
//! §1).
//!
//! The design choice: every URL is built here from three host-side pieces
//! (the API base, `repos/owner/name`, and a path this module wrote) and every
//! body is a bounded object this module encoded. The guest never contributes a
//! URL, a method or a header, so there is no request it can shape into an
//! endpoint the operation set does not name. A redirect is not followed: an
//! API that answers 301 to another host would be egress the policy never saw.

use crate::github::bundle::{Bundle, is_sha};
use crate::github::policy::{API_HOST, API_PREFIX, Policy};
use crate::github::problem::{Kind, Problem};
use crate::github::publish::Publisher;
use serde_json::{Value, json};
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A pull request body is a description, not an attachment.
pub const MAX_TEXT: usize = 64 * 1024;
/// The largest API response the gateway will read. `list_commits` is the
/// largest thing it asks for, and a page of it fits easily.
const MAX_RESPONSE: usize = 4 * 1024 * 1024;
/// The `User-Agent` GitHub requires on every request.
const AGENT: &str = "deck-gateway";
/// Shorter than any GitHub token, so never worth redacting.
const MIN_TOKEN: usize = 8;
/// The prefix a `matching-refs/heads/` reply puts in front of each short name.
const HEADS: &str = "refs/heads/";

#[derive(Clone)]
pub struct Settings {
    /// `https://api.github.com`, or a test's own listener.
    pub api: String,
    pub policy: Policy,
    pub task: String,
    /// The pinned clone URL (`https://github.com/owner/name`), or a test's.
    pub repository_url: String,
    /// The host token for one operation. `None` on the gateway's shared
    /// client; [`Client::authorized`] makes the per-operation one. Never
    /// written, never returned.
    pub token: Option<String>,
    pub scratch: PathBuf,
    pub git: Option<PathBuf>,
}

pub struct Client {
    settings: Settings,
    agent: ureq::Agent,
    /// Every publish shares one scratch repository and its refs, and the
    /// gateway serves each connection on its own thread, so publishes take
    /// turns: two at once could push each other's commit. Shared with every
    /// per-operation client.
    publishing: Arc<Mutex<()>>,
    /// Set when the operation's token is revoked: a running `git` is killed
    /// with its process group, and the operation's result is discarded.
    cancel: Option<Arc<AtomicBool>>,
}

impl Client {
    pub fn new(settings: Settings) -> Self {
        let config = ureq::Agent::config_builder()
            // The gateway decides what a non-2xx means; it is not an ureq error.
            .http_status_as_error(false)
            // A redirect is a destination the policy never admitted.
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(60)))
            .timeout_connect(Some(Duration::from_secs(20)))
            .build();
        Self { settings, agent: config.new_agent(), publishing: Arc::new(Mutex::new(())), cancel: None }
    }

    /// This client for one operation: `token` is the value `deckd` handed out
    /// for it, and `cancel` is what a revocation sets.
    pub fn authorized(&self, token: &str, cancel: Arc<AtomicBool>) -> Self {
        Self {
            settings: Settings { token: Some(token.to_string()), ..self.settings.clone() },
            agent: self.agent.clone(),
            publishing: self.publishing.clone(),
            cancel: Some(cancel),
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn has_token(&self) -> bool {
        self.settings.token.as_ref().is_some_and(|token| !token.is_empty())
    }

    /// The destination an approval or a grant names: the repository, then the
    /// branch or the pull request.
    pub fn destination(&self, detail: &str) -> String {
        if detail.is_empty() {
            self.settings.policy.repository().to_string()
        } else {
            format!("{}#{detail}", self.settings.policy.repository())
        }
    }

    /// Push one bundle to one branch, after the ref rules were applied.
    pub fn publish(&self, branch: &str, bytes: &[u8]) -> Result<String, Problem> {
        let bundle =
            Bundle::parse(bytes).map_err(|why| Problem::invalid(format!("the bundle cannot be published: {why}")))?;
        bundle.branch(branch).map_err(|why| Problem::invalid(format!("the bundle cannot be published: {why}")))?;
        let mut publisher = Publisher::new(
            self.settings.policy.clone(),
            self.settings.task.clone(),
            self.settings.repository_url.clone(),
            self.settings.scratch.clone(),
        )
        .with_token(self.settings.token.clone())
        .with_cancel(self.cancel.clone());
        if let Some(git) = &self.settings.git {
            publisher = publisher.with_git(git.clone());
        }
        let published = {
            let _turn = self.publishing.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            publisher.publish(branch, bytes)?
        };
        Ok(json!({
            "repository": self.settings.policy.repository(),
            "branch": published.branch,
            "commit": published.commit,
            "previous": published.previous,
            "created": published.previous.is_none(),
        })
        .to_string())
    }

    /// Branches, and which of them is this task's. A read: no approval.
    pub fn refs(&self) -> Result<String, Problem> {
        let path = format!("{}git/matching-refs/heads/", self.prefix());
        let reply = self.call("GET", &path, None)?;
        let list = reply.as_array().ok_or_else(|| Problem::unavailable("the branches reply is not a list"))?;
        let task_branch = Policy::task_branch(&self.settings.task);
        let mut branches = Vec::new();
        for item in list {
            let reference = item.get("ref").and_then(Value::as_str).unwrap_or_default();
            if !reference.starts_with(HEADS) {
                continue;
            }
            let name = reference.get(HEADS.len()..).unwrap_or_default();
            let commit = item.pointer("/object/sha").and_then(Value::as_str).unwrap_or_default();
            let task = name == task_branch;
            branches.push(json!({ "branch": name, "commit": commit, "task_branch": task }));
        }
        Ok(json!({ "repository": self.settings.policy.repository(), "branches": branches }).to_string())
    }

    /// Create a pull request from the task's branch (or a listed one) to the
    /// configured base.
    pub fn pull_request(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<String, Problem> {
        if !self.settings.policy.writable(head, &self.settings.task) {
            return Err(Problem::refused(format!("{head:?} is not a branch this task may open a pull request from")));
        }
        if !self.settings.policy.accepts_base(base) {
            return Err(Problem::refused(format!(
                "{base:?} is not a base this task may target (only {:?})",
                self.settings.policy.base()
            )));
        }
        check_text(title, body)?;
        let path = format!("{}pulls", self.prefix());
        let request = json!({ "title": title, "body": body, "head": head, "base": base, "draft": draft });
        let reply = self.call("POST", &path, Some(&request))?;
        Ok(summary(&reply).to_string())
    }

    /// Change what a pull request says: its title, its body, or draft state.
    /// Its base moves only forward, onto a branch the task may target.
    pub fn update_pull_request(
        &self,
        number: u64,
        base: Option<&str>,
        title: Option<&str>,
        body: Option<&str>,
        draft: Option<bool>,
    ) -> Result<String, Problem> {
        if let Some(base) = base
            && !self.settings.policy.accepts_base(base)
        {
            return Err(Problem::refused(format!("{base:?} is not a base this task may target")));
        }
        if title.is_some() || body.is_some() {
            check_text(title.unwrap_or("-"), body.unwrap_or_default())?;
        }
        let mut request = serde_json::Map::new();
        if let Some(title) = title {
            request.insert("title".into(), json!(title));
        }
        if let Some(body) = body {
            request.insert("body".into(), json!(body));
        }
        if let Some(base) = base {
            request.insert("base".into(), json!(base));
        }
        if let Some(draft) = draft {
            request.insert("draft".into(), json!(draft));
        }
        if request.is_empty() {
            return Err(Problem::invalid("an update names a title, a body, a base or the draft state"));
        }
        let path = format!("{}pulls/{number}", self.prefix());
        let reply = self.call("PATCH", &path, Some(&Value::Object(request)))?;
        Ok(summary(&reply).to_string())
    }

    /// Whether pull request `number` is one this task may change: its head is
    /// a branch the task may write, in this repository rather than a fork.
    /// Asked before the approval card, so the user is never asked about a pull
    /// request the gateway would refuse anyway.
    pub fn owns_pull_request(&self, number: u64) -> Result<(), Problem> {
        let path = format!("{}pulls/{number}", self.prefix());
        let reply = self.call("GET", &path, None)?;
        let head = reply.pointer("/head/ref").and_then(Value::as_str).unwrap_or_default();
        let repository = reply.pointer("/head/repo/full_name").and_then(Value::as_str).unwrap_or_default();
        if !repository.eq_ignore_ascii_case(self.settings.policy.repository()) {
            return Err(Problem::refused(format!(
                "pull request {number} is not from a branch of {}",
                self.settings.policy.repository()
            )));
        }
        if !self.settings.policy.writable(head, &self.settings.task) {
            return Err(Problem::refused(format!(
                "pull request {number} is from {head:?}, not a branch this task may write"
            )));
        }
        Ok(())
    }

    /// A pull request's state: head commit, draft, mergeability, and the
    /// check runs on its head commit (`null` when it names no head commit).
    pub fn review(&self, number: u64) -> Result<String, Problem> {
        let path = format!("{}pulls/{number}", self.prefix());
        let reply = self.call("GET", &path, None)?;
        let mut out = summary(&reply);
        out["checks"] = match reply.pointer("/head/sha").and_then(Value::as_str) {
            Some(sha) if is_sha(sha) => self.checks(sha)?,
            _ => Value::Null,
        };
        Ok(out.to_string())
    }

    /// The check runs on one commit, reduced to what an agent decides on:
    /// each run's name, status and conclusion. One page of 100 is the bound.
    fn checks(&self, sha: &str) -> Result<Value, Problem> {
        let path = format!("{}commits/{sha}/check-runs?per_page=100", self.prefix());
        let reply = self.call("GET", &path, None)?;
        let runs: Vec<Value> = reply
            .get("check_runs")
            .and_then(Value::as_array)
            .map(|runs| {
                runs.iter()
                    .map(|run| {
                        json!({
                            "name": run.get("name").cloned().unwrap_or(Value::Null),
                            "status": run.get("status").cloned().unwrap_or(Value::Null),
                            "conclusion": run.get("conclusion").cloned().unwrap_or(Value::Null),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(json!({ "total": reply.get("total_count").cloned().unwrap_or(json!(runs.len())), "runs": runs }))
    }

    fn prefix(&self) -> String {
        format!("{}{}{}/", self.settings.api.trim_end_matches('/'), API_PREFIX, self.settings.policy.repository())
    }

    /// One API call with the host's credential. The guest contributed the
    /// path only through arguments this module already validated.
    pub(crate) fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, Problem> {
        let Some(token) = self.settings.token.as_ref().filter(|token| !token.is_empty()) else {
            return Err(Problem::credential("the host has no GitHub credential"));
        };
        let headers: [(&str, String); 4] = [
            ("accept", "application/vnd.github+json".into()),
            ("x-github-api-version", "2022-11-28".into()),
            ("user-agent", AGENT.into()),
            ("authorization", format!("Bearer {token}")),
        ];
        let payload = body.unwrap_or(&Value::Null).to_string();
        let response = match method {
            "GET" => {
                let mut builder = self.agent.get(path);
                for (name, value) in &headers {
                    builder = builder.header(*name, value.as_str());
                }
                builder.call()
            }
            "POST" => {
                let mut builder = self.agent.post(path);
                for (name, value) in &headers {
                    builder = builder.header(*name, value.as_str());
                }
                builder.send(payload.as_bytes())
            }
            "PATCH" => {
                let mut builder = self.agent.patch(path);
                for (name, value) in &headers {
                    builder = builder.header(*name, value.as_str());
                }
                builder.send(payload.as_bytes())
            }
            other => return Err(Problem::unavailable(format!("{other} is not a GitHub method this gateway makes"))),
        };
        let mut response = match response {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(status)) => {
                return Err(Problem::new(Kind::of_status(status), format!("github answered {status}")));
            }
            Err(error) => return Err(Problem::unavailable(format!("reaching {API_HOST}: {error}"))),
        };
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(MAX_RESPONSE as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| Problem::unavailable(format!("reading github's reply: {error}")))?;
        let text = String::from_utf8_lossy(&bytes);
        if !(200..300).contains(&status) {
            let reason = github_reason(status, &text, token);
            // GitHub reports a rate limit as 403 too; that one passes.
            let kind = match Kind::of_status(status) {
                Kind::Credential if reason.to_ascii_lowercase().contains("rate limit") => Kind::Unavailable,
                kind => kind,
            };
            return Err(Problem::new(kind, reason));
        }
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|_| Problem::unavailable("github's reply is not JSON"))
    }
}

pub(super) fn check_text(title: &str, body: &str) -> Result<(), Problem> {
    if title.trim().is_empty() {
        return Err(Problem::invalid("a pull request needs a title"));
    }
    if title.len() + body.len() > MAX_TEXT {
        return Err(Problem::invalid(format!("the title and body exceed {MAX_TEXT} bytes")));
    }
    Ok(())
}

/// GitHub's reason for a refusal: its `message`, then the first entry of
/// `errors` (where a 422 says what was wrong), with the token never echoed.
fn github_reason(status: u16, text: &str, token: &str) -> String {
    let value = serde_json::from_str::<Value>(text).ok();
    let message = value.as_ref().and_then(|value| value.get("message")).and_then(Value::as_str);
    let detail = value.as_ref().and_then(|value| value.pointer("/errors/0")).and_then(|error| {
        error.get("message").and_then(Value::as_str).map(str::to_owned).or_else(|| {
            error.get("code").and_then(Value::as_str).map(|code| {
                let field = error.get("field").and_then(Value::as_str).unwrap_or("request");
                format!("{field} {code}")
            })
        })
    });
    let reason = match (message, detail) {
        (Some(message), Some(detail)) => format!("github answered {status}: {message}: {detail}"),
        (Some(message), None) => format!("github answered {status}: {message}"),
        (None, Some(detail)) => format!("github answered {status}: {detail}"),
        (None, None) => format!("github answered {status}"),
    };
    // A GitHub token is never this short; replacing a one-letter test token
    // would only garble GitHub's sentence.
    if token.len() < MIN_TOKEN { reason } else { reason.replace(token, "***") }
}

/// The fields an agent reads from a pull request, and nothing more.
fn summary(reply: &Value) -> Value {
    json!({
        "number": reply.get("number").cloned().unwrap_or(Value::Null),
        "title": reply.get("title").cloned().unwrap_or(Value::Null),
        "state": reply.get("state").cloned().unwrap_or(Value::Null),
        "draft": reply.get("draft").cloned().unwrap_or(Value::Null),
        "head": reply.pointer("/head/ref").cloned().unwrap_or(Value::Null),
        "head_commit": reply.pointer("/head/sha").cloned().unwrap_or(Value::Null),
        "base": reply.pointer("/base/ref").cloned().unwrap_or(Value::Null),
        "mergeable": reply.get("mergeable").cloned().unwrap_or(Value::Null),
        "html_url": reply.get("html_url").cloned().unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Client {
        Client::new(Settings {
            api: "https://api.github.com".into(),
            policy: Policy::new("O/R", "main", Vec::new()).unwrap(),
            task: "t-1".into(),
            repository_url: "https://github.com/o/r".into(),
            token: None,
            scratch: PathBuf::from("/tmp"),
            git: None,
        })
    }

    #[test]
    fn every_url_is_built_from_the_policy_repository() {
        let client = client();
        assert_eq!(client.prefix(), "https://api.github.com/repos/o/r/");
        assert_eq!(client.destination(""), "o/r");
        assert_eq!(client.destination("deck/t-1"), "o/r#deck/t-1");
        assert!(!client.has_token());
    }

    #[test]
    fn a_pull_request_from_or_to_a_branch_outside_the_policy_is_refused_first() {
        let client = client();
        let refused = |result: Result<String, Problem>, text: &str, kind: Kind| {
            let problem = result.unwrap_err();
            assert_eq!(problem.kind, kind, "{problem:?}");
            assert!(problem.message.contains(text), "{problem:?}");
        };
        refused(client.pull_request("main", "main", "t", "", false), "may open", Kind::Refused);
        refused(client.pull_request("deck/t-1", "release", "t", "", false), "may target", Kind::Refused);
        refused(client.pull_request("deck/t-1", "main", "", "", false), "needs a title", Kind::Invalid);
        refused(client.pull_request("deck/t-1", "main", "t", "", false), "credential", Kind::Credential);
        refused(client.update_pull_request(1, Some("release"), None, None, None), "may target", Kind::Refused);
        refused(client.update_pull_request(1, None, None, None, None), "names a title", Kind::Invalid);
    }

    #[test]
    fn githubs_reason_names_what_was_wrong_and_never_the_token() {
        let reply =
            r#"{"message":"Validation Failed","errors":[{"message":"A pull request already exists for o:deck/t-1."}]}"#;
        assert_eq!(
            github_reason(422, reply, "ghp_token"),
            "github answered 422: Validation Failed: A pull request already exists for o:deck/t-1."
        );
        let coded =
            r#"{"message":"Validation Failed","errors":[{"resource":"PullRequest","field":"base","code":"invalid"}]}"#;
        assert_eq!(github_reason(422, coded, "ghp_token"), "github answered 422: Validation Failed: base invalid");
        assert_eq!(github_reason(401, r#"{"message":"bad ghp_token"}"#, "ghp_token"), "github answered 401: bad ***");
        assert_eq!(github_reason(500, "<html>", "ghp_token"), "github answered 500");
    }

    #[test]
    fn a_summary_keeps_the_review_fields_and_drops_the_rest() {
        let reply = json!({
            "number": 7, "title": "t", "state": "open", "draft": true,
            "head": {"ref": "deck/t-1", "sha": "abc"}, "base": {"ref": "main"},
            "mergeable": false, "html_url": "https://github.com/o/r/pull/7",
            "body": "not returned", "user": {"login": "x"}
        });
        let summary = summary(&reply);
        assert_eq!(summary["number"], 7);
        assert_eq!(summary["head_commit"], "abc");
        assert!(summary.get("body").is_none());
        assert!(summary.get("user").is_none());
    }
}
