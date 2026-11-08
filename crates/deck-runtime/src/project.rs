//! Projects (ADR 0010 §2–3): a checkout resolves to a project by its root
//! commit, and `deckd` keeps the project index under the data root.
//!
//! The design choice: the root commit is the identity, so clones, worktrees
//! and forks stay one project while a subtree merge that adds a second root
//! still matches the recorded one. A path that is not a git checkout uses its
//! canonical path and is never merged with a `git:` project.

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const INDEX: &str = "projects/index.json";

/// A checkout's project: the recorded id and the checkout root it resolved
/// from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `git:<root sha>` or `path:<canonical path>`.
    pub id: String,
    /// The checkout root (`git rev-parse --show-toplevel`), or the canonical
    /// path outside git.
    pub root: PathBuf,
}

/// One project in `projects/index.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    /// The first checkout root's directory name; renamable later.
    pub name: String,
    /// Every checkout seen for this project.
    pub checkouts: Vec<PathBuf>,
    /// The default agent command.
    pub argv: Vec<String>,
    /// When the project was last opened, in Unix milliseconds.
    pub opened_ms: u64,
    /// A forgotten project stays in the file but drops out of `list`.
    pub hidden: bool,
}

/// The project index under one data root.
pub struct Projects {
    data: PathBuf,
    projects: Vec<Project>,
}

impl Projects {
    /// Read `projects/index.json`. A missing file is built from the task
    /// records, then written, so an existing data root gains the index once.
    pub fn open(data: &Path) -> Result<Self> {
        let path = index_file(data);
        let projects = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let projects = backfill(data)?;
                crate::approvals::store::write_json(&path, &projects)?;
                projects
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { data: data.to_path_buf(), projects })
    }

    /// The projects a window may show, most recently opened first.
    pub fn list(&self) -> Vec<Project> {
        let mut out: Vec<_> = self.projects.iter().filter(|project| !project.hidden).cloned().collect();
        out.sort_by(|a, b| b.opened_ms.cmp(&a.opened_ms).then_with(|| a.id.cmp(&b.id)));
        out
    }

    /// Resolve `path` into its project, adding the checkout if new. The
    /// command is recorded only when the project is new; opening an existing
    /// project keeps its default.
    pub fn open_project(&mut self, path: &Path, argv: Vec<String>) -> Result<Project> {
        let identity = identify(path)?;
        let index =
            match self.projects.iter().position(|project| project.id == identity.id || matches(&project.id, path)) {
                Some(index) => index,
                None => {
                    self.projects.push(Project {
                        id: identity.id.clone(),
                        name: name_of(&identity.root),
                        checkouts: vec![],
                        argv,
                        opened_ms: now_ms(),
                        hidden: false,
                    });
                    self.projects.len() - 1
                }
            };
        let project = &mut self.projects[index];
        if !project.checkouts.iter().any(|checkout| checkout == &identity.root) {
            project.checkouts.push(identity.root);
        }
        project.opened_ms = now_ms();
        project.hidden = false;
        let project = project.clone();
        self.save()?;
        Ok(project)
    }

    /// Hide a project; its tasks stay.
    pub fn forget(&mut self, id: &str) -> Result<()> {
        let project =
            self.projects.iter_mut().find(|project| project.id == id).with_context(|| format!("no project {id}"))?;
        project.hidden = true;
        self.save()
    }

    fn save(&self) -> Result<()> {
        crate::approvals::store::write_json(&index_file(&self.data), &self.projects)
    }
}

/// Resolve `path` to its project. A shallow clone fails: it has no true root.
pub fn identify(path: &Path) -> Result<Identity> {
    if is_shallow(path) {
        bail!(
            "{} is a shallow clone, so its root commit is unknown; run git fetch --unshallow and open it again",
            path.display()
        );
    }
    match rev_roots(path) {
        Some(mut roots) if !roots.is_empty() => {
            roots.sort();
            let root = crate::github::git(path, &["rev-parse", "--show-toplevel"])
                .map(PathBuf::from)
                .unwrap_or_else(|| path.to_path_buf());
            let root = fs::canonicalize(&root).unwrap_or(root);
            Ok(Identity { id: format!("git:{}", roots[0]), root })
        }
        _ => {
            let root = fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
            Ok(Identity { id: format!("path:{}", root.display()), root })
        }
    }
}

/// Whether a recorded project id matches any root of the checkout at `path`.
/// This is how a subtree merge that adds a root keeps the project.
pub fn matches(recorded: &str, path: &Path) -> bool {
    let Some(id) = recorded.strip_prefix("git:") else {
        return false;
    };
    roots(path).is_some_and(|roots| roots.iter().any(|root| root == id))
}

fn roots(path: &Path) -> Option<Vec<String>> {
    if is_shallow(path) {
        return None;
    }
    rev_roots(path)
}

fn rev_roots(path: &Path) -> Option<Vec<String>> {
    let text = crate::github::git(path, &["rev-list", "--max-parents=0", "HEAD"])?;
    Some(text.lines().map(str::trim).filter(|line| !line.is_empty()).map(str::to_owned).collect())
}

fn is_shallow(path: &Path) -> bool {
    crate::github::git(path, &["rev-parse", "--is-shallow-repository"]).as_deref() == Some("true")
}

fn backfill(data: &Path) -> Result<Vec<Project>> {
    let store = crate::record::Store::open(data)?;
    let mut tasks: Vec<_> = store
        .list()?
        .into_iter()
        .map(|task| {
            let at = store.created_ms(&task.id).unwrap_or(0);
            (at, task)
        })
        .collect();
    // Oldest first, so a project's name and checkout order come from its
    // first task; the newest task supplies its command and open time.
    tasks.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    let mut order = vec![];
    let mut groups: BTreeMap<String, Project> = BTreeMap::new();
    for (at, task) in tasks {
        let Ok(identity) = identify(&task.spec.cwd) else {
            continue;
        };
        let id = identity.id.clone();
        let project = groups.entry(id.clone()).or_insert_with(|| {
            order.push(id);
            Project {
                id: identity.id,
                name: name_of(&identity.root),
                checkouts: vec![],
                argv: task.spec.argv.clone(),
                opened_ms: at,
                hidden: false,
            }
        });
        if !project.checkouts.iter().any(|checkout| checkout == &identity.root) {
            project.checkouts.push(identity.root);
        }
        if at >= project.opened_ms {
            project.opened_ms = at;
            project.argv = task.spec.argv.clone();
        }
    }
    Ok(order.into_iter().filter_map(|id| groups.remove(&id)).collect())
}

fn index_file(data: &Path) -> PathBuf {
    data.join(INDEX)
}

fn name_of(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| root.display().to_string())
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::Spec;
    use crate::record::Store;
    use std::process::Command;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deck-project-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn repo(name: &str) -> PathBuf {
        let dir = scratch(name);
        git(&dir, &["init", "--quiet", "-b", "main"]);
        git(&dir, &["config", "user.name", "Ada Lovelace"]);
        git(&dir, &["config", "user.email", "ada@example.com"]);
        fs::write(dir.join("README.md"), format!("{name}\n")).unwrap();
        git(&dir, &["add", "README.md"]);
        git(&dir, &["commit", "--quiet", "-m", name]);
        dir
    }

    fn root_commits(dir: &Path) -> Vec<String> {
        let output =
            Command::new("git").arg("-C").arg(dir).args(["rev-list", "--max-parents=0", "HEAD"]).output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().lines().map(str::to_owned).collect()
    }

    #[test]
    fn a_checkout_is_its_root_commit_and_a_worktree_agrees() {
        let repo = repo("root");
        let roots = root_commits(&repo);
        assert_eq!(identify(&repo).unwrap().id, format!("git:{}", roots[0]));

        let worktree = scratch("worktree");
        fs::remove_dir_all(&worktree).unwrap();
        git(&repo, &["worktree", "add", worktree.to_str().unwrap(), "-b", "topic"]);
        assert_eq!(identify(&worktree).unwrap().id, identify(&repo).unwrap().id);
    }

    #[test]
    fn a_second_root_keeps_the_recorded_id_and_matches_either_root() {
        let repo = repo("two-roots");
        git(&repo, &["checkout", "--quiet", "--orphan", "other"]);
        git(&repo, &["rm", "-rf", "."]);
        fs::write(repo.join("other.txt"), "two\n").unwrap();
        git(&repo, &["add", "other.txt"]);
        git(&repo, &["commit", "--quiet", "-m", "two"]);
        git(&repo, &["checkout", "--quiet", "main"]);
        git(&repo, &["merge", "--quiet", "--allow-unrelated-histories", "--no-edit", "other"]);

        let roots = root_commits(&repo);
        assert_eq!(roots.len(), 2);
        let smallest = roots.iter().min().unwrap();
        assert_eq!(identify(&repo).unwrap().id, format!("git:{smallest}"));
        for root in roots {
            assert!(matches(&format!("git:{root}"), &repo));
        }
    }

    #[test]
    fn a_shallow_clone_has_no_known_root() {
        let repo = repo("shallow-source");
        let parent = scratch("shallow-parent");
        let clone = parent.join("clone");
        git(&parent, &["clone", "--quiet", "--depth", "1", &format!("file://{}", repo.display()), "clone"]);
        let error = identify(&clone).unwrap_err().to_string();
        assert_eq!(
            error,
            format!(
                "{} is a shallow clone, so its root commit is unknown; run git fetch --unshallow and open it again",
                clone.display()
            )
        );
    }

    #[test]
    fn an_empty_checkout_and_a_plain_directory_use_their_canonical_path() {
        let empty = scratch("empty");
        git(&empty, &["init", "--quiet"]);
        assert_eq!(identify(&empty).unwrap().id, format!("path:{}", fs::canonicalize(&empty).unwrap().display()));

        let plain = scratch("plain");
        assert_eq!(identify(&plain).unwrap().id, format!("path:{}", fs::canonicalize(&plain).unwrap().display()));
    }

    fn spec(cwd: &Path, argv: &[&str]) -> Spec {
        Spec {
            argv: argv.iter().map(|arg| (*arg).into()).collect(),
            cwd: cwd.to_path_buf(),
            github: None,
            project: None,
        }
    }

    #[test]
    fn opening_a_checkout_twice_keeps_one_project_and_one_checkout() {
        let data = scratch("index-twice");
        let repo = repo("index-twice-repo");
        let mut projects = Projects::open(&data).unwrap();
        let first = projects.open_project(&repo, vec!["construct".into()]).unwrap();
        let second = projects.open_project(&repo, vec!["other".into()]).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.checkouts.len(), 1);
        assert_eq!(second.argv, vec!["construct".to_string()], "an existing project keeps its command");
    }

    #[test]
    fn a_worktree_adds_a_checkout_to_the_same_project() {
        let data = scratch("index-worktree");
        let repo = repo("index-worktree-repo");
        let worktree = scratch("index-worktree-checkout");
        fs::remove_dir_all(&worktree).unwrap();
        git(&repo, &["worktree", "add", worktree.to_str().unwrap(), "-b", "topic"]);
        let mut projects = Projects::open(&data).unwrap();
        let first = projects.open_project(&repo, vec![]).unwrap();
        let second = projects.open_project(&worktree, vec![]).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.checkouts.len(), 2);
    }

    #[test]
    fn forget_hides_and_open_unhides() {
        let data = scratch("index-forget");
        let repo = repo("index-forget-repo");
        let mut projects = Projects::open(&data).unwrap();
        let project = projects.open_project(&repo, vec![]).unwrap();
        projects.forget(&project.id).unwrap();
        assert!(projects.list().is_empty());
        projects.open_project(&repo, vec![]).unwrap();
        assert_eq!(projects.list().len(), 1);
    }

    #[test]
    fn the_index_round_trips() {
        let data = scratch("index-round-trip");
        let repo = repo("index-round-trip-repo");
        let mut projects = Projects::open(&data).unwrap();
        let project = projects.open_project(&repo, vec!["construct".into()]).unwrap();
        drop(projects);
        let reopened = Projects::open(&data).unwrap();
        assert_eq!(reopened.list(), vec![project]);
    }

    #[test]
    fn a_missing_index_is_backfilled_from_task_records() {
        let data = scratch("index-backfill");
        let repo_a = repo("index-backfill-a");
        let repo_b = repo("index-backfill-b");
        let worktree = scratch("index-backfill-worktree");
        fs::remove_dir_all(&worktree).unwrap();
        git(&repo_a, &["worktree", "add", worktree.to_str().unwrap(), "-b", "topic"]);
        let store = Store::open(&data).unwrap();
        for (cwd, argv) in [(&repo_a, "a"), (&repo_b, "b"), (&worktree, "worktree")] {
            store.create("local", spec(cwd, &[argv])).unwrap();
        }
        let projects = Projects::open(&data).unwrap();
        let list = projects.list();
        assert_eq!(list.len(), 2);
        let a = list.iter().find(|project| project.id == identify(&repo_a).unwrap().id).unwrap();
        assert_eq!(a.checkouts.len(), 2);
        assert!(a.checkouts.contains(&fs::canonicalize(&repo_a).unwrap()));
        assert!(a.checkouts.contains(&fs::canonicalize(&worktree).unwrap()));
        assert!(!a.name.is_empty());
    }
}
