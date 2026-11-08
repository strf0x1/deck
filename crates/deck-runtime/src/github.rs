//! A task's GitHub policy (ADR 0008 §3): the repository, the base and extra
//! writable branches, the credential name, the approval mode, and the host git
//! identity the guest checkout uses.
//!
//! The design choice: the project's own checkout is the source — its `origin`
//! remote, then `.deck/project.toml` — read by the host once at create and
//! recorded in the task's spec. The guest never selects a repository, and one
//! `deckd` can serve projects with different repositories. `DECK_GITHUB_*` in
//! the daemon's environment survives only as a development override for every
//! newly created task.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The project file that overrides what `origin` implies.
pub const PROJECT_FILE: &str = ".deck/project.toml";
/// The repository (`owner/name`); absent means no GitHub gateway.
pub const REPOSITORY_ENV: &str = "DECK_GITHUB_REPOSITORY";
/// The base a pull request targets, when the project file says nothing.
pub const BASE_ENV: &str = "DECK_GITHUB_BASE";
/// Extra branches a task may write, when the project file says nothing.
pub const BRANCHES_ENV: &str = "DECK_GITHUB_BRANCHES";
/// Names the stored credential that holds the GitHub token.
pub const TOKEN_ENV: &str = "DECK_GITHUB_TOKEN_SECRET";
/// The default credential name for the GitHub token.
pub const TOKEN: &str = "DECK_GITHUB_TOKEN";

/// What asks for a card at the gateway (ADR 0008 §6; admission itself is a
/// later step, but the mode travels with the task now).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Approvals {
    /// Writes to the task's own branch are admitted without a card.
    #[default]
    OwnBranch,
    /// A card for every write (ADR 0007 §3).
    Ask,
    /// `own-branch`, including for a public repository.
    OwnBranchPublic,
}

impl Approvals {
    /// Whether this mode admits a write the gateway classified as its own
    /// branch. `public` is the repository's visibility, or `None` when the
    /// gateway could not read it; an unreadable repository is not assumed
    /// private.
    pub fn admits_own_branch(self, public: Option<bool>) -> bool {
        match self {
            Self::Ask => false,
            Self::OwnBranch => public == Some(false),
            Self::OwnBranchPublic => true,
        }
    }
}

/// The host's git identity, so commits the guest makes carry the user's name
/// and email (ADR 0008 §4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
}

/// One task's GitHub policy, recorded in its spec at create.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// `owner/name`, lowercased.
    pub repository: String,
    /// The only base a pull request may target.
    pub base: String,
    /// Branches beyond `deck/<task>` the project lets a task write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
    /// The stored credential name; its value is never in a task record.
    pub credential: String,
    /// The project's approval mode.
    #[serde(default)]
    pub approvals: Approvals,
    /// The host's commit identity.
    #[serde(default)]
    pub identity: Identity,
}

/// `DECK_GITHUB_*` from the daemon's environment. It applies to every newly
/// created task, so a test or a prototype does not need a project file; a
/// task's recorded spec is unchanged once created.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Override {
    repository: Option<String>,
    base: Option<String>,
    branches: Option<Vec<String>>,
    credential: Option<String>,
}

impl Override {
    /// The names this override sets, for the daemon's one log line.
    pub fn describe(&self) -> String {
        let mut fields = Vec::new();
        if let Some(repository) = &self.repository {
            fields.push(format!("{REPOSITORY_ENV}={repository}"));
        }
        if let Some(base) = &self.base {
            fields.push(format!("{BASE_ENV}={base}"));
        }
        if self.branches.is_some() {
            fields.push(BRANCHES_ENV.to_string());
        }
        if let Some(credential) = &self.credential {
            fields.push(format!("{TOKEN_ENV}={credential}"));
        }
        fields.join(", ")
    }

    /// Read the development override, or `None` when no name is set.
    pub fn from_env() -> Result<Option<Self>> {
        let value = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
        let repository = value(REPOSITORY_ENV);
        let base = value(BASE_ENV);
        let branches = value(BRANCHES_ENV).map(|value| {
            value
                .split(|character: char| character == ',' || character.is_whitespace())
                .filter(|branch| !branch.is_empty())
                .map(str::to_owned)
                .collect()
        });
        let credential = value(TOKEN_ENV);
        if repository.is_none() && base.is_none() && branches.is_none() && credential.is_none() {
            return Ok(None);
        }
        Ok(Some(Self { repository, base, branches, credential }))
    }
}

/// Read `cwd`'s GitHub policy, or `None` when the project names no GitHub
/// repository. A project file that cannot be read or parsed is an error: a
/// typo must not silently publish to `origin` instead.
pub fn resolve(cwd: &Path, over: Option<&Override>) -> Result<Option<Policy>> {
    let file = project_file(&project_root(cwd))?;
    let project = file.as_ref().and_then(|file| file.github.as_ref());
    let inferred = origin(cwd).as_deref().and_then(parse_github_remote);

    let repository = match over.and_then(|over| over.repository.as_deref()) {
        Some(repository) => repository_name(repository)?,
        None => match project.and_then(|github| github.repository.as_deref()) {
            Some(repository) => repository_name(repository)?,
            None => match inferred.clone() {
                Some(repository) => repository,
                None => return Ok(None),
            },
        },
    };

    let base = match over.and_then(|over| over.base.clone()).or_else(|| project.and_then(|github| github.base.clone()))
    {
        Some(base) => branch_name(&base).with_context(|| format!("{base:?} is not a branch name"))?,
        // `refs/remotes/origin/HEAD` describes the checkout's own origin. When
        // the policy names another repository, its default branch is unknown.
        None if inferred.as_deref() == Some(repository.as_str()) => {
            default_branch(cwd).unwrap_or_else(|| "main".into())
        }
        None => "main".into(),
    };

    let branches = match over.and_then(|over| over.branches.clone()) {
        Some(branches) => branches,
        None => project.map(|github| github.branches.clone()).unwrap_or_default(),
    };
    let branches = branches
        .into_iter()
        .filter(|branch| !branch.trim().is_empty())
        .map(|branch| branch_name(&branch).with_context(|| format!("{branch:?} is not a branch name")))
        .collect::<Result<Vec<_>>>()?;

    Ok(Some(Policy {
        repository,
        base,
        branches,
        credential: over.and_then(|over| over.credential.clone()).unwrap_or_else(|| TOKEN.into()),
        approvals: project.and_then(|github| github.approvals).unwrap_or_default(),
        identity: Identity { name: git_config(cwd, "user.name"), email: git_config(cwd, "user.email") },
    }))
}

/// The approval mode that applies to the task's next write. The recorded
/// policy is the base, but the project file is re-read so switching a project
/// to `ask` takes effect on its next operation. A file that cannot be read or
/// parsed fails closed to `Ask`: a typo must not silently admit writes.
pub fn live_approvals(cwd: &Path, recorded: Approvals) -> Approvals {
    match project_file(&project_root(cwd)) {
        Ok(Some(file)) => file.github.and_then(|github| github.approvals).unwrap_or_default(),
        Ok(None) => recorded,
        Err(_) => Approvals::Ask,
    }
}

/// The branch a task publishes to (ADR 0007 §2): `deck/<task>`. The gateway
/// derives the same name from the task id, so the guest checkout and the
/// accepted ref cannot disagree.
pub fn task_branch(task: &str) -> String {
    format!("deck/{task}")
}

/// `owner/name`, lowercased, from a GitHub remote URL. Only github.com's
/// HTTPS and SSH forms are accepted; another host is not this gateway's.
pub fn parse_github_remote(remote: &str) -> Option<String> {
    let remote = remote.trim();
    let rest = remote
        .strip_prefix("git@github.com:")
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .or_else(|| remote.strip_prefix("https://github.com/"))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    repository_name(rest).ok()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFile {
    #[serde(default)]
    github: Option<ProjectGithub>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectGithub {
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    branches: Vec<String>,
    #[serde(default)]
    approvals: Option<Approvals>,
}

/// The checkout root when `cwd` is inside one, so `.deck/project.toml` means
/// the project's file rather than whichever subdirectory the task runs in.
fn project_root(cwd: &Path) -> PathBuf {
    git(cwd, &["rev-parse", "--show-toplevel"]).map(PathBuf::from).unwrap_or_else(|| cwd.to_path_buf())
}

fn project_file(cwd: &Path) -> Result<Option<ProjectFile>> {
    let path = cwd.join(PROJECT_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn origin(cwd: &Path) -> Option<String> {
    git(cwd, &["remote", "get-url", "origin"])
}

fn default_branch(cwd: &Path) -> Option<String> {
    let head = git(cwd, &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])?;
    branch_name(head.strip_prefix("refs/remotes/origin/")?)
}

fn git_config(cwd: &Path, key: &str) -> String {
    git(cwd, &["config", "--get", key]).unwrap_or_default()
}

/// A local read; never prompts and never touches the network. A missing
/// `git`, a directory that is not a checkout, or an unset key is `None`.
/// Shared with `project`, which asks the same questions of a checkout.
pub(crate) fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").arg("-C").arg(cwd).args(args).env("GIT_TERMINAL_PROMPT", "0").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// `owner/name`, lowercased. A path, a URL or a wildcard is not a repository.
/// The gateway validates the same policy again (ADR 0007 §2); this copy makes
/// a bad project file fail at create rather than at gateway start.
fn repository_name(repository: &str) -> Result<String> {
    let value = repository.trim();
    let parts: Vec<&str> = value.split('/').collect();
    let ok = parts.len() == 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.len() <= 100
                && *part != "."
                && *part != ".."
                && part
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
        });
    if !ok {
        anyhow::bail!("{value:?} is not an explicit OWNER/REPOSITORY on github.com");
    }
    Ok(value.to_ascii_lowercase())
}

/// What `git check-ref-format --branch` accepts, narrowed as the gateway
/// narrows it: ascii letters, digits and `._-` and `/`, no `..`, no leading
/// or trailing `/` or `.`, and never `HEAD`.
fn branch_name(branch: &str) -> Option<String> {
    let branch = branch.trim();
    if branch.is_empty() || branch.len() > 200 || branch == "HEAD" || branch.starts_with('-') {
        return None;
    }
    let ok = branch.split('/').all(|part| {
        !part.is_empty()
            && !part.starts_with('.')
            && !part.ends_with('.')
            && !part.ends_with(".lock")
            && part.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
    }) && !branch.as_bytes().windows(2).any(|window| window == b"..");
    ok.then(|| branch.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deck-github-policy-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn checkout(name: &str, origin: Option<&str>, base: Option<&str>) -> PathBuf {
        let dir = scratch(name);
        git(&dir, &["init", "--quiet"]);
        git(&dir, &["config", "user.name", "Ada Lovelace"]);
        git(&dir, &["config", "user.email", "ada@example.com"]);
        if let Some(origin) = origin {
            git(&dir, &["remote", "add", "origin", origin]);
        }
        if let Some(base) = base {
            git(&dir, &["symbolic-ref", "refs/remotes/origin/HEAD", &format!("refs/remotes/origin/{base}")]);
        }
        dir
    }

    #[test]
    fn a_task_publishes_to_its_own_branch() {
        assert_eq!(task_branch("t-123"), "deck/t-123");
        assert_eq!(task_branch("abc"), "deck/abc");
    }

    #[test]
    fn github_remote_forms_are_normalized() {
        for (remote, expected) in [
            ("https://github.com/Strf0x1/Deck", Some("strf0x1/deck")),
            ("https://github.com/Strf0x1/Deck.git", Some("strf0x1/deck")),
            ("https://github.com/Strf0x1/Deck.git/", Some("strf0x1/deck")),
            ("git@github.com:Owner/Name.git", Some("owner/name")),
            ("ssh://git@github.com/owner/name.git", Some("owner/name")),
            ("http://github.com/owner/name", None),
            ("git://github.com/owner/name", None),
            ("https://gitlab.com/owner/name", None),
            ("https://github.com/owner", None),
            ("https://github.com/owner/name/tree/main", None),
            ("file:///home/owner/name", None),
        ] {
            assert_eq!(parse_github_remote(remote).as_deref(), expected, "{remote}");
        }
    }

    #[test]
    fn origin_identity_and_default_branch_are_inferred() {
        let dir = checkout("inferred", Some("git@github.com:Owner/Name.git"), Some("develop"));
        let policy = resolve(&dir, None).unwrap().expect("a GitHub origin names a repository");
        assert_eq!(policy.repository, "owner/name");
        assert_eq!(policy.base, "develop", "origin/HEAD names the base");
        assert_eq!(policy.credential, TOKEN);
        assert_eq!(policy.approvals, Approvals::OwnBranch);
        assert_eq!(policy.identity, Identity { name: "Ada Lovelace".into(), email: "ada@example.com".into() });
        assert!(policy.branches.is_empty());
    }

    #[test]
    fn no_origin_head_falls_back_to_main() {
        let dir = checkout("fallback", Some("https://github.com/owner/name"), None);
        assert_eq!(resolve(&dir, None).unwrap().unwrap().base, "main");
    }

    #[test]
    fn a_project_file_overrides_the_origin() {
        let dir = checkout("file", Some("git@github.com:first/one.git"), Some("trunk"));
        std::fs::create_dir_all(dir.join(".deck")).unwrap();
        std::fs::write(
            dir.join(PROJECT_FILE),
            r#"
[github]
repository = "Second/Two"
base = "develop"
branches = ["deck/shared", "release"]
approvals = "ask"
"#,
        )
        .unwrap();
        let policy = resolve(&dir, None).unwrap().unwrap();
        assert_eq!(policy.repository, "second/two");
        assert_eq!(policy.base, "develop");
        assert_eq!(policy.branches, ["deck/shared", "release"]);
        assert_eq!(policy.approvals, Approvals::Ask);
    }

    #[test]
    fn an_overridden_repository_does_not_borrow_the_origins_default_branch() {
        let dir = checkout("other-repo", Some("https://github.com/first/one"), Some("develop"));
        std::fs::create_dir_all(dir.join(".deck")).unwrap();
        std::fs::write(dir.join(PROJECT_FILE), "[github]\nrepository = \"second/two\"\n").unwrap();
        assert_eq!(resolve(&dir, None).unwrap().unwrap().base, "main");

        let over = Override { repository: Some("third/three".into()), ..Default::default() };
        assert_eq!(resolve(&dir, Some(&over)).unwrap().unwrap().base, "main");
    }

    #[test]
    fn a_project_file_for_the_origin_repository_keeps_the_origin_base() {
        let dir = checkout("same-repo", Some("https://github.com/First/One"), Some("develop"));
        std::fs::create_dir_all(dir.join(".deck")).unwrap();
        std::fs::write(dir.join(PROJECT_FILE), "[github]\nrepository = \"first/one\"\n").unwrap();
        assert_eq!(resolve(&dir, None).unwrap().unwrap().base, "develop");
    }

    #[test]
    fn a_project_file_is_read_from_the_checkout_root() {
        let dir = checkout("nested", Some("https://github.com/first/one"), None);
        std::fs::create_dir_all(dir.join(".deck")).unwrap();
        std::fs::write(dir.join(PROJECT_FILE), "[github]\nrepository = \"second/two\"\n").unwrap();
        let nested = dir.join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(resolve(&nested, None).unwrap().unwrap().repository, "second/two");
    }

    #[test]
    fn the_development_override_beats_the_project_file() {
        let dir = checkout("override", Some("git@github.com:first/one.git"), None);
        std::fs::create_dir_all(dir.join(".deck")).unwrap();
        std::fs::write(
            dir.join(PROJECT_FILE),
            r#"
[github]
repository = "second/two"
base = "develop"
branches = ["deck/shared"]
approvals = "ask"
"#,
        )
        .unwrap();
        let over = Override {
            repository: Some("Dev/Repo".into()),
            base: Some("trunk".into()),
            branches: Some(vec!["deck/dev".into()]),
            credential: Some("DEV_TOKEN".into()),
        };
        let policy = resolve(&dir, Some(&over)).unwrap().unwrap();
        assert_eq!(policy.repository, "dev/repo");
        assert_eq!(policy.base, "trunk");
        assert_eq!(policy.branches, ["deck/dev"]);
        assert_eq!(policy.credential, "DEV_TOKEN");
        assert_eq!(policy.approvals, Approvals::Ask, "approval mode stays the project's");
    }

    #[test]
    fn a_non_github_or_missing_origin_has_no_gateway() {
        let dir = checkout("gitlab", Some("https://gitlab.com/owner/name.git"), None);
        assert!(resolve(&dir, None).unwrap().is_none());
        let empty = checkout("empty", None, None);
        assert!(resolve(&empty, None).unwrap().is_none());
    }

    #[test]
    fn a_project_file_selects_a_repository_without_a_remote() {
        let dir = checkout("file-only", None, None);
        std::fs::create_dir_all(dir.join(".deck")).unwrap();
        std::fs::write(dir.join(PROJECT_FILE), "[github]\nrepository = \"owner/name\"\n").unwrap();
        assert_eq!(resolve(&dir, None).unwrap().unwrap().repository, "owner/name");
    }

    #[test]
    fn approval_modes_are_the_three_documented_names() {
        #[derive(Deserialize)]
        struct Mode {
            approvals: Approvals,
        }
        for (name, expected) in [
            ("own-branch", Approvals::OwnBranch),
            ("ask", Approvals::Ask),
            ("own-branch-public", Approvals::OwnBranchPublic),
        ] {
            let parsed: Mode = toml::from_str(&format!("approvals = \"{name}\"")).unwrap();
            assert_eq!(parsed.approvals, expected);
            assert_eq!(serde_json::to_value(parsed.approvals).unwrap(), serde_json::json!(name));
        }
        assert_eq!(Approvals::default(), Approvals::OwnBranch);
    }

    #[test]
    fn the_approval_mode_is_read_live_from_the_project_file() {
        let dir = checkout("live-mode", Some("https://github.com/owner/name"), None);
        std::fs::create_dir_all(dir.join(".deck")).unwrap();
        assert_eq!(
            live_approvals(&dir, Approvals::OwnBranch),
            Approvals::OwnBranch,
            "no file keeps the mode recorded at create"
        );
        std::fs::write(dir.join(PROJECT_FILE), "[github]\napprovals = \"ask\"\n").unwrap();
        assert_eq!(live_approvals(&dir, Approvals::OwnBranch), Approvals::Ask);
        std::fs::write(dir.join(PROJECT_FILE), "[github]\napprovals = \"always\"\n").unwrap();
        assert_eq!(live_approvals(&dir, Approvals::OwnBranch), Approvals::Ask, "a malformed file fails closed");
        std::fs::remove_file(dir.join(PROJECT_FILE)).unwrap();
        assert_eq!(live_approvals(&dir, Approvals::OwnBranchPublic), Approvals::OwnBranchPublic);
    }

    #[test]
    fn a_malformed_project_file_fails_at_create() {
        let dir = checkout("malformed", Some("https://github.com/owner/name"), None);
        std::fs::create_dir_all(dir.join(".deck")).unwrap();
        for text in [
            "[github]\napprovals = \"always\"\n",
            "[github]\nbranches = [\"not a branch\"]\n",
            "[github]\nrepositry = \"owner/name\"\n",
        ] {
            std::fs::write(dir.join(PROJECT_FILE), text).unwrap();
            assert!(resolve(&dir, None).is_err(), "{text:?}");
        }
    }
}
