//! The local agent subprocess: launch it, and kill it when its launcher goes
//! away. The connection itself is [`Client`] and knows nothing about
//! processes; a VM-hosted agent reaches deck over a socket and has no
//! [`Agent`] at all.
//!
//! The design choice: whoever launched the child owns it. Dropping [`Agent`]
//! kills and reaps the child; dropping the [`Client`] only closes the stream.

use crate::Client;
use crate::conn::Inbound;
use anyhow::{Context as _, Result};
use futures::channel::mpsc;
use std::io::BufReader;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// A local agent process, and the right to stop it.
pub struct Agent {
    child: Child,
}

impl Agent {
    /// Launch `argv` in `cwd` and connect to its stdio. Events arrive on the
    /// returned receiver; see [`Client::over`].
    pub fn launch(argv: &[String], cwd: PathBuf) -> Result<(Agent, Client, mpsc::UnboundedReceiver<Inbound>)> {
        let (prog, args) = argv.split_first().context("no agent command")?;
        let mut child = Command::new(prog)
            .args(args)
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("launching {prog}"))?;
        let stdin = child.stdin.take().context("agent stdin")?;
        let stdout = child.stdout.take().context("agent stdout")?;
        let mut agent = Agent { child };
        let (client, rx) = Client::over(cwd, BufReader::new(stdout), stdin).inspect_err(|_| {
            let _ = agent.kill();
        })?;
        Ok((agent, client, rx))
    }

    /// Stop the agent and reap it.
    pub fn kill(&mut self) -> Result<()> {
        self.child.kill().context("killing the agent")?;
        self.child.wait().context("reaping the agent")?;
        Ok(())
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
