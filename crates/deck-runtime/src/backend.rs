//! What a backend must do: run a task's agent, hand out its ACP stream, and
//! stop it with proof. The runtime owns records and state; a backend only
//! owns processes and whatever they run in.
//!
//! The design choice: a stop either confirms the process is gone or fails.
//! There is no "probably stopped", because the runtime must never reuse what
//! a process might still be writing.

use crate::lifecycle::{Holder, Task};
use crate::record::RecordLock;
use anyhow::Result;
use deck_acp::Writer;
use std::io::BufRead;
use std::time::Duration;

/// One ACP stream to a task's agent, in the shape `deck_acp::Client::over`
/// takes.
pub struct Stream {
    pub reader: Box<dyn BufRead + Send>,
    pub writer: Box<dyn Writer>,
}

/// Called once, from any thread, when a task's process ends without being
/// stopped. The argument says how it ended.
pub type Exited = Box<dyn FnOnce(String) + Send>;

pub trait Backend: Send {
    /// The name task records carry, so a record is only ever run by the kind
    /// of backend that created it.
    fn name(&self) -> &'static str;

    /// Start the task's agent and take its liveness lock. `exited` reports an
    /// end nobody asked for.
    fn start(&mut self, task: &Task, lock: RecordLock, exited: Exited) -> Result<Box<dyn Instance>>;

    /// Confirm a dead holder left no executing agent before record reuse.
    fn reconcile(&mut self, _task: &Task) -> Result<()> {
        Ok(())
    }

    /// Adopt an independently running holder. In-process backends leave it alone.
    fn adopt(&mut self, _task: &Task, _exited: Exited) -> Result<Option<Box<dyn Instance>>> {
        Ok(None)
    }

    /// Remove what the task owns besides its record (a disk, a checkout).
    /// The task is not running. The whole task, not just its spec, because a
    /// backend's per-task files live under the task's id (ADR 0003).
    fn destroy(&mut self, task: &Task) -> Result<()>;
}

/// A running task's process.
pub trait Instance: Send {
    fn pid(&self) -> Option<u32>;

    /// An independent holder, when this instance survives the runtime.
    fn holder(&self) -> Option<Holder> {
        None
    }

    /// The agent's ACP stream.
    fn attach(&mut self) -> Result<Stream>;

    /// Ask the process to end, wait up to `grace`, then force it. Returns how
    /// it ended once it is confirmed gone; an error means it may still run.
    fn stop(&mut self, grace: Duration) -> Result<String>;
}
