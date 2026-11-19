//! Facts for the window come from the registered checkout and its Git refs.
//! The removal preflight uses the same inspection, including ignored files
//! and index entries whose changes Git would normally hide.

use super::git::{Shadow, bounded, command, commit, git_dir};
use super::removal::{exists, quarantine, worker_exited};
use super::{Removal, Workspace};
use crate::{State, Task};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePreview {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub commit: String,
    pub dirty: bool,
}

impl SourcePreview {
    pub(crate) fn read(path: &Path, project: &str) -> Result<Option<Self>> {
        let Some(workspace) = Workspace::capture_project(path, project)? else { return Ok(None) };
        let git_dir = git_dir(&workspace.source)?;
        let inspection = Inspection::new(&workspace, &workspace.source, git_dir)?;
        let changes = inspection.changes()?;
        Ok(Some(Self {
            path: workspace.source,
            branch: workspace.target.and_then(|target| target.strip_prefix("refs/heads/").map(str::to_owned)),
            commit: workspace.start,
            dirty: changes.dirty,
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeState {
    Preparing,
    InProgress,
    Uncommitted,
    Ready,
    Merged,
    Unchanged,
    Unavailable,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeStatus {
    pub path: PathBuf,
    pub branch: String,
    pub start: String,
    pub target: Option<String>,
    pub tip: Option<String>,
    pub state: WorktreeState,
    pub merged: bool,
    pub removable: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeDiff {
    pub text: String,
    pub truncated: bool,
}

impl Workspace {
    pub(crate) fn status(&self, task: &Task) -> WorktreeStatus {
        let mut result = WorktreeStatus {
            path: task.spec.cwd.clone(),
            branch: crate::github::task_branch(&task.id),
            start: self.start.clone(),
            target: self.target.clone(),
            tip: None,
            state: WorktreeState::Preparing,
            merged: false,
            removable: false,
            reason: None,
        };
        if self.removal == Some(Removal::Removed) {
            result.state = WorktreeState::Removed;
            result.tip = self.branch_commit(&format!("refs/heads/{}", result.branch)).ok().flatten();
            result.merged = result.tip.as_ref().is_some_and(|tip| tip != &self.start && self.is_merged(tip));
            return result;
        }
        if !self.ready {
            return result;
        }
        if let Err(error) = self.fill_status(task, &mut result) {
            result.state = WorktreeState::Unavailable;
            result.reason = Some(error.to_string());
        }
        result
    }

    fn fill_status(&self, task: &Task, result: &mut WorktreeStatus) -> Result<()> {
        if self.removal_pending() && !exists(&task.spec.cwd)? {
            let held = quarantine(task);
            if exists(&held)? {
                result.path = held;
            }
            self.check_removal(task)?;
            result.removable = true;
            result.state = WorktreeState::Unavailable;
            result.reason = Some("Finish recording the removal by choosing Remove worktree again.".into());
            return Ok(());
        }
        let inspection = Inspection::task(self, task, &task.spec.cwd)?;
        let changes = inspection.changes()?;
        let tip = commit(&task.spec.cwd, "HEAD")?;
        let changed = tip != self.start;
        result.merged = !still_working(task) && !changes.dirty && changed && self.is_merged(&tip);
        result.state = if changes.dirty {
            WorktreeState::Uncommitted
        } else if still_working(task) {
            WorktreeState::InProgress
        } else if result.merged {
            WorktreeState::Merged
        } else if changed {
            WorktreeState::Ready
        } else {
            WorktreeState::Unchanged
        };
        let removal = worker_exited(task)
            .and_then(|()| {
                ensure!(!exists(&quarantine(task))?, "The worktree removal path is occupied.");
                inspection.removal_idle()
            })
            .and_then(|()| changes.removal_safe())
            .and_then(|()| inspection.visible_index())
            .and_then(|()| self.retained_tip(task, &tip));
        result.removable = removal.is_ok();
        result.reason = removal.err().map(|error| error.to_string());
        result.tip = Some(tip);
        Ok(())
    }

    fn is_merged(&self, tip: &str) -> bool {
        let target = self.target.as_ref().and_then(|reference| self.branch_commit(reference).ok().flatten());
        target.is_some_and(|target| self.is_ancestor(tip, &target).unwrap_or(false))
    }

    pub(super) fn is_ancestor(&self, old: &str, new: &str) -> Result<bool> {
        ensure!(crate::integration::is_commit(old) && crate::integration::is_commit(new), "Invalid Git commit.");
        let status = command(&self.source).args(["merge-base", "--is-ancestor", old, new]).status()?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => anyhow::bail!("Git could not verify the branch history."),
        }
    }

    pub(crate) fn diff(&self, task: &Task) -> Result<WorktreeDiff> {
        ensure!(self.ready, "The worktree is still preparing.");
        self.verify_source()?;
        ensure!(crate::integration::is_commit(&self.start), "Invalid starting commit.");
        let args = ["diff", "--no-ext-diff", "--no-textconv", "--no-color", "--ignore-submodules=none", &self.start];
        let (bytes, truncated) = if self.removal == Some(Removal::Removed) {
            let reference = task_ref(task);
            let tip = self.branch_commit(&reference)?.context("The task branch is unavailable.")?;
            bounded(command(&self.source).args(args).args([&tip, "--"]), OUTPUT_LIMIT)?
        } else {
            let inspection = Inspection::task(self, task, &task.spec.cwd)?;
            bounded(inspection.command().args(args).arg("--"), OUTPUT_LIMIT)?
        };
        Ok(WorktreeDiff { text: String::from_utf8_lossy(&bytes).into_owned(), truncated })
    }

    pub(crate) fn removed(&self) -> bool {
        self.removal == Some(Removal::Removed)
    }
}

pub(super) fn task_ref(task: &Task) -> String {
    format!("refs/heads/{}", crate::github::task_branch(&task.id))
}

fn still_working(task: &Task) -> bool {
    task.state == State::Running && task.turn.as_ref().is_none_or(|turn| turn.state != crate::Phase::Idle)
}

pub(super) struct Changes {
    dirty: bool,
    ignored: bool,
}

impl Changes {
    pub(super) fn removal_safe(self) -> Result<()> {
        ensure!(!self.dirty, "Commit or save the uncommitted files before removing this worktree.");
        ensure!(!self.ignored, "Save or clear the ignored files before removing this worktree.");
        Ok(())
    }
}

pub(super) struct Inspection {
    pub(super) shadow: Shadow,
    _scratch: Scratch,
    work: PathBuf,
    pub(super) git_dir: PathBuf,
}

impl Inspection {
    pub(super) fn task(workspace: &Workspace, task: &Task, path: &Path) -> Result<Self> {
        Self::new(workspace, path, workspace.verify(path, &task_ref(task))?)
    }

    pub(super) fn new(workspace: &Workspace, work: &Path, git_dir: PathBuf) -> Result<Self> {
        let scratch = Scratch::create()?;
        let shadow = Shadow::inspect(scratch.0.clone(), work, &workspace.repository.common)?;
        Ok(Self { shadow, _scratch: scratch, work: work.to_path_buf(), git_dir })
    }

    pub(super) fn command(&self) -> std::process::Command {
        self.shadow.command(&self.work, &self.git_dir)
    }

    pub(super) fn changes(&self) -> Result<Changes> {
        let (bytes, truncated) = bounded(
            self.command().args([
                "status",
                "--porcelain=v1",
                "-z",
                "--no-renames",
                "--ignore-submodules=none",
                "--untracked-files=all",
                "--ignored=matching",
            ]),
            OUTPUT_LIMIT,
        )?;
        let mut changes = Changes { dirty: truncated, ignored: false };
        for entry in bytes.split(|byte| *byte == 0).filter(|entry| !entry.is_empty()) {
            if entry.starts_with(b"!! ") {
                changes.ignored = true;
            } else {
                changes.dirty = true;
            }
        }
        Ok(changes)
    }

    pub(super) fn visible_index(&self) -> Result<()> {
        let (bytes, truncated) = bounded(self.command().args(["ls-files", "-v", "-z"]), 4 * 1024 * 1024)?;
        ensure!(
            !truncated
                && !bytes
                    .split(|byte| *byte == 0)
                    .any(|entry| { entry.first().is_some_and(|tag| tag.is_ascii_lowercase() || *tag == b'S') }),
            "Git hides some indexed files. Review the worktree before removing it."
        );
        Ok(())
    }

    pub(super) fn removal_idle(&self) -> Result<()> {
        ensure!(!exists(&self.git_dir.join("locked"))?, "The worktree is locked by Git.");
        for marker in [
            "index.lock",
            "HEAD.lock",
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "rebase-apply",
            "rebase-merge",
            "sequencer",
            "BISECT_START",
        ] {
            ensure!(!exists(&self.git_dir.join(marker))?, "Finish the Git operation before removing this worktree.");
        }
        Ok(())
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn create() -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let next = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("deck-worktree-view-{}-{next}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
