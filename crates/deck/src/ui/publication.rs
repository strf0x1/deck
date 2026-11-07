//! What a task has published, as the window shows it: one chip with one state
//! dot, and the rows of the publication panel (ADR 0008 §8).
//!
//! The design choice: everything here is derived from `deckd`'s answer
//! (`pull_request::State`: the recorded publication plus the last live read)
//! and nothing is a control. GitHub owns merge, close and ready; the agent
//! owns push. The panel only reads, links out, and, once the pull request is
//! merged, offers to destroy the task.

use deck_runtime::Publication;
use deck_runtime::pull_request::State;
use serde_json::Value;

/// The one dot a chip shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dot {
    /// A branch is published and no pull request exists yet.
    Pushed,
    Draft,
    ChecksRunning,
    ChecksFailing,
    /// Open, not a draft, every check passed (or none ran).
    Ready,
    Merged,
    Closed,
}

impl Dot {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pushed => "pushed",
            Self::Draft => "draft",
            Self::ChecksRunning => "checks running",
            Self::ChecksFailing => "checks failing",
            Self::Ready => "ready",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }
}

/// One check run's name and verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub verdict: String,
    pub failed: bool,
}

/// The panel's rows and the chip, for one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub dot: Dot,
    /// `#7`, or the branch name before a pull request exists.
    pub label: String,
    pub branch: Option<String>,
    /// The last published commit, abbreviated.
    pub commit: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub checks: Vec<Check>,
    /// When the state was read and whether it is from before, or why the
    /// last read failed; `None` when no live read exists.
    pub note: Option<String>,
}

impl View {
    /// A merged pull request is the end of the task's work (ADR 0008 §8).
    pub fn done(&self) -> bool {
        self.dot == Dot::Merged
    }
}

const FAILED: &[&str] = &["failure", "cancelled", "timed_out", "action_required", "startup_failure"];

fn short(commit: &str) -> String {
    commit.chars().take(7).collect()
}

/// The view of `state`, or `None` when the task has published nothing.
pub fn view(state: &State, now_ms: u64) -> Option<View> {
    let publication = state.publication.as_ref()?;
    let live = state.live.as_ref();
    let pr = live.and_then(|l| l.pull_request.as_ref());
    let note = live.map(|l| match &l.error {
        Some(error) => format!("last read failed: {error}"),
        None if state.fresh => "read just now".into(),
        None => format!("read {} ago", ago(now_ms.saturating_sub(l.read_at_ms))),
    });
    let checks: Vec<Check> = pr
        .and_then(|p| p.pointer("/checks/runs"))
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .map(|run| {
                    let conclusion = run["conclusion"].as_str();
                    let verdict = conclusion.or(run["status"].as_str()).unwrap_or("unknown").to_string();
                    Check {
                        name: run["name"].as_str().unwrap_or("check").into(),
                        verdict,
                        failed: conclusion.is_some_and(|c| FAILED.contains(&c)),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    match publication {
        Publication::Branch { branch, commit } => {
            let tip = live.and_then(|l| l.branch_commit.as_deref()).unwrap_or(commit);
            Some(View {
                dot: Dot::Pushed,
                label: branch.clone(),
                branch: Some(branch.clone()),
                commit: Some(short(tip)),
                title: None,
                url: None,
                checks,
                note,
            })
        }
        Publication::PullRequest { number, url, title, draft } => {
            let field = |key: &str| pr.and_then(|p| p.get(key));
            let merged = field("merged").and_then(Value::as_bool).unwrap_or(false);
            let closed = field("state").and_then(Value::as_str) == Some("closed");
            let draft = field("draft").and_then(Value::as_bool).unwrap_or(*draft);
            let running = pr
                .and_then(|p| p.pointer("/checks/runs"))
                .and_then(Value::as_array)
                .is_some_and(|runs| runs.iter().any(|r| r["status"].as_str().is_some_and(|s| s != "completed")));
            let dot = match () {
                _ if merged => Dot::Merged,
                _ if closed => Dot::Closed,
                _ if draft => Dot::Draft,
                _ if checks.iter().any(|c| c.failed) => Dot::ChecksFailing,
                _ if running => Dot::ChecksRunning,
                _ => Dot::Ready,
            };
            let text = |key: &str| field(key).and_then(Value::as_str).map(str::to_string);
            Some(View {
                dot,
                label: format!("#{number}"),
                branch: text("head"),
                commit: text("head_commit").map(|c| short(&c)),
                title: text("title").or_else(|| Some(title.clone())),
                url: text("html_url").or_else(|| Some(url.clone())),
                checks,
                note,
            })
        }
    }
}

fn ago(ms: u64) -> String {
    match ms / 1000 {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// The warning a VM task with a repository shows when its agent's tools
/// cannot publish (ADR 0008 §1). `publishes` is what the agent advertised;
/// `None` until its session is ready.
pub fn notice(publishes_here: bool, publishes: Option<bool>) -> Option<&'static str> {
    (publishes_here && publishes == Some(false)).then_some("this agent cannot publish from a sandbox")
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_runtime::pull_request::Live;
    use serde_json::json;

    fn pull(draft: bool) -> Publication {
        Publication::PullRequest {
            number: 7,
            url: "https://github.com/o/r/pull/7".into(),
            title: "Add b".into(),
            draft,
        }
    }

    fn with(pr: Value) -> State {
        State {
            publication: Some(pull(true)),
            live: Some(Live { read_at_ms: 1_000, pull_request: Some(pr), ..Live::default() }),
            fresh: false,
        }
    }

    fn runs(list: Value) -> Value {
        json!({ "state": "open", "draft": false, "merged": false, "head": "deck/t-1", "head_commit": "c".repeat(40),
                "checks": { "total": 2, "runs": list } })
    }

    #[test]
    fn the_dot_follows_github_merge_then_close_then_draft_then_checks() {
        let dot = |pr: Value| view(&with(pr), 1_000).unwrap().dot;
        assert_eq!(dot(json!({ "state": "closed", "merged": true, "draft": false })), Dot::Merged);
        assert_eq!(dot(json!({ "state": "closed", "merged": false })), Dot::Closed);
        assert_eq!(dot(json!({ "state": "open", "draft": true })), Dot::Draft);
        let failing = json!([{ "name": "ci", "status": "completed", "conclusion": "failure" },
                             { "name": "mac", "status": "in_progress", "conclusion": null }]);
        assert_eq!(dot(runs(failing)), Dot::ChecksFailing, "a failure outranks a run in progress");
        let running = json!([{ "name": "ci", "status": "queued", "conclusion": null }]);
        assert_eq!(dot(runs(running)), Dot::ChecksRunning);
        assert_eq!(dot(runs(json!([{ "name": "ci", "status": "completed", "conclusion": "success" }]))), Dot::Ready);
        assert!(view(&with(json!({ "state": "closed", "merged": true })), 0).unwrap().done());
    }

    #[test]
    fn the_panel_rows_come_from_the_live_read_and_fall_back_to_the_record() {
        let v = view(&with(runs(json!([{ "name": "ci", "status": "completed", "conclusion": "failure" }]))), 61_000)
            .unwrap();
        assert_eq!(v.label, "#7");
        assert_eq!(v.branch.as_deref(), Some("deck/t-1"));
        assert_eq!(v.commit.as_deref(), Some("ccccccc"));
        assert_eq!(v.checks, [Check { name: "ci".into(), verdict: "failure".into(), failed: true }]);
        assert_eq!(v.note.as_deref(), Some("read 1m ago"));
        // Recorded only, never read: the record's title, URL and draft flag.
        let recorded = view(&State { publication: Some(pull(true)), live: None, fresh: false }, 0).unwrap();
        assert_eq!((recorded.dot, recorded.title.as_deref()), (Dot::Draft, Some("Add b")));
        assert_eq!(recorded.url.as_deref(), Some("https://github.com/o/r/pull/7"));
        assert_eq!(recorded.note, None);
    }

    #[test]
    fn a_pushed_branch_shows_its_name_and_tip_and_a_failed_read_says_so() {
        let state = State {
            publication: Some(Publication::Branch { branch: "deck/t-1".into(), commit: "a".repeat(40) }),
            live: Some(Live {
                read_at_ms: 0,
                branch_commit: Some("b".repeat(40)),
                error: Some("deck_github_failed (502): x".into()),
                ..Live::default()
            }),
            fresh: true,
        };
        let v = view(&state, 0).unwrap();
        assert_eq!((v.dot, v.label.as_str(), v.commit.as_deref()), (Dot::Pushed, "deck/t-1", Some("bbbbbbb")));
        assert_eq!(v.note.as_deref(), Some("last read failed: deck_github_failed (502): x"));
        assert!(
            view(&State { publication: None, live: None, fresh: false }, 0).is_none(),
            "nothing published, no chip"
        );
    }

    #[test]
    fn only_a_sandbox_with_a_repository_and_a_non_publishing_agent_warns() {
        assert_eq!(notice(true, Some(false)), Some("this agent cannot publish from a sandbox"));
        assert_eq!(notice(true, None), None, "not until the session is ready");
        assert_eq!(notice(true, Some(true)), None);
        assert_eq!(notice(false, Some(false)), None, "a local task or no repository has no gateway");
    }
}
