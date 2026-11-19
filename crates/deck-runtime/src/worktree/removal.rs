//! Explicit removal moves the verified checkout away from its public path.
//! A second check protects files that appeared before the move. Git then
//! removes the private checkout without force, keeping the task branch.

use super::Workspace;
use super::git::{bounded, commit, git_dir};
use super::review::{Inspection, task_ref};
use crate::{State, Task};
use anyhow::{Context as _, Result, bail, ensure};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

impl Workspace {
    pub(crate) fn contains_path(&self, task: &Task, path: &Path) -> bool {
        path.starts_with(&task.spec.cwd) || (self.removal_pending() && path.starts_with(quarantine(task)))
    }

    pub(crate) fn check_removal(&self, task: &Task) -> Result<()> {
        worker_exited(task)?;
        ensure!(self.ready, "The worktree has never been prepared.");
        self.verify_source()?;
        let original = &task.spec.cwd;
        let held = quarantine(task);
        match (exists(original)?, exists(&held)?) {
            (true, false) => self.clean(task, original),
            (false, true) if self.removal_pending() => self.clean(task, &held),
            (false, false) if self.removal_pending() => self.check_registration_gone(task, &held),
            (false, false) => bail!("The recorded worktree is missing."),
            _ => bail!("The worktree removal path {} is occupied. Deck preserved both paths.", held.display()),
        }
    }

    pub(crate) fn remove(&self, task: &Task) -> Result<()> {
        self.check_removal(task)?;
        let original = &task.spec.cwd;
        let held = quarantine(task);
        if exists(original)? {
            self.move_checkout(original, &held)?;
        }
        if exists(&held)? {
            if let Err(error) = self.clean(task, &held) {
                if !exists(original)? && self.move_checkout(&held, original).is_ok() {
                    return Err(error);
                }
                return Err(error.context(format!("Deck preserved the checkout at {}.", held.display())));
            }
            self.source_command(&["worktree", "remove", "--", git_path(&held)?])?;
        }
        ensure!(!exists(&held)?, "Git did not remove the verified worktree.");
        self.check_registration_gone(task, &held)
    }

    fn clean(&self, task: &Task, path: &Path) -> Result<()> {
        let inspection = Inspection::task(self, task, path)?;
        inspection.removal_idle()?;
        inspection.changes()?.removal_safe()?;
        inspection.visible_index()?;
        self.retained_tip(task, &commit(path, "HEAD")?)
    }

    fn move_checkout(&self, from: &Path, to: &Path) -> Result<()> {
        ensure!(!exists(to)?, "The worktree path {} is occupied.", to.display());
        self.source_command(&["worktree", "move", "--", git_path(from)?, git_path(to)?])?;
        Ok(())
    }

    fn source_command(&self, args: &[&str]) -> Result<String> {
        self.verify_source()?;
        let inspection = self.source_inspection()?;
        inspection.shadow.git(&self.source, &inspection.git_dir, args)
    }

    fn check_registration_gone(&self, task: &Task, held: &Path) -> Result<()> {
        let inspection = self.source_inspection()?;
        let (fields, capped) =
            bounded(inspection.command().args(["worktree", "list", "--porcelain", "-z"]), 128 * 1024)?;
        let mut registered = fields.split(|byte| *byte == 0).filter_map(|field| field.strip_prefix(b"worktree "));
        let original = task.spec.cwd.as_os_str().as_bytes();
        let held = held.as_os_str().as_bytes();
        ensure!(
            !capped && !registered.any(|path| path == original || path == held),
            "The absent worktree still has a Git registration. Its files may have been moved."
        );
        let tip = self.branch_commit(&task_ref(task))?.context("The task branch is unavailable.")?;
        ensure!(self.is_ancestor(&self.start, &tip)?, "The task branch no longer contains its starting commit.");
        Ok(())
    }

    pub(super) fn retained_tip(&self, task: &Task, checkout: &str) -> Result<()> {
        let expected = self.branch_commit(&task_ref(task))?.context("The task branch is unavailable.")?;
        ensure!(expected == checkout, "The task branch changed while the worktree was being inspected.");
        ensure!(self.is_ancestor(&self.start, checkout)?, "The task branch no longer contains its starting commit.");
        Ok(())
    }

    fn source_inspection(&self) -> Result<Inspection> {
        Inspection::new(self, &self.source, git_dir(&self.source)?)
    }
}

pub(super) fn quarantine(task: &Task) -> PathBuf {
    task.spec.cwd.with_file_name(".worktree-removing")
}

pub(super) fn exists(path: &Path) -> Result<bool> {
    match path.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn worker_exited(task: &Task) -> Result<()> {
    ensure!(
        !matches!(
            task.state,
            State::Running | State::Failed { failure: crate::lifecycle::Failure::ShutdownUnconfirmed(_) }
        ),
        "Stop the worker before removing its worktree."
    );
    Ok(())
}

fn git_path(path: &Path) -> Result<&str> {
    path.to_str().context("The task worktree path is not UTF-8.")
}
