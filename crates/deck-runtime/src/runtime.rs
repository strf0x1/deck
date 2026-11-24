//! The runtime contract: create, start, attach, stop, resume and destroy
//! tasks, and a stream of the state changes. It keeps every task's record and
//! drives its backends, one per task (ADR 0016).
//!
//! The design choice: every change of state is recorded before anyone hears
//! of it, and the record, not memory, is what [`Runtime::open`] trusts. A
//! running task holds its liveness lock (see [`crate::record`]); on open, a
//! record that says running but whose lock is free belonged to a holder that
//! died, and becomes an unclean stop. Nothing is restarted on its own.

use crate::backend::{Backend, Instance, Stream};
use crate::lifecycle::{Event, Failure, Spec, State, Task};
use crate::record::Store;
use anyhow::{Context as _, Result, anyhow, bail};
use futures::channel::mpsc;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

mod backends;
mod coordinator;
use backends::Backends;
pub use coordinator::BY_USER;

/// How long a stop waits after asking before it forces.
pub const GRACE: Duration = Duration::from_secs(3);

/// A task title's longest length, in characters: a row shows one line and an
/// agent-provided title may replace it later (ADR 0010 §9).
const TITLE_CHARS: usize = 60;

/// A task title as the record keeps it: trimmed, never empty, and at most
/// [`TITLE_CHARS`] characters, ending in `…` when it was cut.
fn bounded_title(title: &str) -> Result<String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        bail!("a task title cannot be empty");
    }
    if trimmed.chars().count() <= TITLE_CHARS {
        return Ok(trimmed.to_string());
    }
    let mut cut: String = trimmed.chars().take(TITLE_CHARS - 1).collect();
    cut.push('…');
    Ok(cut)
}

/// A task changed state or turn, or its holder sent a prompt; the record
/// already says so.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Notice {
    pub task: String,
    pub state: State,
    /// The holder's latest turn (ADR 0011 §3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<crate::Turn>,
    /// Set when this notice announces a prompt `deckd` sent through the
    /// holder (deck#150 R8): who sent it and what it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<crate::record::Prompt>,
    /// Set when this notice announces a coordinator's `create` refused at a
    /// cap (deck#152 R4), so the window can offer to raise it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<crate::control::CapNotice>,
}

/// Cheap to clone; clones share one runtime.
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    store: Store,
    backends: Backends,
    live: HashMap<String, Live>,
    subscribers: Vec<mpsc::UnboundedSender<Notice>>,
    grace: Duration,
}

struct Live {
    instance: Box<dyn Instance>,
    generation: Arc<()>,
}

impl Runtime {
    /// Open the records under `data` and reconcile them: a task recorded as
    /// running with no live holder is stopped, uncleanly.
    pub fn open(data: &Path, backend: impl Backend + 'static) -> Result<Runtime> {
        Runtime::open_with(data, vec![Box::new(backend)])
    }

    /// [`Runtime::open`] with several backends, the first the default for a
    /// task that names none (ADR 0016).
    pub fn open_with(data: &Path, backends: Vec<Box<dyn Backend>>) -> Result<Runtime> {
        let store = Store::open(data)?;
        let runtime = Runtime {
            inner: Arc::new(Mutex::new(Inner {
                store,
                backends: Backends::new(backends)?,
                live: HashMap::new(),
                subscribers: vec![],
                grace: GRACE,
            })),
        };
        {
            let mut inner = runtime.lock();
            for mut task in inner.store.list()? {
                // A record from a backend this runtime lacks is not ours to
                // reconcile, adopt or stop; dropping a backend leaves it alone.
                if !inner.backends.has(&task.backend) {
                    continue;
                }
                let running = task.state == State::Running;
                let unconfirmed = matches!(task.state, State::Failed { failure: Failure::ShutdownUnconfirmed(_) });
                if !running && !unconfirmed {
                    continue;
                }
                if let Some(lock) = inner.store.lock(&task.id)? {
                    inner.backends.of(&task)?.reconcile(&task)?;
                    if running {
                        let reason = "it was not running when the runtime opened".to_string();
                        inner.record(&mut task, Event::Stopped { clean: false, reason })?;
                    }
                    drop(lock);
                } else {
                    let generation = Arc::new(());
                    let exited = exit_notice(task.id.clone(), Arc::downgrade(&runtime.inner), generation.clone());
                    if let Some(instance) = inner.backends.of(&task)?.adopt(&task, exited)? {
                        if running {
                            inner.record(&mut task, Event::Adopted)?;
                        }
                        inner.live.insert(task.id.clone(), Live { instance, generation });
                    }
                }
            }
        }
        Ok(runtime)
    }

    /// Use `grace` instead of [`GRACE`] for stops.
    pub fn with_grace(self, grace: Duration) -> Runtime {
        self.lock().grace = grace;
        self
    }

    pub(crate) fn has_live_tasks(&self) -> bool {
        !self.lock().live.is_empty()
    }

    /// State changes from now on, each after it is recorded.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<Notice> {
        let (tx, rx) = mpsc::unbounded();
        self.lock().subscribers.push(tx);
        rx
    }

    pub fn task(&self, id: &str) -> Result<Task> {
        self.lock().store.load(id)
    }

    pub fn tasks(&self) -> Result<Vec<Task>> {
        self.lock().store.list()
    }

    /// The task's holder wrote a new turn; announce it.
    pub fn turn_changed(&self, id: &str) -> Result<()> {
        let mut inner = self.lock();
        let task = inner.store.load(id)?;
        inner.notify(&task);
        Ok(())
    }

    /// Record a prompt the holder started (deck#150 R8) and announce it.
    pub fn prompted(&self, id: &str, turn: u64, from: Option<&str>, text: &str) -> Result<crate::record::Prompt> {
        let mut inner = self.lock();
        let prompt = inner.store.append_prompt(id, turn, from, text)?;
        let task = inner.store.load(id)?;
        let notice = Notice {
            task: task.id.clone(),
            state: task.state.clone(),
            turn: task.turn.clone(),
            prompt: Some(prompt.clone()),
            cap: None,
        };
        inner.subscribers.retain(|tx| tx.unbounded_send(notice.clone()).is_ok());
        Ok(prompt)
    }

    /// Announce that coordinator `id`'s `create` met a cap. Nothing is
    /// recorded here; the coordinator's `control` event is the audit.
    pub fn announce_cap(&self, id: &str, cap: crate::control::CapNotice) -> Result<()> {
        let mut inner = self.lock();
        let task = inner.store.load(id)?;
        let notice = Notice {
            task: task.id.clone(),
            state: task.state.clone(),
            turn: task.turn.clone(),
            prompt: None,
            cap: Some(cap),
        };
        inner.subscribers.retain(|tx| tx.unbounded_send(notice.clone()).is_ok());
        Ok(())
    }

    /// Append a coordinator's control audit row (deck#152 R5).
    pub fn audit(&self, id: &str, op: &str, target: Option<&str>, outcome: &str) -> Result<()> {
        let inner = self.lock();
        let mut task = inner.store.load(id)?;
        let event = Event::Control { op: op.into(), target: target.map(str::to_owned), outcome: outcome.into() };
        inner.store.append(&mut task, event)
    }

    /// When task `id` was created, from its record's first line.
    pub fn created_ms(&self, id: &str) -> Result<u64> {
        self.lock().store.created_ms(id)
    }

    /// The last assistant message `id`'s holder recorded.
    pub fn last_message(&self, id: &str) -> Option<crate::message::LastMessage> {
        self.lock().store.message(id)
    }

    /// The prompts the task's holder sent, oldest first.
    pub fn prompts(&self, id: &str) -> Result<Vec<crate::record::Prompt>> {
        let inner = self.lock();
        inner.store.load(id)?;
        inner.store.prompts(id)
    }

    /// Remember the agent's session once ACP has identified it. Repeating
    /// the same pointer leaves the durable log unchanged.
    pub fn session(&self, id: &str, session_id: &str) -> Result<()> {
        if session_id.trim().is_empty() {
            bail!("an ACP session id cannot be empty");
        }
        let mut inner = self.lock();
        let mut task = inner.store.load(id)?;
        if task.state != State::Running {
            bail!("task {id} cannot record a session while {:?}", task.state);
        }
        if task.session_id.as_deref() == Some(session_id) {
            return Ok(());
        }
        inner.record(&mut task, Event::Session { id: session_id.into() })
    }

    /// Store the model gateway's latest totals. Repeating a report is free, a
    /// stale one is ignored, and accounting never wakes state subscribers.
    pub fn usage(&self, id: &str, requests: u64, tokens: u64) -> Result<()> {
        let inner = self.lock();
        let mut task = inner.store.load(id)?;
        if task.usage.requests == requests && task.usage.tokens == tokens {
            return Ok(());
        }
        if requests < task.usage.requests || tokens < task.usage.tokens {
            return Ok(());
        }
        inner.store.append(&mut task, Event::Usage { requests, tokens })
    }

    /// Record the task's latest successful publication. It is accounting, not
    /// a lifecycle transition, so it is accepted after a stop and never wakes
    /// state subscribers (ADR 0008 §8). A repeated report is free.
    pub fn publication(&self, id: &str, publication: crate::lifecycle::Publication) -> Result<()> {
        let inner = self.lock();
        let mut task = inner.store.load(id)?;
        if task.publication.as_ref() == Some(&publication) {
            return Ok(());
        }
        inner.store.append(&mut task, Event::Publication { publication })
    }

    /// Record the turn number the window last showed the user. It only moves
    /// forward, and a stale report is free (ADR 0010 §6).
    pub fn seen(&self, id: &str, turn: u64) -> Result<()> {
        let inner = self.lock();
        let mut task = inner.store.load(id)?;
        if turn <= task.seen {
            return Ok(());
        }
        inner.store.append(&mut task, Event::Seen { turn })
    }

    /// Mark the task done, or clear the mark (ADR 0010 §6). Accounting, not a
    /// lifecycle transition, so it is accepted while the task runs and after
    /// it stopped.
    pub fn mark(&self, id: &str, done: bool) -> Result<()> {
        let inner = self.lock();
        let mut task = inner.store.load(id)?;
        if task.done_ms.is_some() == done {
            return Ok(());
        }
        inner.store.append(&mut task, Event::Marked { done })
    }

    /// Record the task's human title, from the first line of its first prompt
    /// (ADR 0010 §9). It is trimmed, an empty one is refused, and a long one
    /// is cut to [`TITLE_CHARS`] characters ending in `…`. Recording the
    /// title it already has leaves the log unchanged.
    pub fn title(&self, id: &str, title: &str) -> Result<()> {
        let title = bounded_title(title)?;
        let inner = self.lock();
        let mut task = inner.store.load(id)?;
        if task.title.as_deref() == Some(title.as_str()) {
            return Ok(());
        }
        inner.store.append(&mut task, Event::Titled { title })
    }

    /// Record the first use of a credential value by a run's gateway.
    pub fn credential_used(&self, id: &str, name: &str, fingerprint: &str) -> Result<()> {
        let inner = self.lock();
        let mut task = inner.store.load(id)?;
        inner.store.append(&mut task, Event::CredentialUsed { name: name.into(), fingerprint: fingerprint.into() })
    }

    /// Write a task's live model budget; its gateway reads the file before
    /// the next admission, so no restart is involved. `None` is unlimited.
    pub fn set_budget(&self, id: &str, requests: Option<u64>, tokens: Option<u64>) -> Result<()> {
        let inner = self.lock();
        inner.store.load(id)?;
        inner.store.set_budget(id, requests, tokens)
    }

    /// The backend a task that names none runs on, as task records name it.
    pub fn backend(&self) -> &'static str {
        self.lock().backends.default_name()
    }

    /// Every backend this runtime drives, the default first.
    pub fn backends(&self) -> Vec<&'static str> {
        self.lock().backends.names()
    }

    /// Record a new task for `spec` on the backend it asks for, or the
    /// default; `spec.backend` is cleared, since the record's own `backend`
    /// says where it runs. Nothing runs yet. A coordinator gets its
    /// default policy in `coordinator.json` and no GitHub policy, and is
    /// refused when its scope already has one; a worker's `created_by` must
    /// name a coordinator.
    pub fn create(&self, spec: Spec) -> Result<String> {
        self.create_with_plan(spec, None)
    }

    pub(crate) fn create_with_plan(
        &self,
        spec: Spec,
        integration: Option<Box<crate::integration::Plan>>,
    ) -> Result<String> {
        self.create_task(spec, integration, None)
    }

    pub(crate) fn create_task(
        &self,
        mut spec: Spec,
        integration: Option<Box<crate::integration::Plan>>,
        workspace: Option<Box<crate::worktree::Workspace>>,
    ) -> Result<String> {
        if spec.project.is_none() {
            spec.project = Some(crate::project::identify(&spec.cwd)?.id);
        }
        let mut inner = self.lock();
        let policy = inner.admit(&mut spec)?;
        let backend = inner.backends.chosen(spec.backend.take().as_deref())?;
        backend.prepare(&mut spec)?;
        let name = backend.name();
        if policy.is_some() {
            // A coordinator publishes nothing, even from a repository
            // (ADR 0011 §2): no GitHub policy, so no gateway and no branch.
            spec.github = None;
        }
        let task = inner.store.create_task(name, spec, integration, workspace)?;
        if let Some(policy) = policy {
            crate::coordinator::write(inner.store.data(), &task.id, policy)?;
        }
        inner.notify(&task);
        Ok(task.id)
    }

    pub(crate) fn record_integration(&self, id: &str, plan: crate::integration::Plan) -> Result<()> {
        let mut inner = self.lock();
        let mut task = inner.store.load(id)?;
        inner.record(&mut task, Event::IntegrationUpdated { plan: Box::new(plan) })
    }

    pub(crate) fn prepare_workspace(&self, id: &str) -> Result<()> {
        let mut inner = self.lock();
        let mut task = inner.store.load(id)?;
        if !task.startable() {
            bail!("task {id} cannot prepare a workspace while {:?}", task.state);
        }
        let _lock = inner.store.lock(id)?.with_context(|| format!("task {id} is held by another process"))?;
        inner.prepare_workspace(&mut task)
    }

    /// Run a task that has never run.
    pub fn start(&self, id: &str) -> Result<()> {
        let mut inner = self.lock();
        let task = inner.store.load(id)?;
        if !task.startable() {
            bail!("task {id} has run before; resume it instead");
        }
        inner.launch(task, Arc::downgrade(&self.inner))
    }

    /// Run a task again after it stopped or failed to start.
    pub fn resume(&self, id: &str) -> Result<()> {
        let mut inner = self.lock();
        let task = inner.store.load(id)?;
        if !task.resumable() {
            bail!("task {id} cannot be resumed while {:?}", task.state);
        }
        inner.launch(task, Arc::downgrade(&self.inner))
    }

    /// Find a running task, resuming a stopped one for a prompt. Concurrent
    /// prompts share the same launch under the runtime lock.
    pub(crate) fn running_for_prompt(&self, id: &str) -> Result<Task> {
        let mut inner = self.lock();
        let task = inner.store.load(id)?;
        match task.state {
            State::Stopped { .. } => {
                inner.launch(task, Arc::downgrade(&self.inner))?;
                inner.store.load(id)
            }
            State::Running if inner.live.contains_key(id) => Ok(task),
            _ => bail!("task {id} is not running or is stopping"),
        }
    }

    /// The running task's ACP stream.
    pub fn attach(&self, id: &str) -> Result<Stream> {
        let mut inner = self.lock();
        let live = inner.live.get_mut(id).ok_or_else(|| anyhow!("task {id} is not running here"))?;
        live.instance.attach()
    }

    /// Stop a running task and confirm it is gone. If that cannot be
    /// confirmed, the task is recorded as failed and the error returned.
    pub fn stop(&self, id: &str) -> Result<()> {
        let (mut live, grace) = {
            let mut inner = self.lock();
            if inner.store.load(id)?.state != State::Running {
                bail!("task {id} is not running; destroy it to retry an unconfirmed shutdown");
            }
            let live = inner.live.remove(id).ok_or_else(|| anyhow!("task {id} is not running here"))?;
            (live, inner.grace)
        };
        // Not under the lock: a stop can take the whole grace period.
        let outcome = live.instance.stop(grace);
        let mut inner = self.lock();
        let mut task = inner.store.load(id)?;
        match outcome {
            Ok(reason) => inner.record(&mut task, Event::Stopped { clean: true, reason }),
            Err(e) => {
                let failure = Failure::ShutdownUnconfirmed(format!("{e:#}"));
                inner.live.insert(id.into(), live);
                inner.record(&mut task, Event::Failed { failure })?;
                Err(e)
            }
        }
    }

    /// Stop the task if it runs, remove what it owns, and record it as
    /// destroyed. Destroying a destroyed task does nothing.
    pub fn destroy(&self, id: &str) -> Result<()> {
        if self.lock().live.contains_key(id) {
            if self.task(id)?.state == State::Running {
                self.stop(id)?;
            } else {
                let (mut live, grace) = {
                    let mut inner = self.lock();
                    (inner.live.remove(id).context("missing failed task holder")?, inner.grace)
                };
                if let Err(error) = live.instance.stop(grace) {
                    self.lock().live.insert(id.into(), live);
                    return Err(error);
                }
            }
        }
        let mut inner = self.lock();
        let mut task = inner.store.load(id)?;
        inner.backends.of(&task)?;
        match &task.state {
            State::Destroyed => return Ok(()),
            State::Running => bail!("task {id} is running in another process"),
            _ => {}
        }
        let Some(_lock) = inner.store.lock(id)? else { bail!("task {id} is held by another process") };
        let backend = inner.backends.of(&task)?;
        backend.reconcile(&task)?;
        backend.destroy(&task)?;
        inner.record(&mut task, Event::Destroyed)
    }

    /// Remove a stopped worker's registered checkout and keep its branch.
    /// Record the intent before Git changes the filesystem so an explicit
    /// retry can finish recording a removal after a daemon restart.
    pub fn remove_worktree(&self, id: &str) -> Result<crate::worktree::WorktreeStatus> {
        let mut inner = self.lock();
        let mut task = inner.store.load(id)?;
        let workspace = task.workspace.as_ref().context("This task has no managed worktree.")?;
        if workspace.removed() {
            return Ok(workspace.status(&task));
        }
        if inner.live.contains_key(id) {
            bail!("Stop the worker before removing its worktree.");
        }
        let Some(_lock) = inner.store.lock(id)? else { bail!("The worker has not exited yet.") };
        workspace.check_removal(&task)?;
        inner.backends.of(&task)?.reconcile(&task)?;
        if !workspace.removal_pending() {
            inner.record(&mut task, Event::WorktreeRemovalStarted)?;
        }
        task.workspace.as_ref().context("This task has no managed worktree.")?.remove(&task)?;
        inner.record(&mut task, Event::WorktreeRemoved)?;
        Ok(task.workspace.as_ref().context("This task has no managed worktree.")?.status(&task))
    }

    /// Run `spec` and attach to it: reuse a task of this spec that is at
    /// rest and not held by anyone, or create one. A coordinator reuses its
    /// scope's coordinator instead, whatever its command.
    pub fn run(&self, mut spec: Spec) -> Result<(String, Stream)> {
        let id = match spec.kind {
            crate::TaskKind::Coordinator => {
                if spec.project.is_none() {
                    spec.project = Some(crate::project::identify(&spec.cwd)?.id);
                }
                let scope: Vec<String> = spec.project.iter().cloned().collect();
                self.coordinator(&scope)?.filter(|task| task.resumable()).map(|task| task.id)
            }
            crate::TaskKind::Task => self.idle(&spec)?,
        };
        match id {
            Some(id) => {
                self.resume(&id)?;
                Ok((id.clone(), self.attach(&id)?))
            }
            None => {
                let id = self.create(spec)?;
                self.start(&id)?;
                Ok((id.clone(), self.attach(&id)?))
            }
        }
    }

    /// A task of `spec`, on the backend it asks for, that may run again here.
    fn idle(&self, spec: &Spec) -> Result<Option<String>> {
        let inner = self.lock();
        let backend = spec.backend.as_deref().unwrap_or(inner.backends.default_name());
        for task in inner.store.list()? {
            let fits = task.integration.is_none()
                && task.backend == backend
                && task.spec.same_command(spec)
                && task.resumable();
            if fits && !inner.live.contains_key(&task.id) && inner.store.lock(&task.id)?.is_some() {
                return Ok(Some(task.id));
            }
        }
        Ok(None)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A poisoned lock means a panic mid-call; records are on disk.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Inner {
    fn launch(&mut self, mut task: Task, me: Weak<Mutex<Inner>>) -> Result<()> {
        if let Some(plan) = &task.integration {
            if plan.phase != crate::integration::Phase::Ready {
                bail!("integration input checkout has not been prepared");
            }
            if self.store.list()?.iter().any(|other| {
                other.id != task.id
                    && crate::integration::reserves(&other.state)
                    && other.integration.as_ref().is_some_and(|other| other.target == plan.target)
            }) {
                bail!("another integration task is active for this destination");
            }
        }
        self.backends.of(&task)?;
        let lock =
            self.store.lock(&task.id)?.with_context(|| format!("task {} is held by another process", task.id))?;
        if task.workspace.is_some()
            && let Err(error) = self.prepare_workspace(&mut task)
        {
            self.record(&mut task, Event::Failed { failure: Failure::StartFailed(format!("{error:#}")) })?;
            return Err(error);
        }
        let generation = Arc::new(());
        let exited = exit_notice(task.id.clone(), me, generation.clone());
        match self.backends.of(&task)?.start(&task, lock, exited) {
            Ok(instance) => {
                self.record(&mut task, Event::Started { pid: instance.pid(), holder: instance.holder() })?;
                self.live.insert(task.id.clone(), Live { instance, generation });
                Ok(())
            }
            Err(e) => {
                self.record(&mut task, Event::Failed { failure: Failure::StartFailed(format!("{e:#}")) })?;
                Err(e)
            }
        }
    }

    fn prepare_workspace(&mut self, task: &mut Task) -> Result<()> {
        let workspace = task.workspace.as_ref().context("task has no managed worktree")?;
        self.backends.of(task)?.reconcile(task)?;
        workspace.prepare(&task.spec.cwd, &task.id)?;
        if !workspace.ready {
            self.record(task, Event::WorktreeReady)?;
        }
        Ok(())
    }

    fn record(&mut self, task: &mut Task, ev: Event) -> Result<()> {
        self.store.append(task, ev)?;
        self.notify(task);
        Ok(())
    }

    fn notify(&mut self, task: &Task) {
        let notice = Notice {
            task: task.id.clone(),
            state: task.state.clone(),
            turn: task.turn.clone(),
            prompt: None,
            cap: None,
        };
        self.subscribers.retain(|tx| tx.unbounded_send(notice.clone()).is_ok());
    }
}

fn exit_notice(id: String, me: Weak<Mutex<Inner>>, generation: Arc<()>) -> crate::Exited {
    Box::new(move |reason| {
        let Some(inner) = me.upgrade() else { return };
        let mut inner = inner.lock().unwrap_or_else(|e| e.into_inner());
        // A stop removed the instance already and records its own outcome.
        if !inner.live.get(&id).is_some_and(|live| Arc::ptr_eq(&live.generation, &generation)) {
            return;
        }
        inner.live.remove(&id);
        if let Ok(mut task) = inner.store.load(&id)
            && task.state == State::Running
        {
            let _ = inner.record(&mut task, Event::Stopped { clean: false, reason });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::Local;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("deck-runtime-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spec(dir: &Path) -> Spec {
        Spec {
            argv: vec!["agent".into()],
            cwd: dir.to_path_buf(),
            project: Some("git:test".into()),
            github: None,
            model: None,
            ..Default::default()
        }
    }

    #[test]
    fn a_local_task_records_no_model() {
        // Construct's own config decides a local task's provider (ADR 0012
        // §2), so a selector from the window is not written into its record.
        let dir = scratch("local-model");
        let rt = Runtime::open(&dir, Local).unwrap();
        let mut spec = spec(&dir);
        spec.model = Some(crate::settings::Pick::Selector("local/qwen3-32b".into()));
        let id = rt.create(spec).unwrap();
        assert_eq!(rt.task(&id).unwrap().spec.model, None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_title_is_trimmed_capped_and_recorded_once() {
        let dir = scratch("title");
        let rt = Runtime::open(&dir, Local).unwrap();
        let id = rt.create(spec(&dir)).unwrap();

        rt.title(&id, "  fix   the rail \n").unwrap();
        assert_eq!(rt.task(&id).unwrap().title.as_deref(), Some("fix   the rail"), "trimmed, inner spaces kept");

        let log = dir.join("tasks").join(&id).join("events.jsonl");
        let before = std::fs::read_to_string(&log).unwrap();
        rt.title(&id, "fix   the rail").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), before, "the same title appends nothing");

        rt.title(&id, "a later one").unwrap();
        assert_eq!(rt.task(&id).unwrap().title.as_deref(), Some("a later one"), "a later title replaces it");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_empty_title_is_refused_and_a_long_one_is_cut_on_a_char_boundary() {
        let dir = scratch("title-bounds");
        let rt = Runtime::open(&dir, Local).unwrap();
        let id = rt.create(spec(&dir)).unwrap();
        assert!(rt.title(&id, " \n\t ").is_err());
        assert_eq!(rt.task(&id).unwrap().title, None, "nothing was recorded");

        rt.title(&id, &"a".repeat(70)).unwrap();
        let stored = rt.task(&id).unwrap().title.unwrap();
        assert_eq!(stored.chars().count(), TITLE_CHARS);
        assert!(stored.ends_with('…'));

        rt.title(&id, &"修".repeat(70)).unwrap();
        let stored = rt.task(&id).unwrap().title.unwrap();
        assert_eq!(stored.chars().count(), TITLE_CHARS, "cut by characters, not bytes");
        assert!(stored.ends_with('…'));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
