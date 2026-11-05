//! Live model and web grants and the operations they authorize (ADR 0004 §7).
//!
//! The design choice: a connection holds the exact persisted rule that
//! admitted it. One monitor re-reads changed policy files every 50 ms and
//! cancels each operation when that rule disappears or expires, even while
//! deckd is down. A replacement grant cannot keep an older tunnel alive.
//! Fixed launch profiles and allow-once requests have no persisted grant.

use crate::web::policy::Allowed;
use serde::Deserialize;
use std::collections::HashMap;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CHECK: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct Rule {
    id: String,
    gateway: String,
    operation: String,
    destination: String,
    scope: String,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    expires_ms: Option<u64>,
}

impl Rule {
    fn live(&self) -> bool {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_millis());
        self.expires_ms.is_none_or(|expires| u128::from(expires) > now)
    }
}

/// The file and exact rule that admitted an operation. Kept until it ends.
#[derive(Debug, Clone)]
pub struct Permit {
    file: usize,
    rule: Rule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    length: u64,
    identity: Option<(u64, u64)>,
}

#[derive(Debug, Default)]
struct Cache {
    stamps: Vec<Stamp>,
    rules: Vec<Vec<Rule>>,
}

struct Active {
    permit: Permit,
    cancel: Box<dyn Fn() + Send + Sync>,
}

#[derive(Default)]
struct Connections {
    next: u64,
    active: HashMap<u64, Active>,
}

pub struct Grants {
    files: Vec<PathBuf>,
    task: String,
    project: Option<String>,
    cached: Mutex<Cache>,
    connections: Mutex<Connections>,
}

impl Grants {
    pub fn new(files: Vec<PathBuf>, task: String, project: Option<String>) -> Self {
        Self {
            files,
            task,
            project,
            cached: Mutex::new(Cache::default()),
            connections: Mutex::new(Connections::default()),
        }
    }

    /// A task/project grant for this operation and exact destination. Once
    /// grants are consumed by the daemon, never admitted from a policy file.
    pub fn web(&self, host: &str, port: u16, operation: &str) -> Option<Permit> {
        self.find(crate::web::GATEWAY, |rule| {
            (rule.operation == operation || (rule.operation == crate::web::CONNECT && operation == crate::web::FETCH))
                && Allowed::parse(&rule.destination).is_some_and(|allowed| allowed.admits(host, port))
        })
    }

    /// Model names are exact; a queue reply must name the same persisted scope.
    pub fn model(&self, model: &str, scope: Option<&str>) -> Option<Permit> {
        self.find("model", |rule| {
            rule.operation == "chat_completions"
                && rule.destination == model
                && scope.is_none_or(|scope| rule.scope == scope)
        })
    }

    fn find(&self, gateway: &str, matches: impl Fn(&Rule) -> bool) -> Option<Permit> {
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        self.refresh(&mut cached);
        cached.rules.iter().enumerate().find_map(|(file, rules)| {
            rules.iter().find_map(|rule| {
                (rule.gateway == gateway
                    && match rule.scope.as_str() {
                        "task" => rule.task.as_deref() == Some(self.task.as_str()),
                        "project" => self.project.is_some() && rule.project == self.project,
                        _ => false,
                    }
                    && rule.live()
                    && matches(rule))
                .then(|| Permit { file, rule: rule.clone() })
            })
        })
    }

    fn live(&self, permit: &Permit) -> bool {
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        self.refresh(&mut cached);
        permit.rule.live() && cached.rules.get(permit.file).is_some_and(|rules| rules.contains(&permit.rule))
    }

    fn refresh(&self, cached: &mut Cache) {
        let stamps: Vec<_> = self.files.iter().map(|path| stamp(path)).collect();
        if cached.stamps != stamps {
            cached.rules = self.files.iter().map(|path| rules(path)).collect();
            cached.stamps = stamps;
        }
    }

    /// Register before forwarding any bytes. Checking again under the
    /// connection lock covers a revocation between admission and the dial.
    pub fn register(
        self: &Arc<Self>,
        permit: Option<Permit>,
        cancel: impl Fn() + Send + Sync + 'static,
    ) -> io::Result<Option<Registration>> {
        let Some(permit) = permit else { return Ok(None) };
        let mut connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        if !self.live(&permit) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "the authorizing grant was revoked"));
        }
        let active = Active { permit, cancel: Box::new(cancel) };
        let id = connections.next;
        connections.next += 1;
        connections.active.insert(id, active);
        Ok(Some(Registration { grants: self.clone(), id }))
    }

    /// One monitor for model requests and both web entry points.
    /// A weak reference lets it end when the gateways are dropped.
    pub fn watch(self: &Arc<Self>) -> io::Result<()> {
        let grants = Arc::downgrade(self);
        std::thread::Builder::new().name("gateway-grants".into()).spawn(move || {
            loop {
                std::thread::sleep(CHECK);
                let Some(grants) = grants.upgrade() else { break };
                let revoked = {
                    let mut connections = grants.connections.lock().unwrap_or_else(|e| e.into_inner());
                    let ids: Vec<_> = connections
                        .active
                        .iter()
                        .filter(|(_, active)| !grants.live(&active.permit))
                        .map(|(id, _)| *id)
                        .collect();
                    ids.into_iter().filter_map(|id| connections.active.remove(&id)).collect::<Vec<_>>()
                };
                // Cancellation can touch a request's phase lock; never hold the registry then.
                for active in revoked {
                    (active.cancel)();
                }
            }
        })?;
        Ok(())
    }
}

/// Removing the registration releases the cancellation handle on normal completion.
pub struct Registration {
    grants: Arc<Grants>,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.grants.connections.lock().unwrap_or_else(|e| e.into_inner()).active.remove(&self.id);
    }
}

fn stamp(path: &Path) -> Stamp {
    match std::fs::metadata(path) {
        // The daemon atomically replaces policy files. A new inode must
        // invalidate the cache even when size and timestamp are unchanged.
        Ok(meta) => {
            Stamp { modified: meta.modified().ok(), length: meta.len(), identity: Some((meta.dev(), meta.ino())) }
        }
        Err(_) => Stamp { modified: None, length: 0, identity: None },
    }
}

fn rules(path: &Path) -> Vec<Rule> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum File {
        Task(Vec<Rule>),
        Project { rules: Vec<Rule> },
    }
    match std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()) {
        Some(File::Task(rules) | File::Project { rules }) => rules,
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests;
