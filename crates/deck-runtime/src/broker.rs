//! `deckd`'s credential broker (ADR 0004 §4.3–§4.5): who may fetch which
//! credential, the fetch itself, revocation, and the push to running gateways.
//!
//! The design choice: the task's set is checked before the store is touched,
//! so a name outside it is refused without a read, and a revoked value is
//! refused after the read, by its fingerprint, so setting a new value restores
//! service without un-revoking anything. Nothing is cached: each fetch reads
//! the store. The value passes through this module once and is never logged;
//! the name and fingerprint are what records carry.

use crate::credentials::revoked::{Revocation, Revoked};
use crate::credentials::{self, Absent, Store};
use anyhow::{Result, bail};
use futures::channel::mpsc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

/// Why a fetch injected nothing. Each maps to one bounded error the guest
/// sees (ADR 0004 §4.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Missing,
    Unavailable(String),
    Refused,
    Revoked(String),
}

impl Refusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing => "deck_credential_missing",
            Self::Unavailable(_) => "deck_credential_unavailable",
            Self::Refused => "deck_credential_refused",
            Self::Revoked(_) => "deck_credential_revoked",
        }
    }

    /// The control reply's error: the code, then a reason that names the
    /// credential and never its value.
    pub fn reply(&self, name: &str) -> String {
        let why = match self {
            Self::Missing => format!("the host has no value for {name}; set it with `deckd credential set {name}`"),
            Self::Unavailable(why) => format!("{name} could not be read: {why}"),
            Self::Refused => format!("{name} is not a credential this task may use"),
            Self::Revoked(fingerprint) => {
                format!("{name}'s current value ({fingerprint}) was revoked; set a new value to restore it")
            }
        };
        format!("{}: {why}", self.code())
    }
}

/// One fetched value and whether this task had not used that value yet.
pub struct Fetched {
    pub value: String,
    pub fingerprint: String,
    pub first_use: bool,
}

/// A name as `deckd credential list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listed {
    pub name: String,
    pub fingerprint: Option<String>,
    pub revoked: bool,
    pub error: Option<String>,
}

/// The daemon's store description and its credential listing, without values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub store: String,
    pub credentials: Vec<Listed>,
}

struct Watcher {
    names: Vec<String>,
    tx: mpsc::UnboundedSender<Revocation>,
}

pub struct Broker {
    store: Store,
    revoked: Revoked,
    watchers: Mutex<Vec<Watcher>>,
    /// (task, fingerprint) pairs already recorded as `credential_used` since
    /// the task last started, so the record says which value a run used
    /// without an event per request.
    used: Mutex<HashSet<(String, String)>>,
}

impl Broker {
    pub fn open(data: &Path, store: Store) -> Result<Self> {
        Ok(Self {
            store,
            revoked: Revoked::open(data)?,
            watchers: Mutex::new(Vec::new()),
            used: Mutex::new(HashSet::new()),
        })
    }

    pub fn describe(&self) -> String {
        self.store.describe()
    }

    /// One value for one operation. `allowed` is the requesting task's set,
    /// from its `vmm.json`; anything else is refused before the store is read.
    pub fn fetch(&self, task: &str, name: &str, allowed: &[String]) -> Result<Fetched, Refusal> {
        if !credentials::valid_name(name) || !allowed.iter().any(|allowed| allowed == name) {
            return Err(Refusal::Refused);
        }
        let value = self.store.read(name).map_err(|absent| match absent {
            Absent::Missing => Refusal::Missing,
            Absent::Unavailable(why) => Refusal::Unavailable(why),
        })?;
        let fingerprint = credentials::fingerprint(&value);
        if self.revoked.is_revoked(&fingerprint) {
            return Err(Refusal::Revoked(fingerprint));
        }
        let first_use =
            self.used.lock().unwrap_or_else(|e| e.into_inner()).insert((task.to_string(), fingerprint.clone()));
        Ok(Fetched { value, fingerprint, first_use })
    }

    /// Revoke `name`'s current value: record it, then tell every gateway
    /// whose set holds the name. Fails when there is no value to revoke.
    pub fn revoke(&self, name: &str) -> Result<Revocation> {
        if !credentials::valid_name(name) {
            bail!("{name:?} is not a credential name");
        }
        let value = match self.store.read(name) {
            Ok(value) => value,
            Err(Absent::Missing) => bail!("{name} has no value to revoke"),
            Err(Absent::Unavailable(why)) => bail!("{name} could not be read to revoke it: {why}"),
        };
        let revocation = self.revoked.revoke(name, &credentials::fingerprint(&value))?;
        eprintln!("deckd: revoked {} {}", revocation.name, revocation.fingerprint);
        let mut watchers = self.watchers.lock().unwrap_or_else(|e| e.into_inner());
        watchers.retain(|watcher| {
            !watcher.names.iter().any(|watched| watched == name)
                || watcher.tx.unbounded_send(revocation.clone()).is_ok()
        });
        Ok(revocation)
    }

    /// A gateway's watch: the revocations already recorded for its names, and
    /// every later one.
    pub fn watch(&self, names: Vec<String>) -> (Vec<Revocation>, mpsc::UnboundedReceiver<Revocation>) {
        let (tx, rx) = mpsc::unbounded();
        // Registered before the snapshot is taken, so a revocation between the
        // two arrives twice rather than not at all.
        self.watchers.lock().unwrap_or_else(|e| e.into_inner()).push(Watcher { names: names.clone(), tx });
        (self.revoked.for_names(&names), rx)
    }

    /// A new run records the value it uses afresh.
    pub fn forget_task(&self, task: &str) {
        self.used.lock().unwrap_or_else(|e| e.into_inner()).retain(|(used, _)| used != task);
    }

    /// Every stored name with its current fingerprint and revoked state.
    pub fn list(&self) -> Result<Vec<Listed>> {
        list(&self.store, &self.revoked)
    }

    /// The listing for one task's set: only those names are read, so a window
    /// never causes a read (or a Keychain dialog) for a credential the task
    /// cannot use. A name with no stored value is listed with its error.
    pub fn list_names(&self, names: &[String]) -> Vec<Listed> {
        let mut seen = HashSet::new();
        names
            .iter()
            .filter(|name| credentials::valid_name(name) && seen.insert(name.as_str()))
            .map(|name| list_one(&self.store, &self.revoked, name.clone()))
            .collect()
    }
}

/// Read the daemon's selected store and join its outstanding reads by name.
fn list(store: &Store, revoked: &Revoked) -> Result<Vec<Listed>> {
    Ok(store.names()?.into_iter().map(|name| list_one(store, revoked, name)).collect())
}

/// One name as listed: its current fingerprint and revoked state, or why
/// there is none. This is the only place a listing reads a value.
fn list_one(store: &Store, revoked: &Revoked, name: String) -> Listed {
    let (fingerprint, error) = match store.read(&name) {
        Ok(value) => (Some(credentials::fingerprint(&value)), None),
        Err(Absent::Missing) => (None, Some("no value".to_string())),
        Err(Absent::Unavailable(why)) => (None, Some(why)),
    };
    let is_revoked = fingerprint.as_deref().is_some_and(|fingerprint| revoked.is_revoked(fingerprint));
    Listed { name, fingerprint, revoked: is_revoked, error }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn broker(name: &str) -> (Broker, PathBuf) {
        let dir = std::env::temp_dir().join(format!("deck-broker-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("credentials.json");
        let store = Store::file(file.clone()).unwrap();
        store.set("OPENAI_API_KEY", "sk-one").unwrap();
        (Broker::open(&dir, store).unwrap(), file)
    }

    fn set() -> Vec<String> {
        vec!["OPENAI_API_KEY".into()]
    }

    #[test]
    fn a_name_outside_the_set_is_refused_without_reading_the_store() {
        let (broker, file) = broker("refused");
        // An unreadable store would make any read unavailable; a refusal
        // proves nothing was read.
        std::fs::write(&file, "broken").unwrap();
        assert_eq!(broker.fetch("t", "DECK_GITHUB_TOKEN", &set()).err(), Some(Refusal::Refused));
        assert_eq!(broker.fetch("t", "lower", &["lower".into()]).err(), Some(Refusal::Refused));
        assert!(matches!(broker.fetch("t", "OPENAI_API_KEY", &set()), Err(Refusal::Unavailable(_))));
    }

    #[test]
    fn the_first_use_of_a_value_is_reported_once_per_run() {
        let (broker, _) = broker("used");
        let first = broker.fetch("t", "OPENAI_API_KEY", &set()).unwrap();
        assert_eq!((first.value.as_str(), first.first_use), ("sk-one", true));
        assert_eq!(first.fingerprint, credentials::fingerprint("sk-one"));
        assert!(!broker.fetch("t", "OPENAI_API_KEY", &set()).unwrap().first_use);
        assert!(broker.fetch("other", "OPENAI_API_KEY", &set()).unwrap().first_use);
        broker.forget_task("t");
        assert!(broker.fetch("t", "OPENAI_API_KEY", &set()).unwrap().first_use);
    }

    #[test]
    fn revoking_blocks_the_value_and_a_new_value_restores_the_name() {
        let (broker, _) = broker("revoke");
        let (snapshot, mut rx) = broker.watch(set());
        assert!(snapshot.is_empty());
        let (_, mut other) = broker.watch(vec!["DECK_GITHUB_TOKEN".into()]);
        let revocation = broker.revoke("OPENAI_API_KEY").unwrap();
        assert_eq!(revocation.fingerprint, credentials::fingerprint("sk-one"));
        assert_eq!(rx.try_recv().unwrap(), revocation, "a watcher of the name hears it");
        assert!(other.try_recv().is_err(), "a watcher of another name does not");
        assert_eq!(
            broker.fetch("t", "OPENAI_API_KEY", &set()).err(),
            Some(Refusal::Revoked(revocation.fingerprint.clone()))
        );
        assert_eq!(broker.watch(set()).0, std::slice::from_ref(&revocation), "a later watch starts with it");

        broker.store.set("OPENAI_API_KEY", "sk-two").unwrap();
        assert_eq!(broker.fetch("t", "OPENAI_API_KEY", &set()).unwrap().value, "sk-two");
        let listed = broker.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].revoked);
        assert!(broker.revoke("DECK_GITHUB_TOKEN").is_err(), "nothing to revoke");
    }

    #[test]
    fn a_task_listing_names_only_its_set_and_shows_revocation() {
        let (broker, _) = broker("listing");
        broker.store.set("OTHER_KEY", "sk-other").unwrap();
        let names = vec!["OPENAI_API_KEY".to_string(), "DECK_GITHUB_TOKEN".into(), "lower".into()];
        let listed = broker.list_names(&names);
        assert_eq!(
            listed.iter().map(|item| item.name.as_str()).collect::<Vec<_>>(),
            ["OPENAI_API_KEY", "DECK_GITHUB_TOKEN"]
        );
        assert_eq!(listed[0].fingerprint.as_deref(), Some(credentials::fingerprint("sk-one").as_str()));
        assert_eq!((listed[0].revoked, listed[1].error.as_deref()), (false, Some("no value")));
        broker.revoke("OPENAI_API_KEY").unwrap();
        assert!(broker.list_names(&names)[0].revoked);
        // The listing never carries a value.
        let json = serde_json::to_string(&broker.list_names(&names)).unwrap();
        assert!(!json.contains("sk-"), "{json}");
    }

    #[test]
    fn a_refusal_reply_starts_with_its_code_and_never_carries_a_value() {
        let reply = Refusal::Revoked("sha256:aaaaaaaaaaaa".into()).reply("OPENAI_API_KEY");
        assert!(reply.starts_with("deck_credential_revoked: "), "{reply}");
        assert!(Refusal::Missing.reply("K").starts_with("deck_credential_missing: "));
        assert!(Refusal::Refused.reply("K").starts_with("deck_credential_refused: "));
        assert!(Refusal::Unavailable("x".into()).reply("K").starts_with("deck_credential_unavailable: "));
    }
}
