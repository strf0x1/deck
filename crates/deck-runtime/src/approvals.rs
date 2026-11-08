//! The host's boundary approval queue, its grants, and the leases that say a
//! capable window is attached to a task (ADR 0004 §5).
//!
//! The design choice: pending items and leases are ephemeral; decisions and
//! their resulting task/project grants are durable before the decision is
//! reported. A request that needs a new scope waits only while a window with
//! the boundary card is attached, and only for [`Settings::hold`]; otherwise
//! it is answered as pending at once, so a socket never waits on a human who
//! is not there. The gateway asks; this module decides and remembers.

mod access;
mod admission;
pub(crate) mod store;
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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use store::{audit_revoked, load_projects, save_projects, save_task, task_grants};

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
    holds: HashMap<String, usize>,
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
                holds: HashMap::new(),
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
            item.waiters = 0;
            self.notify(&mut inner, &item);
            inner.items.insert(id.to_owned(), item.clone());
            self.wake.notify_all();
            return Ok(item);
        }
        let Some(scope) = decision.scope() else {
            // Keep the waiter count for the decision reply: it tells a window
            // whether the agent is already learning this through its own wait.
            item.state = State::Denied;
            self.notify(&mut inner, &item);
            inner.items.insert(id.to_owned(), item.clone());
            self.wake.notify_all();
            return Ok(item);
        };
        if scope == Scope::Once && item.gateway == GITHUB && item.details.identity.is_none() {
            bail!("this GitHub gateway does not identify the reviewed write; restart the task before allowing Once");
        }
        let grant_expires_ms = self.remember(&mut inner, &item, scope, now)?;
        item.state = State::Allowed(scope);
        item.grant_expires_ms = grant_expires_ms;
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

    /// One item's state for the gateway's status endpoint. A decided item
    /// stays readable for as long as the queue keeps it.
    pub fn status(&self, task: &str, id: &str) -> Result<Option<Approval>> {
        let mut inner = self.lock();
        self.prune(&mut inner, now_ms());
        Ok(inner.items.get(id).filter(|item| item.task == task).cloned())
    }

    /// Wait up to `wait` for an item's decision, as the guest's status
    /// endpoint's long poll. The queue owns the wait, so the gateway makes one
    /// bounded control call instead of polling. `waiters` is visible to
    /// attached windows through the item's watch updates.
    pub fn wait(&self, task: &str, id: &str, wait: Duration) -> Result<Approval> {
        let mut inner = self.lock();
        let now = now_ms();
        self.prune(&mut inner, now);
        let Some(item) = inner.items.get(id).filter(|item| item.task == task).cloned() else {
            bail!("unknown approval {id:?} for task {task:?}");
        };
        if !item.state.pending() {
            return Ok(item);
        }
        let mut item = item;
        item.waiters = item.waiters.saturating_add(1);
        self.notify(&mut inner, &item);
        inner.items.insert(id.to_owned(), item);

        let until = Instant::now() + wait;
        loop {
            let now = now_ms();
            let Some(current) = inner.items.get(id) else { break };
            let left = until.saturating_duration_since(Instant::now());
            if !current.state.pending() || current.expires_ms <= now || left.is_zero() {
                break;
            }
            let left =
                left.min(Duration::from_millis(current.expires_ms.saturating_sub(now))).max(Duration::from_millis(1));
            let (guard, _) = self.wake.wait_timeout(inner, left).unwrap_or_else(|e| e.into_inner());
            inner = guard;
            self.prune(&mut inner, now_ms());
        }
        let Some(mut current) = inner.items.get(id).cloned() else {
            bail!("approval {id:?} was cleared while the agent waited");
        };
        if current.state.pending() {
            current.waiters = current.waiters.saturating_sub(1);
        } else {
            current.waiters = 0;
        }
        self.notify(&mut inner, &current);
        inner.items.insert(id.to_owned(), current.clone());
        Ok(current)
    }

    /// The agent no longer needs a pending item. Withdrawal grants nothing;
    /// the next matching request asks again.
    pub fn withdraw(&self, task: &str, id: &str) -> Result<Approval> {
        let mut inner = self.lock();
        self.prune(&mut inner, now_ms());
        let Some(item) = inner.items.get(id).filter(|item| item.task == task).cloned() else {
            bail!("unknown approval {id:?} for task {task:?}");
        };
        if !item.state.pending() {
            return Ok(item);
        }
        let mut item = item;
        item.state = State::Withdrawn;
        item.waiters = 0;
        self.notify(&mut inner, &item);
        inner.items.insert(id.to_owned(), item.clone());
        self.wake.notify_all();
        Ok(item)
    }

    /// Remove a task grant durably, then audit it before replying. The web
    /// gateway observes the file change and closes the grant's active streams.
    pub fn revoke(&self, task: &str, id: &str) -> Result<()> {
        let mut inner = self.lock();
        self.load_task(&mut inner, task)?;
        let Some(grants) = inner.grants.get_mut(task) else { bail!("unknown grant {id:?}") };
        let Some(grant) = grants.iter().find(|grant| grant.id == id).cloned() else {
            bail!("unknown grant {id:?}");
        };
        let next: Vec<_> = grants.iter().filter(|grant| grant.id != id).cloned().collect();
        save_task(&self.data, task, &next)?;
        *grants = next;
        audit_revoked(&self.data, &[grant], now_ms())?;
        Ok(())
    }

    /// A new run of a task starts with no pending items and no task grants.
    /// Project rules are the user's, not the run's, and stay.
    pub fn clear_task(&self, task: &str) -> Result<()> {
        let mut inner = self.lock();
        let removed: Vec<Approval> = inner.items.values().filter(|item| item.task == task).cloned().collect();
        inner.items.retain(|_, item| item.task != task);
        let remaining: HashSet<String> = inner.items.keys().cloned().collect();
        inner.holds.retain(|id, _| remaining.contains(id));
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
        // A window that attaches while an item is already pending must see it
        // before the next change, or the card would be invisible until then.
        for item in inner.items.values().filter(|item| item.task == task && item.state.pending()) {
            let _ = tx.unbounded_send(item.clone());
        }
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
                item.waiters = 0;
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
        inner.items.retain(|id, item| {
            inner.holds.contains_key(id) || item.state.pending() || item.expires_ms.saturating_add(keep) > now
        });
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
    fn take_grant(&self, inner: &mut Inner, request: &Incoming, now: u64) -> Option<(Scope, Option<u64>)> {
        let task_scope = inner.grants.get_mut(&request.task).and_then(|grants| {
            let index = grants
                .iter()
                .position(|grant| grant.matches(request) && grant.expires_ms.is_none_or(|expires| expires > now))?;
            let grant = grants[index].clone();
            if grant.scope == Scope::Once {
                grants.remove(index);
            }
            Some((grant.scope, grant.expires_ms))
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
            .map(|grant| (grant.scope, grant.expires_ms))
    }

    fn remember(&self, inner: &mut Inner, item: &Approval, scope: Scope, now: u64) -> Result<Option<u64>> {
        inner.seq += 1;
        let grant = Grant {
            id: format!("{:x}-g{:x}", now, inner.seq),
            task: matches!(scope, Scope::Timed | Scope::Task).then(|| item.task.clone()),
            project: (scope == Scope::Project).then(|| item.project.clone()),
            gateway: item.gateway.clone(),
            operation: item.operation.clone(),
            destination: match (scope, item.gateway.as_str()) {
                (Scope::Project, types::GITHUB) => types::github_repository(&item.destination).to_string(),
                _ => item.destination.clone(),
            },
            credential: item.credential.clone(),
            scope,
            expires_ms: match scope {
                Scope::Once => Some(now + millis(self.settings.once)),
                Scope::Timed => Some(now + millis(self.settings.timed)),
                Scope::Task => Some(now + millis(self.settings.task)),
                // Only a decision creates a grant; a default admission is
                // returned before the queue ever sees a card.
                Scope::Project | Scope::Default => None,
            },
            reviewed: (scope == Scope::Once).then(|| Reviewed { identity: item.details.identity.clone() }),
        };
        let expires_ms = grant.expires_ms;
        match scope {
            Scope::Project => {
                inner.projects.push(grant);
                save_projects(&self.data, &inner.projects, inner.version)?;
            }
            Scope::Timed | Scope::Task => {
                let grants = inner.grants.entry(item.task.clone()).or_default();
                grants.push(grant);
                save_task(&self.data, &item.task, grants)?;
            }
            Scope::Once => {
                inner.grants.entry(item.task.clone()).or_default().push(grant);
            }
            Scope::Default => {}
        }
        Ok(expires_ms)
    }

    /// Approve or revoke a whole web profile for a project. Each destination
    /// becomes a project rule (`web`/`connect` on `host:port`), so a profile
    /// the task did not start with is approved once rather than asked per
    /// package. A destination already granted by this project keeps its rule,
    /// so approving again changes nothing; revoking removes exactly the named
    /// destinations and nothing else.
    pub fn profile(&self, project: Option<&str>, destinations: &[String], revoke: bool) -> Result<usize> {
        let operations: Vec<String> = if revoke { Vec::new() } else { vec!["connect".to_string()] };
        self.rules(project, "web", &operations, destinations, revoke)
    }

    /// Always-allow a GitHub operation for a project (ADR 0007 §3). The
    /// operations are the gateway's closed set, so a project rule names one of
    /// `publish`, `create_pr` or `update_pr`; a read needs no rule.
    pub fn github_profile(
        &self,
        project: Option<&str>,
        repository: &str,
        operations: &[String],
        credential: &str,
        revoke: bool,
    ) -> Result<usize> {
        let operations: Vec<String> = operations
            .iter()
            .filter(|operation| matches!(operation.as_str(), "publish" | "create_pr" | "update_pr"))
            .cloned()
            .collect();
        if operations.is_empty() {
            bail!("a GitHub profile names publish, create_pr or update_pr");
        }
        // The gateway asks about `owner/name#…` lowercased; a rule on anything
        // else (a URL, a branch, a different case) would never match it.
        let repository = repository.trim().to_ascii_lowercase();
        let parts: Vec<&str> = repository.split('/').collect();
        let valid = parts.len() == 2
            && parts.iter().all(|part| {
                !part.is_empty()
                    && !part.starts_with('.')
                    && part.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
            });
        if !valid {
            bail!("a GitHub profile names its repository as owner/name, not {repository:?}");
        }
        self.rules_for(project, GITHUB, &operations, &repository, Some(credential), revoke)
    }

    /// The shared body of a whole-profile decision: one project rule per
    /// gateway operation on one destination, idempotent, and a revoke that
    /// removes exactly the named ones.
    fn rules(
        &self,
        project: Option<&str>,
        gateway: &str,
        operations: &[String],
        destinations: &[String],
        revoke: bool,
    ) -> Result<usize> {
        // A rule belongs to a project, whether or not there is a destination
        // to write it on.
        if project.is_none_or(str::is_empty) {
            bail!("a profile decision names the project it applies to");
        }
        let mut changed = 0;
        for destination in destinations {
            changed += self.rules_for(project, gateway, operations, destination, None, revoke)?;
        }
        Ok(changed)
    }

    fn rules_for(
        &self,
        project: Option<&str>,
        gateway: &str,
        operations: &[String],
        destination: &str,
        credential: Option<&str>,
        revoke: bool,
    ) -> Result<usize> {
        let Some(project) = project.map(str::to_owned).filter(|project| !project.is_empty()) else {
            bail!("a profile decision names the project it applies to");
        };
        let mut inner = self.lock();
        let now = now_ms();
        if revoke {
            // A revoke removes the named operations in this gateway; a
            // decision that names none removes every rule for the project on
            // the destination, which is how a web profile revokes.
            let (removed, kept): (Vec<_>, Vec<_>) = inner.projects.iter().cloned().partition(|rule| {
                let same = rule.project.as_deref() == Some(project.as_str()) && rule.destination == destination;
                let scoped = operations.is_empty() || (rule.gateway == gateway && operations.contains(&rule.operation));
                same && scoped
            });
            let changed = removed.len();
            if changed > 0 {
                save_projects(&self.data, &kept, inner.version)?;
                inner.projects = kept;
                audit_revoked(&self.data, &removed, now)?;
            }
            return Ok(changed);
        }
        let mut changed = 0;
        for operation in operations {
            let exists = inner.projects.iter().any(|rule| {
                rule.gateway == gateway
                    && rule.operation == *operation
                    && rule.destination == destination
                    && rule.project.as_deref() == Some(project.as_str())
                    && rule.credential.as_deref() == credential
            });
            if exists {
                continue;
            }
            inner.seq += 1;
            let id = format!("{:x}-p{:x}", now, inner.seq);
            inner.projects.push(Grant {
                id,
                task: None,
                project: Some(project.clone()),
                gateway: gateway.into(),
                operation: operation.clone(),
                destination: destination.to_string(),
                credential: credential.map(str::to_owned),
                scope: Scope::Project,
                expires_ms: None,
                reviewed: None,
            });
            changed += 1;
        }
        if changed > 0 {
            save_projects(&self.data, &inner.projects, inner.version)?;
        }
        Ok(changed)
    }
}

fn resolved(request: &Incoming, now: u64, policy: u64, scope: Scope, grant_expires_ms: Option<u64>) -> Approval {
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
        grant_expires_ms,
        policy,
        state: State::Allowed(scope),
        waiters: 0,
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
