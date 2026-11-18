//! `deckd`'s coordinator operations for windows (ADR 0011 §2, §4): a
//! coordinator's command comes from its project, a run selects the scope's
//! coordinator, and `coordinator_policy` reads or replaces the policy.
//!
//! The design choice: these are window operations only. The control surface
//! a coordinator reaches (deck#152) never routes here, so a coordinator can
//! read its caps through its own refusals but never raise them.

use super::Service;
use crate::coordinator::{self, Settings};
use crate::wire::RunResult;
use crate::{Spec, State, TaskKind};
use anyhow::{Context as _, Result, bail};

impl Service {
    /// Fill in what a window may not choose for a coordinator: its project,
    /// when left out, and its command, which is the project's coordinator
    /// command (`construct acp` by default), never the window's argv.
    pub(super) fn coordinate(&self, spec: &mut Spec) -> Result<()> {
        if spec.kind != TaskKind::Coordinator {
            return Ok(());
        }
        let project = match &spec.project {
            Some(project) => project.clone(),
            None => crate::project::identify(&spec.cwd)?.id,
        };
        spec.argv = self.projects.coordinator_argv(&project);
        spec.project = Some(project);
        Ok(())
    }

    /// Run the scope's coordinator: the running one as it is, a stopped or
    /// failed one started again, or a new one when there is none or the last
    /// was destroyed (ADR 0011 §2: one per scope).
    pub(super) fn run_coordinator(&mut self, mut spec: Spec) -> Result<RunResult> {
        self.coordinate(&mut spec)?;
        let scope: Vec<String> = spec.project.iter().cloned().collect();
        let id = match self.runtime.coordinator(&scope)? {
            Some(task) if task.state == State::Running => return Ok(RunResult { task, started: false }),
            Some(task) => task.id,
            None => self.runtime.create(spec)?,
        };
        let resume = self.runtime.task(&id)?.resumable();
        self.launch(&id, resume)?;
        Ok(RunResult { task: self.runtime.task(&id)?, started: true })
    }

    /// Read a coordinator's policy and its project's coordinator command, or
    /// replace both for the user. Everything is checked before anything is
    /// written, so a refused edit leaves both files as they were.
    pub(super) fn coordinator_policy(&mut self, id: &str, set: Option<Settings>) -> Result<Settings> {
        let task = self.runtime.task(id)?;
        if task.spec.kind != TaskKind::Coordinator {
            bail!("task {id} is not a coordinator");
        }
        let project = task.spec.project.clone().context("the coordinator has no project")?;
        if let Some(set) = set {
            coordinator::validate_argv(&set.coordinator_argv)?;
            let mut policy = set.policy;
            policy.validate()?;
            let known = self.projects.get(&project).is_some();
            let current = self.projects.get(&project).and_then(|p| p.coordinator_argv.clone());
            let changed = current != set.coordinator_argv;
            if changed && !known {
                bail!("project {project} is not open, so its coordinator command cannot be set");
            }
            self.runtime.set_policy(id, policy)?;
            if changed {
                self.projects.set_coordinator_argv(&project, set.coordinator_argv)?;
            }
        }
        Ok(Settings {
            policy: self.runtime.policy(id)?,
            coordinator_argv: self.projects.get(&project).and_then(|p| p.coordinator_argv.clone()),
        })
    }
}
