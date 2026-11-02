//! The window's task: its agent, run through `deck_runtime` with the local
//! backend, and the ACP client over the stream the runtime hands back.
//!
//! The design choice: for now the window owns its task, so closing the window
//! stops it, as before the runtime existed. The record survives either way,
//! and the next window for the same command and directory resumes it rather
//! than creating another (ADR 0001; `deckd` lifts the ownership later).

use anyhow::Result;
use deck_acp::{Client, Inbound};
use deck_runtime::{Local, Runtime, Spec};
use futures::channel::mpsc;
use std::path::{Path, PathBuf};

pub struct WindowTask {
    runtime: Runtime,
    id: String,
}

impl WindowTask {
    /// Run `argv` in `cwd` under the runtime whose records are in `data`, and
    /// connect to it.
    pub fn run(
        data: &Path,
        argv: Vec<String>,
        cwd: PathBuf,
    ) -> Result<(WindowTask, Client, mpsc::UnboundedReceiver<Inbound>)> {
        let runtime = Runtime::open(data, Local)?;
        let (id, stream) = runtime.run(Spec { argv, cwd: cwd.clone() })?;
        // From here a failure stops the task on its way out.
        let task = WindowTask { runtime, id };
        let (client, rx) = Client::over(cwd, stream.reader, stream.writer)?;
        Ok((task, client, rx))
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for WindowTask {
    fn drop(&mut self) {
        if let Err(e) = self.runtime.stop(&self.id) {
            eprintln!("deck: stopping task {}: {e:#}", self.id);
        }
    }
}
