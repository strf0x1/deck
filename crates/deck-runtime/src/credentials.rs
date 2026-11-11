//! Host credentials (ADR 0004 §4): names, fingerprints, and the one store
//! `deckd` reads them from.
//!
//! The design choice: exactly one store, chosen when the process starts. A
//! `DECK_CREDENTIALS` file is the only store while that variable is set; the
//! OS store (Keychain, Secret Service) is used otherwise. They are never
//! merged, so "which source won" has one answer. A read is bounded and at
//! most one per name is outstanding, because the OS store may be waiting on
//! an access dialog: a read that misses the bound is *unavailable*, never
//! *missing*, and the dialog's answer serves whoever asks next. No value is
//! cached here; successful calls read the store, so rotation is immediate.
//! An OS cancellation or authentication refusal is retained until a host retry; outages are not.

pub mod cli;
mod file;
mod os;
pub mod revoked;

use anyhow::{Result, bail};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Names a 0600 JSON file that is the only store while it is set.
pub const FILE_ENV: &str = "DECK_CREDENTIALS";
/// The Keychain service and the Secret Service `service` attribute.
pub const SERVICE: &str = "com.sprawlcommons.deck";
/// How long one store read may take before it is reported unavailable.
pub const READ_BOUND: Duration = Duration::from_secs(5);

/// A credential value on its way into the store from a window (ADR 0012
/// §3). It serializes as the plain string, but its `Debug` never shows it and
/// it has no `Display`, so a logged request or error cannot carry it.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Whether `name` is a credential name: `[A-Z][A-Z0-9_]{0,63}`, the shape of
/// the environment variables it replaces.
pub fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z'))
        && name.len() <= 64
        && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// `sha256:` and the first 12 hex digits of SHA-256 over the value: enough to
/// tell two machine-generated keys apart, nothing useful about either.
pub fn fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let hex: String = digest.iter().take(6).map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

/// Why a read produced no value. `Missing` is the user's to fix;
/// `Unavailable` (an unreachable store, a timed-out dialog) may pass on retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Absent {
    Missing,
    /// OS access was cancelled or authentication refused; automatic reads must not retry.
    Denied,
    Unavailable(String),
}

type Outcome = std::result::Result<String, Absent>;

#[derive(Clone)]
enum Source {
    File(PathBuf),
    Os,
    /// A store whose answers a test scripts, to prove the bound and the
    /// single outstanding read.
    #[cfg(test)]
    Scripted(Arc<dyn Fn(&str) -> Outcome + Send + Sync>),
}

/// One outstanding read: its result once the store answers.
#[derive(Default)]
struct Flight {
    outcome: Mutex<Option<Outcome>>,
    done: Condvar,
}

type Reads = Arc<Mutex<HashMap<String, Arc<Flight>>>>;

pub struct Store {
    source: Source,
    reads: Reads,
    bound: Duration,
}

impl Store {
    /// The store this process uses: the file `DECK_CREDENTIALS` names, or the
    /// OS store. A file that exists with group or other permission bits, or
    /// that is not a regular file owned by this user, is refused here.
    pub fn from_env() -> Result<Self> {
        match std::env::var_os(FILE_ENV).filter(|value| !value.is_empty()) {
            Some(path) => Self::file(PathBuf::from(path)),
            None => Ok(Self::with(Source::Os)),
        }
    }

    /// A file store at `path`. A file that does not exist yet is an empty
    /// store; `set` creates it at 0600.
    pub fn file(path: PathBuf) -> Result<Self> {
        if path.exists() {
            file::check(&path)?;
        }
        Ok(Self::with(Source::File(path)))
    }

    fn with(source: Source) -> Self {
        Self { source, reads: Reads::default(), bound: READ_BOUND }
    }

    #[cfg(test)]
    pub(crate) fn scripted(bound: Duration, answer: impl Fn(&str) -> Outcome + Send + Sync + 'static) -> Self {
        Self { bound, ..Self::with(Source::Scripted(Arc::new(answer))) }
    }

    /// Where values come from, for `list` and the daemon's log.
    pub fn describe(&self) -> String {
        match &self.source {
            Source::File(path) => format!("file {}", path.display()),
            Source::Os => os::NAME.to_string(),
            #[cfg(test)]
            Source::Scripted(_) => "scripted store".to_string(),
        }
    }

    /// The current value under `name`, read now. Waits at most the read bound
    /// for the store; a read already outstanding for the same name is joined
    /// rather than repeated.
    pub fn read(&self, name: &str) -> Outcome {
        if !valid_name(name) {
            return Err(Absent::Missing);
        }
        let (flight, leader) = {
            let mut reads = self.reads.lock().unwrap_or_else(|e| e.into_inner());
            match reads.get(name) {
                Some(flight) => (flight.clone(), false),
                None => {
                    let flight = Arc::new(Flight::default());
                    reads.insert(name.to_string(), flight.clone());
                    (flight, true)
                }
            }
        };
        if leader {
            self.lead(name, flight.clone());
        }
        let deadline = Instant::now() + self.bound;
        let mut outcome = flight.outcome.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(outcome) = outcome.as_ref() {
                return outcome.clone();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Absent::Unavailable(format!(
                    "the {} did not answer within {} s",
                    self.describe(),
                    self.bound.as_secs()
                )));
            }
            outcome = flight.done.wait_timeout(outcome, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// Run the one read for `name` on its own thread, so a dialog that is
    /// never answered costs a parked thread, not a caller. The thread removes
    /// its entry after a non-denial answer, so the next request reads again.
    /// Cancelled/refused access remains a completed flight until a host retry.
    fn lead(&self, name: &str, flight: Arc<Flight>) {
        let source = self.source.clone();
        let owned = name.to_string();
        let reads = self.reads.clone();
        let spawned = std::thread::Builder::new().name("credential-read".into()).spawn({
            let flight = flight.clone();
            move || {
                let outcome = read_source(&source, &owned);
                finish(&reads, &owned, &flight, outcome);
            }
        });
        if let Err(error) = spawned {
            finish(&self.reads, name, &flight, Err(Absent::Unavailable(format!("starting a store read: {error}"))));
        }
    }

    /// Clear only an answered denial. A retry never starts a read, clears a
    /// pending flight, or changes a task grant or revoked fingerprint.
    pub fn retry(&self, name: &str) -> Result<bool> {
        check_name(name)?;
        let mut reads = self.reads.lock().unwrap_or_else(|e| e.into_inner());
        let denied = reads.get(name).is_some_and(|flight| {
            matches!(*flight.outcome.lock().unwrap_or_else(|e| e.into_inner()), Some(Err(Absent::Denied)))
        });
        if denied {
            reads.remove(name);
        }
        Ok(denied)
    }

    /// Store `value` under `name`, replacing any value there.
    pub fn set(&self, name: &str, value: &str) -> Result<()> {
        check_name(name)?;
        if value.is_empty() {
            bail!("{name}: an empty value is not a credential");
        }
        match &self.source {
            Source::File(path) => file::set(path, name, value),
            Source::Os => os::set(name, value),
            #[cfg(test)]
            Source::Scripted(_) => bail!("a scripted store is read-only"),
        }
    }

    /// Remove `name`. False when there was nothing to remove.
    pub fn delete(&self, name: &str) -> Result<bool> {
        check_name(name)?;
        match &self.source {
            Source::File(path) => file::delete(path, name),
            Source::Os => os::delete(name),
            #[cfg(test)]
            Source::Scripted(_) => bail!("a scripted store is read-only"),
        }
    }

    /// The names the store holds, sorted.
    pub fn names(&self) -> Result<Vec<String>> {
        let mut names = match &self.source {
            Source::File(path) => file::names(path)?,
            Source::Os => os::names()?,
            #[cfg(test)]
            Source::Scripted(_) => Vec::new(),
        };
        names.retain(|name| valid_name(name));
        names.sort();
        names.dedup();
        Ok(names)
    }
}

/// Publish a read's answer; retain only explicit denial until a host retry.
fn finish(reads: &Reads, name: &str, flight: &Arc<Flight>, outcome: Outcome) {
    let mut reads = reads.lock().unwrap_or_else(|e| e.into_inner());
    let denied = outcome == Err(Absent::Denied);
    *flight.outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
    if !denied && reads.get(name).is_some_and(|current| Arc::ptr_eq(current, flight)) {
        reads.remove(name);
    }
    flight.done.notify_all();
}

fn read_source(source: &Source, name: &str) -> Outcome {
    let value = match source {
        Source::File(path) => file::read(path, name),
        Source::Os => os::read(name),
        #[cfg(test)]
        Source::Scripted(answer) => answer(name),
    }?;
    if value.is_empty() { Err(Absent::Missing) } else { Ok(value) }
}

pub(crate) fn denied_message(name: &str) -> String {
    format!(
        "OS access was cancelled or refused; automatic reads are paused. Run `deckd credential retry {name}` to try again"
    )
}

fn check_name(name: &str) -> Result<()> {
    if !valid_name(name) {
        bail!("{name:?} is not a credential name: use [A-Z][A-Z0-9_]{{0,63}}, like OPENAI_API_KEY");
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod retry_tests;
