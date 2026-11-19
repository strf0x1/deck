//! A local worker owns a registered Git worktree and task branch. The intent
//! pins the source repository before Git touches the destination.

use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
mod git;
mod removal;
mod review;

use self::git::{Shadow, branch, command, commit, common_dir, git_dir, path_output, reference_exists, write_private};
pub use review::{SourcePreview, WorktreeDiff, WorktreeState, WorktreeStatus};

pub(crate) const CHECKOUT: &str = "worktree";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub source: PathBuf,
    pub(crate) repository: Repository,
    pub start: String,
    pub target: Option<String>,
    #[serde(default)]
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    removal: Option<Removal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Removal {
    Pending,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Repository {
    common: PathBuf,
    device: u64,
    inode: u64,
}

impl Workspace {
    pub(crate) fn capture_project(path: &Path, project: &str) -> Result<Option<Self>> {
        let checkout =
            Self::capture(path).with_context(|| format!("Could not read the checkout at {}.", path.display()))?;
        if project.starts_with("git:") {
            let checkout =
                checkout.as_ref().with_context(|| format!("The Git checkout at {} is unavailable.", path.display()))?;
            ensure!(
                crate::project::matches(project, &checkout.source),
                "The checkout at {} belongs to another project.",
                path.display()
            );
        }
        Ok(checkout)
    }

    pub(crate) fn capture(path: &Path) -> Result<Option<Self>> {
        let probe = command(path).args(["rev-parse", "--show-toplevel"]).output()?;
        if !probe.status.success() {
            return Ok(None);
        }
        let source = path_output(probe)?.canonicalize()?;
        let common = common_dir(&source)?;
        let metadata = fs::metadata(&common)?;
        let start = commit(&source, "HEAD").context("Commit the first project files before starting a worker.")?;
        let target = branch(&source)?;
        let repository = Repository { common, device: metadata.dev(), inode: metadata.ino() };
        Ok(Some(Self { source, repository, start, target, ready: false, removal: None }))
    }

    pub(crate) fn target_branch(&mut self, branch: &str) -> Result<()> {
        let reference = format!("refs/heads/{branch}");
        self.start =
            self.branch_commit(&reference)?.with_context(|| format!("Local branch {branch} is unavailable."))?;
        self.target = Some(reference);
        Ok(())
    }

    pub(crate) fn branch_commit(&self, reference: &str) -> Result<Option<String>> {
        self.verify_source()?;
        reference_exists(&self.source, reference)?.then(|| commit(&self.source, reference)).transpose()
    }

    pub(crate) fn has_commit(&self, oid: &str) -> Result<bool> {
        self.verify_source()?;
        ensure!(crate::integration::is_commit(oid), "Invalid Git commit ID.");
        let output = command(&self.source).args(["cat-file", "-t", oid]).output()?;
        Ok(output.status.success() && output.stdout == b"commit\n")
    }

    pub(crate) fn review_commit(&self, task: &crate::Task) -> Result<Option<String>> {
        if !self.ready || task.integration.as_ref().is_some_and(|plan| plan.phase != crate::integration::Phase::Ready) {
            return Ok(None);
        }
        let reference = format!("refs/heads/{}", crate::github::task_branch(&task.id));
        let Some(tip) = self.branch_commit(&reference)?.filter(|tip| tip != &self.start) else {
            return Ok(None);
        };
        let mut pins = vec![self.start.as_str()];
        if let Some(plan) = &task.integration {
            for input in &plan.inputs {
                pins.push(input.commit.as_deref().context("Integration has an unpinned input")?);
            }
        }
        for pin in pins {
            ensure!(crate::integration::is_commit(pin), "Invalid pinned Git commit.");
            match command(&self.source).args(["merge-base", "--is-ancestor", pin, &tip]).status()?.code() {
                Some(0) => {}
                Some(1) => return Ok(None),
                _ => bail!("Git could not verify the ancestry of the review branch."),
            }
        }
        Ok(Some(tip))
    }

    pub(crate) fn prepare(&self, path: &Path, id: &str) -> Result<()> {
        ensure!(!self.removed(), "This worktree was removed. Its task branch remains in the source repository.");
        let branch = format!("refs/heads/{}", crate::github::task_branch(id));
        if self.ready {
            self.verify(path, &branch)?;
            return Ok(());
        }
        self.verify_source()?;
        let parent = path.parent().context("task worktree has no parent directory")?;
        let scratch = Shadow::prepare(parent.join("worktree-setup"), &self.source, &self.repository.common)?;
        if !path.exists() {
            ensure!(
                path.symlink_metadata().is_err() && !reference_exists(&self.source, &branch)?,
                "The task worktree path or branch is already in use."
            );
            let branch = branch.strip_prefix("refs/heads/").context("invalid task branch")?;
            let source_git_dir = git_dir(&self.source)?;
            scratch.git(
                &self.source,
                &source_git_dir,
                &[
                    "worktree",
                    "add",
                    "--no-checkout",
                    "--quiet",
                    "--no-track",
                    "-b",
                    branch,
                    "--",
                    path.to_str().context("task worktree path is not UTF-8")?,
                    &self.start,
                ],
            )?;
        }
        let native = self.verify(path, &branch)?;
        ensure!(
            commit(path, "HEAD")? == self.start,
            "The incomplete task worktree has moved from its starting commit."
        );

        // Older Git writes the shadow worktrees path in .git. Store the
        // registered native directory before removing the temporary config.
        write_private(&scratch.0.join("gitdir"), &format!("gitdir: {}\n", native.display()))?;
        fs::rename(scratch.0.join("gitdir"), path.join(".git"))?;
        if native.join("index").is_file() {
            ensure!(
                scratch
                    .git(path, &native, &["status", "--porcelain=v1", "--untracked-files=all", "--ignored=matching"])?
                    .is_empty(),
                "The incomplete task worktree contains changes. Deck preserved them."
            );
        } else {
            ensure!(
                fs::read_dir(path)?.all(|entry| entry.is_ok_and(|entry| entry.file_name() == ".git")),
                "The incomplete task worktree contains files. Deck preserved them."
            );
            scratch.git(path, &native, &["read-tree", "--reset", "-u", &self.start])?;
        }
        fs::remove_dir_all(&scratch.0)?;
        Ok(())
    }

    pub(crate) fn record_removal(&mut self, complete: bool) -> Result<()> {
        ensure!(self.ready, "the worktree was never prepared");
        self.removal = match (self.removal, complete) {
            (None, false) => Some(Removal::Pending),
            (Some(Removal::Pending), true) => Some(Removal::Removed),
            _ => bail!("invalid worktree removal transition"),
        };
        Ok(())
    }

    pub(crate) fn removal_pending(&self) -> bool {
        self.removal == Some(Removal::Pending)
    }

    fn verify_source(&self) -> Result<()> {
        self.repository.verify()?;
        ensure!(
            common_dir(&self.source)? == self.repository.common,
            "The source checkout no longer points to its recorded Git repository."
        );
        Ok(())
    }

    fn verify(&self, path: &Path, expected: &str) -> Result<PathBuf> {
        self.repository.verify()?;
        ensure!(
            path.canonicalize().context("The recorded task worktree is missing.")? == path,
            "The task worktree moved."
        );
        let dotgit = path.join(".git");
        let target = pointer(&dotgit)?;
        let target = target.strip_prefix("gitdir: ").context("The worktree has an invalid Git pointer.")?;
        let admin = path.join(target).canonicalize()?;
        let common = &self.repository.common;
        ensure!(admin.parent() == Some(common.join("worktrees").as_path()), "The worktree's Git directory changed.");
        let back = pointer(&admin.join("gitdir"))?;
        ensure!(
            admin.join(back).canonicalize()? == dotgit
                && common_dir(path)? == *common
                && branch(path)?.as_deref() == Some(expected),
            "The task worktree no longer matches its recorded repository and branch."
        );
        Ok(admin)
    }
}

impl Repository {
    fn verify(&self) -> Result<()> {
        let metadata = fs::metadata(&self.common).context("The recorded source Git directory is unavailable.")?;
        ensure!(
            self.common.canonicalize()? == self.common && metadata.dev() == self.device && metadata.ino() == self.inode,
            "The recorded source Git directory changed."
        );
        Ok(())
    }
}

fn pointer(path: &Path) -> Result<String> {
    let file = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    ensure!(file.metadata()?.is_file(), "Git worktree pointer is not a regular file.");
    let mut value = String::new();
    file.take(8193).read_to_string(&mut value)?;
    let trimmed = value.trim_end_matches(['\n', '\r']);
    ensure!(
        !trimmed.is_empty() && value.len() <= 8192 && !trimmed.contains(['\0', '\n', '\r']),
        "Invalid Git worktree pointer."
    );
    Ok(trimmed.into())
}
