//! The runtime contract: create, start, attach, stop, resume and destroy
//! tasks, and a stream of the state changes. It keeps every task's record and
//! drives one backend.
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

/// How long a stop waits after asking before it forces.
pub const GRACE: Duration = Duration::from_secs(3);

/// A task changed state; the record already says so.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Notice {
    pub task: String,
    pub state: State,
}

/// Cheap to clone; clones share one runtime.
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    store: Store,
    backend: Box<dyn Backend>,
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
        let store = Store::open(data)?;
        let runtime = Runtime {
            inner: Arc::new(Mutex::new(Inner {
                store,
                backend: Box::new(backend),
                live: HashMap::new(),
                subscribers: vec![],
                grace: GRACE,
            })),
        };
        {
            let mut inner = runtime.lock();
            for mut task in inner.store.list()? {
                // One runtime serves one backend. A record from another
                // backend is not ours to reconcile, adopt or stop; switching
                // backends leaves it alone.
                if task.backend != inner.backend.name() {
                    continue;
                }
                let running = task.state == State::Running;
                let unconfirmed = matches!(task.state, State::Failed { failure: Failure::ShutdownUnconfirmed(_) });
                if !running && !unconfirmed {
                    continue;
                }
                if let Some(lock) = inner.store.lock(&task.id)? {
                    inner.backend.reconcile(&task)?;
                    if running {
                        let reason = "it was not running when the runtime opened".to_string();
                        inner.record(&mut task, Event::Stopped { clean: false, reason })?;
                    }
                    drop(lock);
                } else if task.backend == inner.backend.name() {
                    let generation = Arc::new(());
                    let exited = exit_notice(task.id.clone(), Arc::downgrade(&runtime.inner), generation.clone());
                    if let Some(instance) = inner.backend.adopt(&task, exited)? {
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

    /// Record a new task for `spec`. Nothing runs yet.
    pub fn create(&self, mut spec: Spec) -> Result<String> {
        if spec.project.is_none() {
            spec.project = Some(crate::project::identify(&spec.cwd)?.id);
        }
        let mut inner = self.lock();
        inner.backend.prepare(&mut spec)?;
        let name = inner.backend.name();
        let task = inner.store.create(name, spec)?;
        inner.notify(&task);
        Ok(task.id)
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
        if task.backend != inner.backend.name() {
            bail!("task {id} belongs to the {} backend, not {}", task.backend, inner.backend.name());
        }
        match &task.state {
            State::Destroyed => return Ok(()),
            State::Running => bail!("task {id} is running in another process"),
            _ => {}
        }
        let Some(_lock) = inner.store.lock(id)? else { bail!("task {id} is held by another process") };
        inner.backend.reconcile(&task)?;
        inner.backend.destroy(&task)?;
        inner.record(&mut task, Event::Destroyed)
    }

    /// Run `spec` and attach to it: reuse a task of this spec that is at
    /// rest and not held by anyone, or create one.
    pub fn run(&self, spec: Spec) -> Result<(String, Stream)> {
        let id = self.idle(&spec)?;
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

    /// A task of `spec`, from this backend, that may run again here.
    fn idle(&self, spec: &Spec) -> Result<Option<String>> {
        let inner = self.lock();
        for task in inner.store.list()? {
            let fits = task.backend == inner.backend.name() && task.spec.same_command(spec) && task.resumable();
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
        if task.backend != self.backend.name() {
            bail!("task {} belongs to the {} backend, not {}", task.id, task.backend, self.backend.name());
        }
        let lock =
            self.store.lock(&task.id)?.with_context(|| format!("task {} is held by another process", task.id))?;
        let generation = Arc::new(());
        let exited = exit_notice(task.id.clone(), me, generation.clone());
        match self.backend.start(&task, lock, exited) {
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

    fn record(&mut self, task: &mut Task, ev: Event) -> Result<()> {
        self.store.append(task, ev)?;
        self.notify(task);
        Ok(())
    }

    fn notify(&mut self, task: &Task) {
        let notice = Notice { task: task.id.clone(), state: task.state.clone() };
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
