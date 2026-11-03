//! The host's boundary approval queue, its grants, and the leases that say a
//! capable window is attached to a task (ADR 0004 §5).
//!
//! The design choice: pending items and leases are ephemeral; decisions and
//! their resulting task/project grants are durable before the decision is
//! reported. A request that needs a new scope waits only while a window with
//! the boundary card is attached, and only for [`Settings::hold`]; otherwise
//! it is answered as pending at once, so a socket never waits on a human who
//! is not there. The gateway asks; this module decides and remembers.

mod store;
mod types;

#[cfg(test)]
mod tests;

pub use types::*;

use anyhow::{Context as _, Result, bail};
use futures::channel::mpsc;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use store::{load_projects, save_projects, save_task, task_grants};

/// One attached window's claim on a task. Dropping it detaches the window.
pub struct Lease {
    boundary: Arc<Boundary>,
    task: String,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut inner = self.boundary.lock();
        if let Some(count) = inner.leases.get_mut(&self.task) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                inner.leases.remove(&self.task);
            }
        }
    }
}

struct Inner {
    items: HashMap<String, Approval>,
    grants: HashMap<String, Vec<Grant>>,
    loaded: HashSet<String>,
    projects: Vec<Grant>,
    leases: HashMap<String, usize>,
    waiters: HashMap<String, usize>,
    subscribers: Vec<mpsc::UnboundedSender<Approval>>,
    version: u64,
    seq: u64,
}

/// The one authority for approvals in a data root. Cloning is not allowed:
/// callers share an `Arc`.
pub struct Boundary {
    inner: Mutex<Inner>,
    wake: Condvar,
    data: PathBuf,
    settings: Settings,
}

impl Boundary {
    pub fn open(data: &Path) -> Result<Arc<Self>> {
        Self::with_settings(data, Settings::default())
    }

    pub fn with_settings(data: &Path, settings: Settings) -> Result<Arc<Self>> {
        let (projects, version) = load_projects(data)?;
        let boundary = Arc::new(Self {
            inner: Mutex::new(Inner {
                items: HashMap::new(),
                grants: HashMap::new(),
                loaded: HashSet::new(),
                projects,
                leases: HashMap::new(),
                waiters: HashMap::new(),
                subscribers: vec![],
                version,
                seq: 0,
            }),
            wake: Condvar::new(),
            data: data.to_owned(),
            settings,
        });
        Ok(boundary)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Ask for a scope. On a matching grant the request proceeds at once.
    /// Otherwise an item exists (deduplicated) and, while a capable window is
    /// attached, the call waits up to `hold` for its decision.
    pub fn submit(&self, request: Incoming) -> Result<Approval> {
        let mut inner = self.lock();
        let now = now_ms();
        self.load_task(&mut inner, &request.task)?;
        self.prune(&mut inner, now);
        if let Some(scope) = self.take_grant(&mut inner, &request, now) {
            return Ok(resolved(&request, now, inner.version, scope));
        }
        let existing = inner
            .items
            .values()
            .find(|item| item.state.pending() && item.expires_ms > now && item.same_request(&request))
            .map(|item| item.id.clone());
        let id = match existing {
            Some(id) => id,
            None => {
                inner.seq += 1;
                let id = format!("{:x}-{:x}", now, inner.seq);
                let item = Approval {
                    id: id.clone(),
                    task: request.task.clone(),
                    project: request.project.clone(),
                    session: request.session.clone(),
                    gateway: request.gateway.clone(),
                    operation: request.operation.clone(),
                    destination: request.destination.clone(),
                    credential: request.credential.clone(),
                    details: request.details.clone(),
                    reason: request.reason.clone(),
                    created_ms: now,
                    expires_ms: now + millis(self.settings.pending),
                    policy: inner.version,
                    state: State::Pending,
                };
                self.notify(&mut inner, &item);
                inner.items.insert(id.clone(), item);
                id
            }
        };
        if inner.leases.get(&request.task).copied().unwrap_or(0) == 0 {
            return Ok(inner.items[&id].clone());
        }
        let deadline = now + millis(self.settings.hold);
        *inner.waiters.entry(id.clone()).or_default() += 1;
        let item = loop {
            let item = inner.items[&id].clone();
            if !item.state.pending() || item.expires_ms <= now {
                break item;
            }
            let now = now_ms();
            let left = deadline.saturating_sub(now);
            if left == 0 {
                break item;
            }
            let (guard, _) =
                self.wake.wait_timeout(inner, Duration::from_millis(left)).unwrap_or_else(|e| e.into_inner());
            inner = guard;
            self.prune(&mut inner, now_ms());
        };
        if let Some(count) = inner.waiters.get_mut(&id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                inner.waiters.remove(&id);
            }
        }
        Ok(item)
    }

    /// Apply the first decision for an item. Later decisions report the state
    /// the item already has.
    pub fn decide(&self, id: &str, decision: Decision) -> Result<Approval> {
        let mut inner = self.lock();
        let now = now_ms();
        let Some(mut item) = inner.items.get(id).cloned() else { bail!("unknown approval {id:?}") };
        if !item.state.pending() {
            return Ok(item);
        }
        if item.expires_ms <= now {
            item.state = State::Expired;
            self.notify(&mut inner, &item);
            inner.items.insert(id.to_owned(), item.clone());
            self.wake.notify_all();
            return Ok(item);
        }
        let Some(scope) = decision.scope() else {
            item.state = State::Denied;
            self.notify(&mut inner, &item);
            inner.items.insert(id.to_owned(), item.clone());
            self.wake.notify_all();
            return Ok(item);
        };
        let waiting = inner.waiters.get(id).copied().unwrap_or(0) > 0;
        if scope != Scope::Once || !waiting {
            self.remember(&mut inner, &item, scope, now)?;
        }
        item.state = State::Allowed(scope);
        self.notify(&mut inner, &item);
        inner.items.insert(id.to_owned(), item.clone());
        self.wake.notify_all();
        Ok(item)
    }

    /// Pending items for a task, oldest first.
    pub fn pending(&self, task: &str) -> Result<Vec<Approval>> {
        let mut inner = self.lock();
        self.load_task(&mut inner, task)?;
        self.prune(&mut inner, now_ms());
        let mut items: Vec<_> =
            inner.items.values().filter(|item| item.task == task && item.state.pending()).cloned().collect();
        items.sort_by_key(|item| (item.created_ms, item.id.clone()));
        Ok(items)
    }

    /// Task-scoped grants. Project rules are looked up on submit and listed
    /// separately once the access panel needs them.
    pub fn grants(&self, task: &str) -> Result<Vec<Grant>> {
        let mut inner = self.lock();
        self.load_task(&mut inner, task)?;
        Ok(inner.grants.get(task).cloned().unwrap_or_default())
    }

    /// Drop a task grant. The next matching request asks again.
    pub fn revoke(&self, task: &str, id: &str) -> Result<()> {
        let mut inner = self.lock();
        self.load_task(&mut inner, task)?;
        let Some(grants) = inner.grants.get_mut(task) else { bail!("unknown grant {id:?}") };
        let before = grants.len();
        grants.retain(|grant| grant.id != id);
        if grants.len() == before {
            bail!("unknown grant {id:?}");
        }
        save_task(&self.data, task, grants)?;
        Ok(())
    }

    /// A new run of a task starts with no pending items and no task grants.
    /// Project rules are the user's, not the run's, and stay.
    pub fn clear_task(&self, task: &str) -> Result<()> {
        let mut inner = self.lock();
        let removed: Vec<Approval> = inner.items.values().filter(|item| item.task == task).cloned().collect();
        inner.items.retain(|_, item| item.task != task);
        let remaining: HashSet<String> = inner.items.keys().cloned().collect();
        inner.waiters.retain(|id, _| remaining.contains(id));
        inner.grants.remove(task);
        inner.loaded.remove(task);
        let path = task_grants(&self.data, task);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("removing {}", path.display())),
        }
        inner.subscribers.retain(|tx| removed.iter().all(|item| tx.unbounded_send(item.clone()).is_ok()));
        self.wake.notify_all();
        Ok(())
    }

    /// Attach a capable window to `task`. The returned receiver sees every
    /// item change; the connection owning the lease filters by task.
    pub fn watch(self: &Arc<Self>, task: &str) -> (Lease, mpsc::UnboundedReceiver<Approval>) {
        let (tx, rx) = mpsc::unbounded();
        let mut inner = self.lock();
        inner.subscribers.push(tx);
        *inner.leases.entry(task.to_owned()).or_default() += 1;
        (Lease { boundary: self.clone(), task: task.to_owned() }, rx)
    }

    fn notify(&self, inner: &mut Inner, item: &Approval) {
        inner.subscribers.retain(|tx| tx.unbounded_send(item.clone()).is_ok());
    }

    fn prune(&self, inner: &mut Inner, now: u64) {
        let mut expired = vec![];
        for item in inner.items.values_mut() {
            if item.state.pending() && item.expires_ms <= now {
                item.state = State::Expired;
                expired.push(item.clone());
            }
        }
        inner.grants.retain(|_, grants| {
            grants.retain(|grant| grant.expires_ms.is_none_or(|expires| expires > now));
            true
        });
        // Keep a decided item long enough to answer a late status request,
        // then forget it; pending items are never dropped before expiry.
        let keep = millis(self.settings.pending);
        inner.items.retain(|_, item| item.state.pending() || item.expires_ms.saturating_add(keep) > now);
        inner.subscribers.retain(|tx| expired.iter().all(|item| tx.unbounded_send(item.clone()).is_ok()));
        if !expired.is_empty() {
            self.wake.notify_all();
        }
    }

    fn load_task(&self, inner: &mut Inner, task: &str) -> Result<()> {
        if !inner.loaded.insert(task.to_owned()) {
            return Ok(());
        }
        let path = task_grants(&self.data, task);
        match fs::read(&path) {
            Ok(bytes) => {
                let grants: Vec<Grant> =
                    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
                inner.grants.insert(task.to_owned(), grants);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        }
        Ok(())
    }

    /// A matching task or project grant, consuming an allow-once.
    fn take_grant(&self, inner: &mut Inner, request: &Incoming, now: u64) -> Option<Scope> {
        let task_scope = inner.grants.get_mut(&request.task).and_then(|grants| {
            let index = grants
                .iter()
                .position(|grant| grant.matches(request) && grant.expires_ms.is_none_or(|expires| expires > now))?;
            let grant = grants[index].clone();
            if grant.scope == Scope::Once {
                grants.remove(index);
            }
            Some(grant.scope)
        });
        if task_scope.is_some() {
            return task_scope;
        }
        inner
            .projects
            .iter()
            .find(|grant| {
                grant.project.as_deref() == Some(request.project.as_str())
                    && grant.matches(request)
                    && grant.expires_ms.is_none_or(|expires| expires > now)
            })
            .map(|grant| grant.scope)
    }

    fn remember(&self, inner: &mut Inner, item: &Approval, scope: Scope, now: u64) -> Result<()> {
        inner.seq += 1;
        let grant = Grant {
            id: format!("{:x}-g{:x}", now, inner.seq),
            task: (scope == Scope::Task).then(|| item.task.clone()),
            project: (scope == Scope::Project).then(|| item.project.clone()),
            gateway: item.gateway.clone(),
            operation: item.operation.clone(),
            destination: item.destination.clone(),
            scope,
            expires_ms: (scope == Scope::Once).then(|| now + millis(self.settings.once)),
        };
        match scope {
            Scope::Project => {
                inner.projects.push(grant);
                save_projects(&self.data, &inner.projects, inner.version)?;
            }
            Scope::Task => {
                let grants = inner.grants.entry(item.task.clone()).or_default();
                grants.push(grant);
                save_task(&self.data, &item.task, grants)?;
            }
            Scope::Once => {
                inner.grants.entry(item.task.clone()).or_default().push(grant);
            }
        }
        Ok(())
    }
}

fn resolved(request: &Incoming, now: u64, policy: u64, scope: Scope) -> Approval {
    Approval {
        id: String::new(),
        task: request.task.clone(),
        project: request.project.clone(),
        session: request.session.clone(),
        gateway: request.gateway.clone(),
        operation: request.operation.clone(),
        destination: request.destination.clone(),
        credential: request.credential.clone(),
        details: request.details.clone(),
        reason: request.reason.clone(),
        created_ms: now,
        expires_ms: now,
        policy,
        state: State::Allowed(scope),
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
