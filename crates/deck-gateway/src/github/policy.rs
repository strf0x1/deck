//! Which GitHub operation a task may perform, on which repository and which
//! refs (ADR 0007).
//!
//! The design choice: the guest names an operation, never a route. The
//! repository, the branch its task may write, the base a pull request may
//! target and the permission set are host state read from the daemon's
//! environment, and the operation set is compiled into this table. A request
//! that names an operation outside it is answered by name, so an agent learns
//! it is not permitted instead of retrying a 404.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// The gateway kind approvals, grants and events carry.
pub const GATEWAY: &str = "github";

/// The profile name a project rule uses for this gateway, beside the web
/// profiles. `github_profile` expands it on the daemon side.
pub const PROFILE: &str = "github";

/// The GitHub API host and the API path prefix. The gateway builds every
/// request itself, so neither is guest input.
pub const API_HOST: &str = "api.github.com";
pub const API_PREFIX: &str = "/repos/";

/// The framed kind a connection names on the extension port.
pub const KIND: &str = "github";

/// One operation the gateway performs. The set is closed: a merge, a close, a
/// tag or a release is not in it and never becomes a generic relay (§1 of ADR
/// 0007).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Push a commit to the task's branch (or a branch the policy lists).
    Publish,
    /// List branches, so the agent can see what exists.
    Refs,
    /// Create a pull request to an allowed base.
    PullRequest,
    /// Change a pull request's title, body, draft state or base.
    UpdatePullRequest,
    /// Read a pull request's state and checks.
    Review,
}

impl Operation {
    /// The name approvals and grants carry for this operation.
    pub fn name(self) -> &'static str {
        match self {
            Self::Publish => "publish",
            Self::Refs => "refs",
            Self::PullRequest => "create_pr",
            Self::UpdatePullRequest => "update_pr",
            Self::Review => "review",
        }
    }

    /// The HTTP path the guest posts to. Nothing else is served.
    pub fn path(self) -> &'static str {
        match self {
            Self::Publish => "/operations/publish",
            Self::PullRequest => "/operations/pull_request",
            Self::UpdatePullRequest => "/operations/pull_request/update",
            Self::Refs => "/operations/refs",
            Self::Review => "/operations/pull_request/review",
        }
    }

    /// Whether the operation writes to GitHub and therefore asks the approval
    /// queue. A read of the task's own repository does not (§3).
    pub fn asks(self) -> bool {
        !matches!(self, Self::Refs | Self::Review)
    }

    /// Whether the request carries a git bundle body after its JSON head.
    pub fn carries_bundle(self) -> bool {
        matches!(self, Self::Publish)
    }

    /// The permission bits on the fine-grained token this operation needs.
    /// `None` is read-only access, which the same token's public-scope reads
    /// cover without a write permission.
    pub fn permission(self) -> Option<&'static str> {
        match self {
            Self::Publish => Some("contents: write"),
            Self::PullRequest | Self::UpdatePullRequest => Some("pull_requests: write"),
            Self::Refs | Self::Review => None,
        }
    }

    /// Operations the ADR names as refused by name rather than 404, so an
    /// agent that asks learns why.
    pub fn refused() -> &'static [&'static str] {
        &["merge", "close", "delete", "tag", "release", "force_push", "workflow", "dispatch", "settings"]
    }
}

/// The project's GitHub policy: the repository, the refs and the branches a
/// task may write. Read from the daemon's environment (ADR 0007 §2); a task
/// with no repository has no gateway at all, which is decided by the runtime
/// before this structure exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// `owner/name`, lowercased.
    repository: String,
    /// The base a pull request may target.
    base: String,
    /// Branches beyond `deck/<task>` the project allows a task to write.
    branches: Vec<String>,
}

impl Policy {
    /// `branch` is the task's own branch. A policy file (`branches`) widens
    /// the writable set only to names it lists; there is no wildcard.
    pub fn new(repository: &str, base: &str, branches: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let repository = repository_name(repository)?;
        let base = branch_name(base).ok_or_else(|| format!("{base:?} is not a branch name"))?;
        let mut writable = Vec::new();
        for branch in branches {
            let branch = branch.trim();
            if branch.is_empty() {
                continue;
            }
            let name = branch_name(branch).ok_or_else(|| format!("{branch:?} is not a branch name"))?;
            writable.push(name);
        }
        Ok(Self { repository, base, branches: writable })
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// The branch this task publishes to. It is derived from the task, so the
    /// guest cannot choose a name on the shared repository.
    pub fn task_branch(task: &str) -> String {
        format!("deck/{task}")
    }

    /// Whether `branch` is one this task may write: its own, or one the
    /// project listed.
    pub fn writable(&self, branch: &str, task: &str) -> bool {
        branch == Self::task_branch(task) || self.branches.iter().any(|allowed| allowed == branch)
    }

    /// Whether a pull request may target `base`: the configured base only.
    pub fn accepts_base(&self, base: &str) -> bool {
        base == self.base
    }
}

/// `owner/name`, lowercased. A path, a URL or a wildcard is not a repository.
pub fn repository_name(repository: &str) -> Result<String, String> {
    let value = repository.trim();
    let parts: Vec<&str> = value.split('/').collect();
    let ok = parts.len() == 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.len() <= 100
                && *part != "."
                && *part != ".."
                && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        });
    if !ok {
        return Err(format!("{value:?} is not an explicit OWNER/REPOSITORY on github.com"));
    }
    Ok(value.to_ascii_lowercase())
}

/// What `git check-ref-format --branch` accepts, narrowed: ascii letters,
/// digits and `._-` and `/`, no `..`, no leading or trailing `/` or `.`, no
/// `@{`, no `*`, no `?`, no `[`, never `HEAD`. A branch the policy can name is
/// one `git` will accept, so the host never builds a command git refuses.
pub fn branch_name(branch: &str) -> Option<String> {
    let branch = branch.trim();
    if branch.is_empty() || branch.len() > 200 || branch == "HEAD" || branch.starts_with('-') {
        return None;
    }
    let bytes = branch.as_bytes();
    let ok = branch.split('/').all(|part| {
        !part.is_empty()
            && !part.starts_with('.')
            && !part.ends_with('.')
            && !part.ends_with(".lock")
            && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    }) && !bytes.windows(2).any(|window| window == b"..");
    ok.then(|| branch.to_string())
}

/// The live project rule `deckd` writes for this gateway, re-read when it
/// changes the way the web grants are (ADR 0004 §2). A rule names one
/// operation and the repository; the destination is `owner/name`.
#[derive(Debug, Default)]
pub struct Rules {
    file: Option<PathBuf>,
    cached: RwLock<Cached>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Cached {
    stamp: Stamp,
    /// Bit per [`Operation`], so a rule read is a compare.
    allowed: u8,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<std::time::SystemTime>,
    length: u64,
}

impl Rules {
    pub fn new(file: Option<PathBuf>) -> Self {
        Self { file, cached: RwLock::default() }
    }

    /// Whether the project has an always-allow rule for this operation.
    pub fn allows(&self, operation: Operation, repository: &str) -> bool {
        let Some(file) = &self.file else { return false };
        let stamp = stamp(file);
        {
            let cached = self.cached.read().unwrap_or_else(|poisoned| poisoned.into_inner());
            if cached.stamp == stamp {
                return cached.allowed & bit(operation) != 0;
            }
        }
        let allowed = rules(file, repository);
        let mut cached = self.cached.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        cached.stamp = stamp;
        cached.allowed = allowed;
        allowed & bit(operation) != 0
    }
}

fn bit(operation: Operation) -> u8 {
    1 << (operation as u8)
}

fn stamp(path: &Path) -> Stamp {
    match std::fs::metadata(path) {
        Ok(meta) => Stamp { modified: meta.modified().ok(), length: meta.len() },
        Err(_) => Stamp::default(),
    }
}

/// The operations the project rule file names for this repository. Every
/// grant shape shares `gateway`, `operation` and `destination`, so the same
/// file the web gateway reads also serves this one.
fn rules(path: &Path, repository: &str) -> u8 {
    let Ok(bytes) = std::fs::read(path) else { return 0 };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return 0 };
    let list = match &value {
        serde_json::Value::Array(items) => items.clone(),
        serde_json::Value::Object(object) => match object.get("rules").and_then(|rules| rules.as_array()) {
            Some(rules) => rules.clone(),
            None => return 0,
        },
        _ => return 0,
    };
    let mut allowed = 0;
    for grant in list {
        if grant.get("gateway").and_then(|value| value.as_str()) != Some(GATEWAY) {
            continue;
        }
        if grant.get("scope").and_then(|value| value.as_str()) == Some("once") {
            continue;
        }
        let destination = grant.get("destination").and_then(|value| value.as_str()).unwrap_or_default();
        if !destination.eq_ignore_ascii_case(repository) {
            continue;
        }
        let operation = grant.get("operation").and_then(|value| value.as_str()).unwrap_or_default();
        if let Some(known) = operations().find(|candidate| candidate.name() == operation) {
            allowed |= bit(*known);
        }
    }
    allowed
}

/// Every operation, so a rule or a request can be compared to the table.
pub fn operations() -> impl Iterator<Item = &'static Operation> {
    const ALL: [Operation; 5] =
        [Operation::Publish, Operation::Refs, Operation::PullRequest, Operation::UpdatePullRequest, Operation::Review];
    ALL.iter()
}

/// The operation a guest's `operation` field asks for, or the refusal message
/// for a name the gateway does not perform.
pub fn requested(operation: &str) -> Result<Operation, String> {
    if let Some(known) = operations().find(|candidate| candidate.name() == operation) {
        return Ok(*known);
    }
    if Operation::refused().contains(&operation) {
        return Err(format!(
            "{operation} is not a Deck GitHub operation; a task may publish, open or update a pull request, and read it"
        ));
    }
    Err(format!("{operation:?} is not a Deck GitHub operation"))
}

/// One bounded request body. An action asks for exactly one operation; a
/// publish request adds the branch and a bundle on the connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {
    pub operation: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub base: String,
    #[serde(default)]
    pub head: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub empty_body: bool,
    #[serde(default)]
    pub draft: Option<bool>,
    #[serde(default)]
    pub number: Option<u64>,
}

impl Ask {
    pub fn parse(bytes: &[u8]) -> Result<(Self, Operation), String> {
        let ask: Self =
            serde_json::from_slice(bytes).map_err(|error| format!("the request is not a JSON object: {error}"))?;
        let operation = requested(&ask.operation)?;
        Ok((ask, operation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy::new("Strf0x1/Deck", "main", Vec::new()).unwrap()
    }

    #[test]
    fn the_operation_set_is_closed_and_names_its_refusals() {
        assert_eq!(requested("publish").unwrap(), Operation::Publish);
        assert_eq!(requested("create_pr").unwrap(), Operation::PullRequest);
        for refused in ["merge", "tag", "force_push"] {
            let message = requested(refused).unwrap_err();
            assert!(message.contains("not a Deck GitHub operation"), "{message}");
            assert!(message.contains("may publish"), "a refusal explains the set: {message}");
        }
        assert!(requested("GET /repos/owner/name/git/refs").is_err());
        assert_eq!(operations().count(), 5);
    }

    #[test]
    fn writes_ask_and_reads_do_not() {
        assert!(Operation::Publish.asks());
        assert!(Operation::PullRequest.asks());
        assert!(Operation::UpdatePullRequest.asks());
        assert!(!Operation::Refs.asks());
        assert!(!Operation::Review.asks());
        assert!(Operation::Publish.carries_bundle());
        assert!(!Operation::Review.carries_bundle());
        assert_eq!(Operation::Publish.permission(), Some("contents: write"));
        assert_eq!(Operation::Review.permission(), None);
    }

    #[test]
    fn the_repository_is_explicit_and_case_insensitive() {
        assert_eq!(policy().repository(), "strf0x1/deck");
        for invalid in ["deck", "owner/repo/extra", "https://github.com/o/r", "../repo", "owner/..", "a/*"] {
            assert!(repository_name(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn a_task_writes_its_own_branch_and_only_listed_ones_elsewhere() {
        let policy = policy();
        assert!(policy.writable("deck/t-123", "t-123"));
        assert!(!policy.writable("deck/t-456", "t-123"), "another task's branch");
        assert!(!policy.writable("main", "t-123"), "the base is never a task write");
        assert!(!policy.writable("deck/*", "t-123"));
        assert_eq!(Policy::task_branch("t-123"), "deck/t-123");

        let shared = Policy::new("o/r", "main", ["deck/shared".to_string()]).unwrap();
        assert!(shared.writable("deck/shared", "t-123"));
        assert!(shared.writable("deck/t-123", "t-123"));
    }

    #[test]
    fn a_pull_request_targets_the_configured_base_only() {
        let policy = policy();
        assert!(policy.accepts_base("main"));
        assert!(!policy.accepts_base("release"));
        assert!(!policy.accepts_base(""));
    }

    #[test]
    fn branch_names_follow_gits_rules() {
        for ok in ["deck/t-1", "main", "feature/x-1.2"] {
            assert_eq!(branch_name(ok).as_deref(), Some(ok), "{ok}");
        }
        for bad in ["", "HEAD", "-x", "a..b", "x/", "/x", ".hidden", "x.lock", "a b", "a~b", "a^b", "a\\b"] {
            assert!(branch_name(bad).is_none(), "{bad}");
        }
        assert!(Policy::new("o/r", "not a branch", Vec::new()).is_err());
    }

    #[test]
    fn a_rule_matches_one_operation_on_the_repository() {
        let dir = std::env::temp_dir().join(format!("deck-github-rules-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("projects.json");
        std::fs::write(
            &path,
            r#"{"rules":[
                {"gateway":"github","operation":"publish","destination":"o/r","scope":"project"},
                {"gateway":"github","operation":"create_pr","destination":"o/r","scope":"once"},
                {"gateway":"github","operation":"update_pr","destination":"other/r","scope":"project"},
                {"gateway":"web","operation":"connect","destination":"o/r","scope":"project"}
            ]}"#,
        )
        .unwrap();
        let rules = Rules::new(Some(path.clone()));
        assert!(rules.allows(Operation::Publish, "o/r"));
        assert!(!rules.allows(Operation::PullRequest, "o/r"), "a once grant is not a project rule");
        assert!(!rules.allows(Operation::UpdatePullRequest, "o/r"), "another repository's rule");
        assert!(!rules.allows(Operation::Review, "o/r"));
        // A new file replaces the old set without a restart.
        std::fs::write(&path, r#"[]"#).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!rules.allows(Operation::Publish, "o/r"));
        assert!(!Rules::new(None).allows(Operation::Publish, "o/r"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ask_carries_its_operation() {
        let (ask, operation) = Ask::parse(br#"{"operation":"create_pr","base":"main","title":"t"}"#).unwrap();
        assert_eq!(operation, Operation::PullRequest);
        assert_eq!(ask.base, "main");
        assert!(Ask::parse(br#"{"operation":"merge"}"#).unwrap_err().contains("not a Deck"));
        assert!(Ask::parse(b"[]").is_err());
    }
}
