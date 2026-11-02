//! One ACP connection over any line reader and writer: feed the agent's lines
//! to [`Conn`], write the replies back on the same stream, and turn what the
//! agent says into [`Inbound`] events on a channel the UI awaits.
//!
//! The design choice: an agent is a stream, not a process. Plain threads, no
//! async runtime: one reader thread owns the input and feeds [`Conn`], writes
//! go through a shared lock so the reader (handshake replies, answers to
//! agent-initiated requests) and the UI (prompts, permission answers) never
//! interleave a line. Who launched the agent is not this module's business;
//! `deck_runtime` runs agents and hands their streams here.

use super::Inbound;
use super::conn::{Conn, Target};
use anyhow::{Result, anyhow};
use futures::channel::mpsc;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The write half of a connection.
///
/// The reader thread holds its own clone of the stream, so dropping the
/// client's writer would not close a socket the reader still references.
/// Closing has to shut the write direction down. A pipe needs nothing extra:
/// its writer descriptor is the only write end, so dropping it is the close.
pub trait Writer: Write + Send + 'static {
    /// Stop writing. Called once, when the [`Client`] drops.
    fn close(&mut self);
}

impl Writer for std::process::ChildStdin {
    fn close(&mut self) {}
}

/// What a runtime hands out, when it does not say which stream it is.
impl Writer for Box<dyn Writer> {
    fn close(&mut self) {
        (**self).close();
    }
}

#[cfg(unix)]
impl Writer for std::os::unix::net::UnixStream {
    fn close(&mut self) {
        let _ = self.shutdown(std::net::Shutdown::Write);
    }
}

type Out = Arc<Mutex<Option<Box<dyn Writer>>>>;

pub struct Client {
    conn: Arc<Mutex<Conn>>,
    out: Out,
}

impl Client {
    /// Speak ACP over `input` and `output`, starting the handshake. Events,
    /// including the `Ready` that ends the handshake, arrive on the returned
    /// receiver; it closes when the stream does.
    pub fn over(
        cwd: PathBuf,
        input: impl BufRead + Send + 'static,
        output: impl Writer,
    ) -> Result<(Client, mpsc::UnboundedReceiver<Inbound>)> {
        Self::connect(cwd, Target::New, input, output)
    }

    /// Initialize and load an existing session, replaying its history before Ready.
    pub fn load(
        cwd: PathBuf,
        input: impl BufRead + Send + 'static,
        output: impl Writer,
        session_id: String,
    ) -> Result<(Client, mpsc::UnboundedReceiver<Inbound>)> {
        Self::connect(cwd, Target::Load(session_id), input, output)
    }

    /// Reattach to a running session only when the agent advertises liveAttach.
    pub fn attach(
        cwd: PathBuf,
        input: impl BufRead + Send + 'static,
        output: impl Writer,
        session_id: String,
    ) -> Result<(Client, mpsc::UnboundedReceiver<Inbound>)> {
        Self::connect(cwd, Target::Attach(session_id), input, output)
    }

    fn connect(
        cwd: PathBuf,
        target: Target,
        input: impl BufRead + Send + 'static,
        output: impl Writer,
    ) -> Result<(Client, mpsc::UnboundedReceiver<Inbound>)> {
        let (conn, init) = Conn::begin(cwd, target, Some(request_prefix()));
        let conn = Arc::new(Mutex::new(conn));
        let out: Out = Arc::new(Mutex::new(Some(Box::new(output))));
        write(&out, &init)?;

        let (tx, rx) = mpsc::unbounded();
        let (r_conn, r_out) = (conn.clone(), out.clone());
        std::thread::Builder::new().name("acp-reader".into()).spawn(move || {
            for line in input.lines() {
                let Ok(line) = line else { break };
                #[expect(clippy::unwrap_used, reason = "poisoned lock: a panic elsewhere already ended the session")]
                let step = r_conn.lock().unwrap().read(&line);
                for l in &step.lines {
                    if write(&r_out, l).is_err() {
                        return;
                    }
                }
                for ev in step.events {
                    if tx.unbounded_send(ev).is_err() {
                        return;
                    }
                }
            }
            let _ = tx.unbounded_send(Inbound::Failed("the connection closed".into()));
        })?;
        Ok((Client { conn, out }, rx))
    }

    /// Send a prompt. Fails before the handshake is done or once closed.
    pub fn prompt(&self, text: &str) -> Result<()> {
        #[expect(clippy::unwrap_used, reason = "poisoned lock: the reader panicked")]
        let line = self.conn.lock().unwrap().prompt(text).ok_or_else(|| anyhow!("the agent is not ready"))?;
        write(&self.out, &line)
    }

    /// Ask the agent to stop the running turn.
    pub fn cancel(&self) -> Result<()> {
        #[expect(clippy::unwrap_used, reason = "poisoned lock: the reader panicked")]
        let line = self.conn.lock().unwrap().cancel();
        line.map_or(Ok(()), |l| write(&self.out, &l))
    }

    /// Reply to a permission request with an option id, or `None` to cancel.
    pub fn answer(&self, request: &serde_json::Value, option: Option<&str>) -> Result<()> {
        #[expect(clippy::unwrap_used, reason = "poisoned lock: the reader panicked")]
        let line = self
            .conn
            .lock()
            .unwrap()
            .answer(request, option)
            .ok_or_else(|| anyhow!("the permission request is stale"))?;
        write(&self.out, &line)
    }
}

impl Drop for Client {
    /// Close the stream: the agent's input reaches end of file even though the
    /// reader thread still holds a clone. The agent itself keeps running.
    fn drop(&mut self) {
        if let Ok(mut out) = self.out.lock()
            && let Some(mut w) = out.take()
        {
            w.close();
        }
    }
}

fn write(out: &Mutex<Option<Box<dyn Writer>>>, line: &str) -> Result<()> {
    #[expect(clippy::unwrap_used, reason = "poisoned lock: a writer panicked mid-line")]
    let mut guard = out.lock().unwrap();
    let w = guard.as_mut().ok_or_else(|| anyhow!("the connection is closed"))?;
    w.write_all(line.as_bytes())?;
    w.flush()?;
    Ok(())
}

/// Namespaces survive window/process restarts without depending on the peer's epochs.
pub(crate) fn request_prefix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("deck-{}-{time}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}
