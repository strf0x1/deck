//! Domain client for deckd. Startup races, detached process launch, control
//! framing and Unix paths stay here; windows see tasks and ACP streams.

use crate::approvals::{Approval, Ask, Decision, Grant};
use crate::paths::Paths;
use crate::wire::{self, Reply, Request};
use crate::{Notice, Spec, Stream, Task};
use anyhow::{Context as _, Result, anyhow, bail};
use futures::channel::mpsc;
use serde::de::DeserializeOwned;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

pub use crate::wire::RunResult;

/// Longer than any backend start deckd waits on (a VMM gets 30 s).
const REPLY: Duration = Duration::from_secs(45);

#[derive(Clone)]
pub struct DaemonClient {
    socket: Arc<Mutex<BufReader<UnixStream>>>,
    paths: Paths,
}

impl DaemonClient {
    pub fn connect(data: &Path) -> Result<Self> {
        Self::at(Paths::for_data(data)?)
    }

    fn at(paths: Paths) -> Result<Self> {
        let socket = UnixStream::connect(paths.control())?;
        // A request can wait on a backend start (a holder or a VMM), so this
        // outlasts the longest of those rather than cutting them short.
        socket.set_read_timeout(Some(REPLY))?;
        socket.set_write_timeout(Some(REPLY))?;
        let client = Self { socket: Arc::new(Mutex::new(BufReader::new(socket))), paths };
        // A connect alone can succeed after idle shutdown was committed.
        // A reply proves deckd accepted and counted this connection.
        client.tasks()?;
        Ok(client)
    }

    /// Start the sibling deckd binary if no daemon answers. Competing starters
    /// are resolved by deckd's data lock, then all clients retry the socket.
    pub fn connect_or_start(data: &Path) -> Result<Self> {
        let paths = Paths::for_data(data)?;
        if let Ok(client) = Self::at(paths.clone()) {
            return Ok(client);
        }
        paths.prepare()?;
        let executable = executable()?;
        let mut command = Command::new(&executable);
        command.arg("--data").arg(data).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit());
        // SAFETY: setsid takes no pointers and is async-signal-safe between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().with_context(|| format!("starting {}", executable.display()))?;
        // Reap the launcher when this process stays open. Reparenting handles
        // a window that exits first; waiting never ties the daemon to Deck.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        // A cold first exec of a freshly built deckd can be slow on macOS.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match Self::at(paths.clone()) {
                Ok(client) => return Ok(client),
                Err(e) if Instant::now() >= deadline => return Err(e).context("waiting for deckd"),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    pub fn create(&self, spec: Spec) -> Result<Task> {
        self.request(Request::Create { spec })
    }
    pub fn start(&self, id: &str) -> Result<Task> {
        self.request(Request::Start { id: id.into() })
    }
    pub fn resume(&self, id: &str) -> Result<Task> {
        self.request(Request::Resume { id: id.into() })
    }
    pub fn stop(&self, id: &str) -> Result<()> {
        self.request(Request::Stop { id: id.into() })
    }
    pub fn destroy(&self, id: &str) -> Result<()> {
        self.request(Request::Destroy { id: id.into() })
    }
    pub fn task(&self, id: &str) -> Result<Task> {
        self.request(Request::Task { id: id.into() })
    }
    pub fn tasks(&self) -> Result<Vec<Task>> {
        self.request(Request::Tasks)
    }
    pub fn run(&self, spec: Spec) -> Result<RunResult> {
        self.request(Request::Run { spec })
    }
    pub fn session(&self, id: &str, session_id: &str) -> Result<()> {
        self.request(Request::Session { id: id.into(), session_id: session_id.into() })
    }

    /// Record the model gateway's latest totals for a task.
    pub fn model_usage(&self, id: &str, requests: u64, tokens: u64) -> Result<()> {
        self.request(Request::ModelUsage { task: id.into(), requests, tokens })
    }

    /// Replace a task's live model budget; `None` is unlimited.
    pub fn set_budget(&self, id: &str, requests: Option<u64>, tokens: Option<u64>) -> Result<()> {
        self.request(Request::SetBudget { task: id.into(), requests, tokens })
    }

    /// Ask the daemon's queue for a scope. This may hold for a decision when
    /// a capable window is attached; the gateway is the normal caller.
    pub fn request_approval(&self, task: &str, ask: Ask) -> Result<Approval> {
        self.request(Request::Approval { task: task.into(), ask })
    }

    pub fn approvals(&self, task: &str) -> Result<Vec<Approval>> {
        self.request(Request::Approvals { task: task.into() })
    }

    pub fn decide(&self, id: &str, decision: Decision) -> Result<Approval> {
        self.request(Request::DecideApproval { id: id.into(), decision })
    }

    /// One item's state, readable after a decision as well as while pending.
    pub fn approval_status(&self, task: &str, id: &str) -> Result<Option<Approval>> {
        self.request(Request::ApprovalStatus { task: task.into(), id: id.into() })
    }

    /// Drop a pending item on the agent's behalf.
    pub fn withdraw(&self, task: &str, id: &str) -> Result<Approval> {
        self.request(Request::WithdrawApproval { task: task.into(), id: id.into() })
    }

    pub fn grants(&self, task: &str) -> Result<Vec<Grant>> {
        self.request(Request::Grants { task: task.into() })
    }

    pub fn revoke_grant(&self, task: &str, id: &str) -> Result<()> {
        self.request(Request::RevokeGrant { task: task.into(), id: id.into() })
    }

    pub fn attach(&self, id: &str) -> Result<Stream> {
        Request::Task { id: id.into() }.validate()?;
        let socket = UnixStream::connect(self.paths.task(id))?;
        Ok(Stream { reader: Box::new(BufReader::new(socket.try_clone()?)), writer: Box::new(socket) })
    }

    /// Attach this window to a task's boundary card and stream its approval
    /// changes. The daemon holds the lease for exactly this connection's life,
    /// so dropping the receiver detaches the window.
    pub fn watch(&self, task: &str) -> Result<(Watch, mpsc::UnboundedReceiver<Approval>)> {
        let socket = UnixStream::connect(self.paths.control())?;
        socket.set_read_timeout(Some(Duration::from_secs(15)))?;
        let mut input = BufReader::new(socket);
        wire::write(input.get_mut(), &Request::Watch { task: task.into() })?;
        let reply = wire::read(&mut input)?.context("deckd closed the watch before confirming it")?;
        decode::<()>(&reply)?;
        input.get_mut().set_read_timeout(Some(Duration::from_millis(100)))?;
        let (tx, rx) = mpsc::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let watched = stop.clone();
        std::thread::spawn(move || {
            use std::io::Read as _;
            let mut pending = Vec::new();
            while !tx.is_closed() && !watched.load(Ordering::Acquire) {
                let mut bytes = [0; 4096];
                match input.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(n) => {
                        pending.extend_from_slice(&bytes[..n]);
                        while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                            let line: Vec<_> = pending.drain(..=end).collect();
                            let Ok(approval) = serde_json::from_slice(&line) else { return };
                            if tx.unbounded_send(approval).is_err() {
                                return;
                            }
                        }
                        if pending.len() > wire::LIMIT {
                            return;
                        }
                    }
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                    Err(_) => break,
                }
            }
        });
        Ok((Watch { stop }, rx))
    }

    /// Notices use a separate connection, leaving ordinary control requests
    /// available. Dropping the receiver closes the idle subscription promptly.
    pub fn subscribe(&self) -> Result<mpsc::UnboundedReceiver<Notice>> {
        let socket = UnixStream::connect(self.paths.control())?;
        socket.set_read_timeout(Some(Duration::from_secs(15)))?;
        let mut input = BufReader::new(socket);
        wire::write(input.get_mut(), &Request::Subscribe)?;
        let reply = wire::read(&mut input)?.context("deckd closed subscription")?;
        decode::<()>(&reply)?;
        input.get_mut().set_read_timeout(Some(Duration::from_millis(100)))?;
        let (tx, rx) = mpsc::unbounded();
        std::thread::spawn(move || {
            // BufRead framing retains a partial notice across timed-out reads.
            let mut pending = Vec::new();
            use std::io::Read as _;
            while !tx.is_closed() {
                let mut bytes = [0; 4096];
                match input.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(n) => {
                        pending.extend_from_slice(&bytes[..n]);
                        while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                            let line: Vec<_> = pending.drain(..=end).collect();
                            let Ok(notice) = serde_json::from_slice(&line) else { return };
                            if tx.unbounded_send(notice).is_err() {
                                return;
                            }
                        }
                        if pending.len() > wire::LIMIT {
                            return;
                        }
                    }
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                    Err(_) => break,
                }
            }
        });
        Ok(rx)
    }

    fn request<T: DeserializeOwned>(&self, request: Request) -> Result<T> {
        request.validate()?;
        let mut socket = self.socket.lock().unwrap_or_else(|e| e.into_inner());
        wire::write(socket.get_mut(), &request)?;
        let reply = wire::read(&mut *socket)?.context("deckd closed control connection")?;
        decode(&reply)
    }
}

/// Ends a watch connection when the window that owns it is dropped, even
/// though the receiving task may still be parked on an empty channel.
pub struct Watch {
    stop: Arc<AtomicBool>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn decode<T: DeserializeOwned>(line: &[u8]) -> Result<T> {
    let reply: Reply = serde_json::from_slice(line)?;
    if let Some(error) = reply.error {
        bail!("{error}");
    }
    Ok(serde_json::from_value(reply.ok.unwrap_or(serde_json::Value::Null))?)
}

fn executable() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("DECKD") {
        return Ok(path.into());
    }
    let current = std::env::current_exe()?;
    let dir = current.parent().context("executable has no parent directory")?;
    let dir = if dir.file_name().is_some_and(|n| n == "deps") {
        dir.parent().context("test directory has no parent")?
    } else {
        dir
    };
    let path = dir.join("deckd");
    if !path.is_file() {
        return Err(anyhow!("{} is missing; build deckd alongside deck", path.display()));
    }
    Ok(path)
}
