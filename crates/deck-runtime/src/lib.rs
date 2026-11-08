//! deck's task runtime: tasks with a durable record and a lifecycle, run by a
//! backend. `lifecycle` is the pure state machine, `record` the files under
//! the data root, `backend` what a backend must do, `local` the backend that
//! runs the agent as a child process, `runtime` the contract over them.
//!
//! The design choice: the record is the truth. State is folded from an
//! append-only log of lifecycle events, each fsynced before anyone hears of
//! it, and reopening the runtime trusts the log, not memory (ADR 0001). A
//! connection is not a task: [`Stream`] is what `deck_acp::Client::over`
//! speaks over, and closing it stops nothing. This crate has no UI
//! dependency, and must never gain one.

pub mod approvals;
pub mod backend;
pub mod boundary;
pub mod broker;
pub mod child;
pub mod credentials;
pub mod daemon;
pub mod daemon_client;
mod detached;
pub mod github;
pub mod holder;
pub mod import;
pub mod krun;
pub mod lifecycle;
pub mod local;
pub mod local_guard;
pub mod paths;
pub mod project;
pub mod pull_request;
mod reconcile;
pub mod record;
pub mod relay;
pub mod runtime;
pub mod vm;
pub mod vmm;
mod wire;

pub use backend::{Backend, Exited, Instance, Stream};
pub use daemon_client::{DaemonClient, RunResult, Watch};
pub use lifecycle::{Event, Failure, Publication, Spec, State, Task};
pub use local::Local;
pub use runtime::{GRACE, Notice, Runtime};
pub use vm::Vm;

use anyhow::{Context as _, Result};
use std::path::PathBuf;

/// Overrides where task records live, for tests and side-by-side builds.
pub const DATA_ENV: &str = "DECK_DATA";

/// Where task records live: `$DECK_DATA` if set; otherwise
/// `~/Library/Application Support/deck` on macOS and
/// `${XDG_DATA_HOME:-~/.local/share}/deck` elsewhere.
pub fn data_root() -> Result<PathBuf> {
    let set = |k| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(dir) = set(DATA_ENV) {
        return Ok(dir);
    }
    let home = set("HOME").context("HOME is not set, so deck has nowhere to keep task records")?;
    if cfg!(target_os = "macos") {
        return Ok(home.join("Library/Application Support/deck"));
    }
    Ok(set("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share")).join("deck"))
}
