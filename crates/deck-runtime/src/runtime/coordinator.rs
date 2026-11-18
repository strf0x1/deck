//! The runtime's coordinator operations (ADR 0011 §2, §4): what `create`
//! checks and records for a coordinator or a worker, the one coordinator per
//! scope, the user's policy edits, and an agent that refuses its session.
//!
//! The design choice: a coordinator's scope is its policy's project set, and
//! a coordinator whose policy file is missing falls back to its recorded
//! project, so a half-written create can never let a second coordinator into
//! the same scope. Destroyed coordinators do not count.

use super::{Inner, Runtime};
use crate::coordinator::{self, Policy};
use crate::lifecycle::{Event, Failure, Spec, State, Task, TaskKind};
use anyhow::{Context as _, Result, bail};

/// Who may change a coordinator's policy. Only the user, through a window.
pub const BY_USER: &str = "user";
/// The most of an agent's refusal a failed start records.
const REASON_CHARS: usize = 4096;

impl Inner {
    /// Check a spec's kind and provenance before it is recorded, and return
    /// the policy a coordinator starts with.
    pub(super) fn admit(&self, spec: &mut Spec) -> Result<Option<Policy>> {
        if let Some(creator) = &spec.created_by {
            if spec.kind == TaskKind::Coordinator {
                bail!("only the user creates a coordinator");
            }
            let creator = self.store.load(creator).with_context(|| format!("no coordinator {creator}"))?;
            if creator.spec.kind != TaskKind::Coordinator {
                bail!("task {} is not a coordinator", creator.id);
            }
        }
        if spec.kind != TaskKind::Coordinator {
            return Ok(None);
        }
        let project = spec.project.clone().context("a coordinator needs a project")?;
        let policy = Policy::new(vec![project])?;
        if let Some(existing) = self.coordinator(&policy.projects)? {
            bail!("project scope already has coordinator {}", existing.id);
        }
        Ok(Some(policy))
    }

    /// The coordinator of exactly this scope, on any backend, unless it was
    /// destroyed: one per scope (ADR 0011 §2), wherever it runs (ADR 0016).
    pub(super) fn coordinator(&self, projects: &[String]) -> Result<Option<Task>> {
        for task in self.store.list()? {
            // One on a backend this deckd lacks cannot run here; dropping a
            // backend leaves it alone, as `Runtime::open` does.
            if task.spec.kind != TaskKind::Coordinator
                || task.state == State::Destroyed
                || !self.backends.has(&task.backend)
            {
                continue;
            }
            if scope_of(self, &task)? == projects {
                return Ok(Some(task));
            }
        }
        Ok(None)
    }
}

fn scope_of(inner: &Inner, task: &Task) -> Result<Vec<String>> {
    match coordinator::read(inner.store.data(), &task.id)? {
        Some(policy) => Ok(policy.projects),
        None => Ok(task.spec.project.iter().cloned().collect()),
    }
}

impl Runtime {
    /// The coordinator for `projects` on this backend, running, stopped or
    /// failed, but not destroyed (ADR 0011 §2: one per scope).
    pub fn coordinator(&self, projects: &[String]) -> Result<Option<Task>> {
        let projects = coordinator::scope(projects)?;
        self.lock().coordinator(&projects)
    }

    /// A coordinator's policy as `deckd` last wrote it.
    pub fn policy(&self, id: &str) -> Result<Policy> {
        let inner = self.lock();
        let task = inner.store.load(id)?;
        if task.spec.kind != TaskKind::Coordinator {
            bail!("task {id} is not a coordinator");
        }
        coordinator::read(inner.store.data(), id)?.with_context(|| format!("coordinator {id} has no policy"))
    }

    /// Replace a coordinator's policy for the user (ADR 0011 §4, deck#151
    /// R7). The scope is fixed at create. The file is replaced atomically,
    /// the record says who changed it, and a changed `model_budget` becomes
    /// the coordinator's live budget. Nothing already running is stopped.
    pub fn set_policy(&self, id: &str, policy: Policy) -> Result<Policy> {
        let inner = self.lock();
        let mut task = inner.store.load(id)?;
        if task.spec.kind != TaskKind::Coordinator {
            bail!("task {id} is not a coordinator");
        }
        if task.state == State::Destroyed {
            bail!("coordinator {id} was destroyed");
        }
        let data = inner.store.data();
        let before = coordinator::read(data, id)?;
        let mut policy = policy;
        policy.validate()?;
        let scope = match &before {
            Some(before) => before.projects.clone(),
            None => task.spec.project.iter().cloned().collect(),
        };
        if policy.projects != scope {
            bail!("a coordinator's projects are fixed when it is created");
        }
        let policy = coordinator::write(data, id, policy)?;
        if before.as_ref().map(|before| before.model_budget) != Some(policy.model_budget) {
            inner.store.set_budget(id, policy.model_budget.requests, policy.model_budget.tokens)?;
        }
        inner.store.append(&mut task, Event::PolicyChanged { by: BY_USER.into() })?;
        Ok(policy)
    }

    /// The agent refused to set up its session, as Construct refuses a model
    /// under `optchat`'s window floor (deck#151 R6): stop it and record the
    /// refusal as a failed start, so the record shows why and a resume may
    /// retry. Nothing changes for a task that is not running.
    pub fn session_refused(&self, id: &str, error: &str) -> Result<()> {
        if self.task(id)?.state != State::Running {
            return Ok(());
        }
        self.stop(id)?;
        let mut inner = self.lock();
        let mut task = inner.store.load(id)?;
        // Bounded like the window's report (`wire`), since a holder's error
        // carries whatever the agent said.
        let error: String = error.trim().chars().take(REASON_CHARS).collect();
        let failure = Failure::StartFailed(format!("the agent refused its session: {error}"));
        inner.record(&mut task, Event::Failed { failure })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::Local;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deck-runtime-coordinator-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn coordinator(dir: &Path, argv: &[&str]) -> Spec {
        Spec {
            argv: argv.iter().map(|arg| (*arg).into()).collect(),
            cwd: dir.to_path_buf(),
            project: Some("git:scope".into()),
            kind: TaskKind::Coordinator,
            ..Default::default()
        }
    }

    fn eventually(mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(Instant::now() < deadline, "not observed before timeout");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_coordinator_records_its_default_policy_and_no_github_policy() {
        let dir = scratch("create");
        let rt = Runtime::open(&dir, Local).unwrap();
        let mut spec = coordinator(&dir, &["agent"]);
        spec.github = Some(crate::github::Policy {
            repository: "owner/repo".into(),
            base: "main".into(),
            branches: vec![],
            credential: crate::github::TOKEN.into(),
            approvals: Default::default(),
            identity: Default::default(),
        });
        let id = rt.create(spec).unwrap();
        assert_eq!(rt.task(&id).unwrap().spec.github, None, "a coordinator publishes nothing");
        let policy = rt.policy(&id).unwrap();
        assert_eq!(policy.projects, vec!["git:scope".to_string()]);
        assert_eq!((policy.max_concurrent, policy.creates_per_hour), (4, 20));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn one_coordinator_per_scope_until_it_is_destroyed() {
        let dir = scratch("scope");
        let rt = Runtime::open(&dir, Local).unwrap();
        let first = rt.create(coordinator(&dir, &["agent"])).unwrap();
        assert!(rt.create(coordinator(&dir, &["other"])).is_err(), "the scope already has one");
        assert_eq!(rt.coordinator(&["git:scope".into()]).unwrap().unwrap().id, first);
        let mut elsewhere = coordinator(&dir, &["agent"]);
        elsewhere.project = Some("git:other".into());
        assert_ne!(rt.create(elsewhere).unwrap(), first, "another scope has its own");

        rt.destroy(&first).unwrap();
        assert!(rt.coordinator(&["git:scope".into()]).unwrap().is_none(), "a destroyed one does not count");
        let second = rt.create(coordinator(&dir, &["agent"])).unwrap();
        assert_ne!(second, first);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn provenance_names_a_coordinator_and_only_the_user_creates_one() {
        let dir = scratch("provenance");
        let rt = Runtime::open(&dir, Local).unwrap();
        let lead = rt.create(coordinator(&dir, &["agent"])).unwrap();
        let worker = |created_by: &str| Spec {
            argv: vec!["agent".into()],
            cwd: dir.clone(),
            project: Some("git:scope".into()),
            created_by: Some(created_by.into()),
            ..Default::default()
        };
        let id = rt.create(worker(&lead)).unwrap();
        assert_eq!(rt.task(&id).unwrap().spec.created_by.as_deref(), Some(lead.as_str()));
        assert!(rt.create(worker(&id)).is_err(), "a worker is not a coordinator");
        assert!(rt.create(worker("nobody")).is_err());
        let mut nested = coordinator(&dir, &["agent"]);
        nested.project = Some("git:nested".into());
        nested.created_by = Some(lead.clone());
        assert!(rt.create(nested).is_err(), "coordination depth is one");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_user_replaces_the_policy_and_an_invalid_one_changes_nothing() {
        let dir = scratch("policy");
        let rt = Runtime::open(&dir, Local).unwrap();
        let id = rt.create(coordinator(&dir, &["agent"])).unwrap();
        let file = dir.join("tasks").join(&id).join("coordinator.json");
        let before = std::fs::read(&file).unwrap();
        let policy = rt.policy(&id).unwrap();

        assert!(rt.set_policy(&id, Policy { max_concurrent: 0, ..policy.clone() }).is_err());
        assert!(rt.set_policy(&id, Policy { projects: vec!["git:wider".into()], ..policy.clone() }).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), before, "a refused edit leaves the file");

        let budget = coordinator::Budget { requests: Some(10), tokens: None };
        let raised = rt.set_policy(&id, Policy { max_concurrent: 8, model_budget: budget, ..policy }).unwrap();
        assert_eq!(rt.policy(&id).unwrap(), raised);
        let live: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("tasks").join(&id).join("budget.json")).unwrap()).unwrap();
        assert_eq!(live["requests"], 10, "the coordinator's own budget is live");
        let log = std::fs::read_to_string(dir.join("tasks").join(&id).join("events.jsonl")).unwrap();
        assert!(log.lines().last().unwrap().contains(r#""event":"policy_changed","by":"user""#), "{log}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_local_coordinator_runs_with_optchat_and_a_refused_session_is_a_failed_start() {
        let dir = scratch("env");
        let rt = Runtime::open(&dir, Local).unwrap();
        let out = dir.join("strategy");
        let script = format!("printf %s \"$CONSTRUCT_CONTEXT_STRATEGY\" > '{}'; exec cat", out.display());
        let (id, _stream) = rt.run(coordinator(&dir, &["sh", "-c", &script])).unwrap();
        eventually(|| std::fs::read_to_string(&out).is_ok_and(|s| s == "optchat"));

        rt.session_refused(&id, "session/new: context.optchat needs a context window of at least 65536 tokens")
            .unwrap();
        let State::Failed { failure: Failure::StartFailed(reason) } = rt.task(&id).unwrap().state else {
            panic!("a refused session must be a failed start");
        };
        assert!(reason.contains("65536"), "{reason}");

        std::fs::remove_file(&out).unwrap();
        let (again, _stream) = rt.run(coordinator(&dir, &["sh", "-c", &script])).unwrap();
        assert_eq!(again, id, "the failed coordinator is the scope's, and runs again");
        eventually(|| std::fs::read_to_string(&out).is_ok_and(|s| s == "optchat"));
        rt.stop(&id).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
