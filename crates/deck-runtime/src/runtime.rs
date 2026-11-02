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
use crate::record::{RecordLock, Store};
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
    /// Held for as long as the task runs here.
    _lock: RecordLock,
}

impl Runtime {
    /// Open the records under `data` and reconcile them: a task recorded as
    /// running with no live holder is stopped, uncleanly.
    pub fn open(data: &Path, backend: impl Backend + 'static) -> Result<Runtime> {
        let store = Store::open(data)?;
        for mut task in store.list()? {
            if task.state == State::Running
                && let Some(lock) = store.lock(&task.id)?
            {
                let reason = "it was not running when the runtime opened".to_string();
                store.append(&mut task, Event::Stopped { clean: false, reason })?;
                drop(lock);
            }
        }
        let inner =
            Inner { store, backend: Box::new(backend), live: HashMap::new(), subscribers: vec![], grace: GRACE };
        Ok(Runtime { inner: Arc::new(Mutex::new(inner)) })
    }

    /// Use `grace` instead of [`GRACE`] for stops.
    pub fn with_grace(self, grace: Duration) -> Runtime {
        self.lock().grace = grace;
        self
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

    /// Record a new task for `spec`. Nothing runs yet.
    pub fn create(&self, spec: Spec) -> Result<String> {
        let mut inner = self.lock();
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
                inner.record(&mut task, Event::Failed { failure })?;
                Err(e)
            }
        }
    }

    /// Stop the task if it runs, remove what it owns, and record it as
    /// destroyed. Destroying a destroyed task does nothing.
    pub fn destroy(&self, id: &str) -> Result<()> {
        if self.lock().live.contains_key(id) {
            self.stop(id)?;
        }
        let mut inner = self.lock();
        let mut task = inner.store.load(id)?;
        match &task.state {
            State::Destroyed => return Ok(()),
            State::Running => bail!("task {id} is running in another process"),
            _ => {}
        }
        let Some(_lock) = inner.store.lock(id)? else { bail!("task {id} is held by another process") };
        inner.backend.destroy(&task.spec)?;
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
            let fits = task.backend == inner.backend.name() && task.spec == *spec && task.resumable();
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
        let id = task.id.clone();
        let exited = Box::new(move |reason: String| {
            let Some(inner) = me.upgrade() else { return };
            let mut inner = inner.lock().unwrap_or_else(|e| e.into_inner());
            // A task stop already removed is reported by the stop.
            if inner.live.remove(&id).is_some()
                && let Ok(mut task) = inner.store.load(&id)
            {
                let _ = inner.record(&mut task, Event::Stopped { clean: false, reason });
            }
        });
        match self.backend.start(&task.spec, exited) {
            Ok(instance) => {
                self.record(&mut task, Event::Started { pid: instance.pid() })?;
                self.live.insert(task.id.clone(), Live { instance, _lock: lock });
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
