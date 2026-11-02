//! The agent subprocess: spawn it, write lines to its stdin, and turn its
//! stdout into [`Inbound`] events on a channel the UI awaits.
//!
//! The design choice: plain threads, no async runtime. One reader thread owns
//! stdout and feeds [`Conn`]; writes go through a shared lock, so the reader
//! (handshake replies) and the UI (prompts, answers) never interleave a line.

use super::conn::{Conn, Inbound};
use anyhow::{Context as _, Result};
use futures::channel::mpsc;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

pub struct Client {
    conn: Arc<Mutex<Conn>>,
    stdin: Arc<Mutex<ChildStdin>>,
    child: Mutex<Child>,
}

impl Client {
    /// Launch `argv` in `cwd` and start the handshake. Events, including the
    /// `Ready` that ends the handshake, arrive on the returned receiver; it
    /// closes when the agent exits.
    pub fn spawn(argv: &[String], cwd: PathBuf) -> Result<(Client, mpsc::UnboundedReceiver<Inbound>)> {
        let (prog, args) = argv.split_first().context("no agent command")?;
        let mut child = Command::new(prog)
            .args(args)
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("launching {prog}"))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().context("agent stdin")?));
        let stdout = child.stdout.take().context("agent stdout")?;
        let (conn, init) = Conn::start(cwd);
        let conn = Arc::new(Mutex::new(conn));
        write(&stdin, &init)?;

        let (tx, rx) = mpsc::unbounded();
        let (r_conn, r_stdin) = (conn.clone(), stdin.clone());
        std::thread::Builder::new().name("acp-reader".into()).spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                #[expect(clippy::unwrap_used, reason = "poisoned lock: a panic elsewhere already ended the session")]
                let step = r_conn.lock().unwrap().read(&line);
                for l in &step.lines {
                    if write(&r_stdin, l).is_err() {
                        return;
                    }
                }
                for ev in step.events {
                    if tx.unbounded_send(ev).is_err() {
                        return;
                    }
                }
            }
            let _ = tx.unbounded_send(Inbound::Failed("the agent exited".into()));
        })?;
        Ok((Client { conn, stdin, child: Mutex::new(child) }, rx))
    }

    /// Send a prompt. Fails before the handshake is done.
    pub fn prompt(&self, text: &str) -> Result<()> {
        #[expect(clippy::unwrap_used, reason = "poisoned lock: the reader panicked")]
        let line = self.conn.lock().unwrap().prompt(text).context("the agent is not ready")?;
        write(&self.stdin, &line)
    }

    /// Ask the agent to stop the running turn.
    pub fn cancel(&self) -> Result<()> {
        #[expect(clippy::unwrap_used, reason = "poisoned lock: the reader panicked")]
        let line = self.conn.lock().unwrap().cancel();
        line.map_or(Ok(()), |l| write(&self.stdin, &l))
    }

    /// Reply to a permission request with an option id, or `None` to cancel.
    pub fn answer(&self, request: &serde_json::Value, option: Option<&str>) -> Result<()> {
        write(&self.stdin, &Conn::answer(request, option))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn write(stdin: &Mutex<ChildStdin>, line: &str) -> Result<()> {
    #[expect(clippy::unwrap_used, reason = "poisoned lock: a writer panicked mid-line")]
    let mut w = stdin.lock().unwrap();
    w.write_all(line.as_bytes())?;
    w.flush()?;
    Ok(())
}
