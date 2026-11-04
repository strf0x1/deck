//! The GitHub REST calls the gateway makes, and none it does not (ADR 0007
//! §1).
//!
//! The design choice: every URL is built here from three host-side pieces
//! (the API base, `repos/owner/name`, and a path this module wrote) and every
//! body is a bounded object this module encoded. The guest never contributes a
//! URL, a method or a header, so there is no request it can shape into an
//! endpoint the operation set does not name. A redirect is not followed: an
//! API that answers 301 to another host would be egress the policy never saw.

use crate::github::bundle::Bundle;
use crate::github::policy::{API_HOST, API_PREFIX, Policy};
use crate::github::publish::{Publisher, Refused};
use serde_json::{Value, json};
use std::io::Read as _;
use std::path::PathBuf;
use std::time::Duration;

/// A pull request body is a description, not an attachment.
pub const MAX_TEXT: usize = 64 * 1024;
/// The largest API response the gateway will read. `list_commits` is the
/// largest thing it asks for, and a page of it fits easily.
const MAX_RESPONSE: usize = 4 * 1024 * 1024;
/// The `User-Agent` GitHub requires on every request.
const AGENT: &str = "deck-gateway";
/// The prefix a `matching-refs/heads/` reply puts in front of each short name.
const HEADS: &str = "refs/heads/";

pub struct Settings {
    /// `https://api.github.com`, or a test's own listener.
    pub api: String,
    pub policy: Policy,
    pub task: String,
    /// The pinned clone URL (`https://github.com/owner/name`), or a test's.
    pub repository_url: String,
    /// The host token. Never written, never returned.
    pub token: Option<String>,
    pub scratch: PathBuf,
    pub git: Option<PathBuf>,
}

pub struct Client {
    settings: Settings,
    agent: ureq::Agent,
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
        Self { settings, agent: config.new_agent() }
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
    pub fn publish(&self, branch: &str, bytes: &[u8]) -> Result<String, String> {
        let bundle = Bundle::parse(bytes).map_err(|why| Refused::Bundle(why).message())?;
        bundle.branch(branch).map_err(|why| Refused::Bundle(why).message())?;
        let mut publisher = Publisher::new(
            self.settings.policy.clone(),
            self.settings.task.clone(),
            self.settings.repository_url.clone(),
            self.settings.scratch.clone(),
        )
        .with_token(self.settings.token.clone());
        if let Some(git) = &self.settings.git {
            publisher = publisher.with_git(git.clone());
        }
        let published = publisher.publish(branch, bytes).map_err(|refused| refused.message())?;
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
    pub fn refs(&self) -> Result<String, String> {
        let path = format!("{}git/matching-refs/heads/", self.prefix());
        let reply = self.call("GET", &path, None)?;
        let list = reply.as_array().ok_or("the branches reply is not a list")?;
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
    pub fn pull_request(&self, head: &str, base: &str, title: &str, body: &str, draft: bool) -> Result<String, String> {
        if !self.settings.policy.writable(head, &self.settings.task) {
            return Err(format!("{head:?} is not a branch this task may open a pull request from"));
        }
        if !self.settings.policy.accepts_base(base) {
            return Err(format!(
                "{base:?} is not a base this task may target (only {:?})",
                self.settings.policy.base()
            ));
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
    ) -> Result<String, String> {
        if let Some(base) = base
            && !self.settings.policy.accepts_base(base)
        {
            return Err(format!("{base:?} is not a base this task may target"));
        }
        if let (Some(title), Some(body)) = (title, body) {
            check_text(title, body)?;
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
            return Err("an update names a title, a body, a base or the draft state".to_string());
        }
        let path = format!("{}pulls/{number}", self.prefix());
        let reply = self.call("PATCH", &path, Some(&Value::Object(request)))?;
        Ok(summary(&reply).to_string())
    }

    /// A pull request's state: head commit, draft, mergeability and checks.
    pub fn review(&self, number: u64) -> Result<String, String> {
        let path = format!("{}pulls/{number}", self.prefix());
        let reply = self.call("GET", &path, None)?;
        let mut out = summary(&reply);
        out["checks"] = Value::Null;
        Ok(out.to_string())
    }

    fn prefix(&self) -> String {
        format!("{}{}{}/", self.settings.api.trim_end_matches('/'), API_PREFIX, self.settings.policy.repository())
    }

    /// One API call with the host's credential. The guest contributed the
    /// path only through arguments this module already validated.
    pub(crate) fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let Some(token) = self.settings.token.as_ref().filter(|token| !token.is_empty()) else {
            return Err("the host has no GitHub credential".to_string());
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
            other => return Err(format!("{other} is not a GitHub method this gateway makes")),
        };
        let mut response = match response {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(status)) => return Err(format!("github answered {status}")),
            Err(error) => return Err(format!("reaching {API_HOST}: {error}")),
        };
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(MAX_RESPONSE as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("reading github's reply: {error}"))?;
        let text = String::from_utf8_lossy(&bytes);
        if !(200..300).contains(&status) {
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|value| value.get("message").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| format!("github answered {status}"));
            return Err(message);
        }
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|_| "github's reply is not JSON".to_string())
    }
}

fn check_text(title: &str, body: &str) -> Result<(), String> {
    if title.trim().is_empty() {
        return Err("a pull request needs a title".to_string());
    }
    if title.len() + body.len() > MAX_TEXT {
        return Err(format!("the title and body exceed {MAX_TEXT} bytes"));
    }
    Ok(())
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
        assert!(client.pull_request("main", "main", "t", "", false).unwrap_err().contains("may open"));
        assert!(client.pull_request("deck/t-1", "release", "t", "", false).unwrap_err().contains("may target"));
        assert!(client.pull_request("deck/t-1", "main", "", "", false).unwrap_err().contains("needs a title"));
        assert!(client.pull_request("deck/t-1", "main", "t", "", false).unwrap_err().contains("credential"));
        assert!(client.update_pull_request(1, Some("release"), None, None, None).is_err());
        assert!(client.update_pull_request(1, None, None, None, None).unwrap_err().contains("names a title"));
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
