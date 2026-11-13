//! Domain client for deckd. Startup races, detached process launch, control
//! framing and Unix paths stay here; windows see tasks and ACP streams.
//!
//! The design choice: every connection starts with a `hello` handshake. A
//! daemon from another build is refused with the command that replaces it,
//! never with a raw "unknown variant" from its request grammar. The shared
//! control connection reconnects on the next request after a daemon restart,
//! and an approval watch reconnects on its own, so a window keeps its tasks
//! through `deck daemon restart`.

use crate::approvals::{Approval, Ask, Decision, Grant};
use crate::paths::Paths;
use crate::wire::{self, Hello, Reply, Request};
use crate::{Notice, Spec, Stream, Task};
use anyhow::{Context as _, Result, anyhow, bail};
use futures::channel::mpsc;
use serde::de::DeserializeOwned;
use std::fmt;
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
/// How long a cold daemon start may take to open its control socket.
const START: Duration = Duration::from_secs(15);
/// How often a dropped approval watch tries the daemon again.
const REWATCH: Duration = Duration::from_millis(250);

/// A running daemon that cannot serve this client: another protocol, or a
/// binary that was replaced after the process started.
#[derive(Debug)]
pub struct Incompatible(String);

impl fmt::Display for Incompatible {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Incompatible {}

#[derive(Clone)]
pub struct DaemonClient {
    /// `None` after the daemon went away; the next request reconnects.
    socket: Arc<Mutex<Option<BufReader<UnixStream>>>>,
    paths: Paths,
    /// The data root when this client may start deckd again, as a window's
    /// long-lived client may; `None` for a client that only observes.
    start: Option<PathBuf>,
}

impl DaemonClient {
    pub fn connect(data: &Path) -> Result<Self> {
        Self::at(Paths::for_data(data)?, None)
    }

    fn at(paths: Paths, start: Option<PathBuf>) -> Result<Self> {
        Ok(Self::open(paths, start, true)?.0)
    }

    /// A client whose first request may launch the sibling `deckd` binary.
    /// Competing starters are resolved by deckd's data lock, then all clients
    /// retry the socket. An incompatible daemon is reported, not replaced
    /// behind the user's back: its tasks are running.
    pub fn connect_or_start(data: &Path) -> Result<Self> {
        Self::at(Paths::for_data(data)?, Some(data.to_path_buf()))
    }

    /// The daemon serving `data` and what it said, or `None` when none is
    /// running. A stale binary is not refused here: stopping it is how it is
    /// replaced.
    pub fn running(data: &Path) -> Result<Option<(Self, Hello)>> {
        let paths = Paths::for_data(data)?;
        match Self::open(paths, None, false) {
            Ok(found) => Ok(Some(found)),
            Err(e) if e.downcast_ref::<Incompatible>().is_some() => Err(e),
            Err(_) => Ok(None),
        }
    }

    fn open(paths: Paths, start: Option<PathBuf>, strict: bool) -> Result<(Self, Hello)> {
        let client = Self { socket: Arc::new(Mutex::new(None)), paths, start };
        let hello = client.hello()?;
        check(&hello, strict)?;
        Ok((client, hello))
    }

    /// The daemon's identity and compatibility. This is also the liveness
    /// test: a connect alone can succeed after idle shutdown was committed,
    /// while a reply proves deckd accepted and counted the connection.
    pub fn hello(&self) -> Result<Hello> {
        match self.request::<Hello>(Request::Hello) {
            Ok(hello) => Ok(hello),
            Err(e) if e.to_string().contains("unknown variant `hello`") => Err(Incompatible(
                "the running deckd predates this Deck's compatibility handshake, so it cannot report \
                 its version. Close Deck, run `pkill -x deckd`, and open Deck again to start the \
                 current build; its tasks keep running."
                    .into(),
            )
            .into()),
            Err(e) => Err(e),
        }
    }

    /// Ask the daemon to exit. It touches no task holder, so agents keep
    /// running and the next daemon adopts them.
    pub fn shutdown(&self) -> Result<()> {
        self.request::<serde_json::Value>(Request::Shutdown).map(|_| ())
    }

    /// A control connection, launching deckd when this client may.
    fn connect_control(&self) -> Result<BufReader<UnixStream>> {
        let e = match open(&self.paths) {
            Ok(socket) => return Ok(socket),
            Err(e) => e,
        };
        let Some(data) = self.start.as_deref() else { return Err(e) };
        launch(data)?;
        // A cold first exec of a freshly built deckd can be slow on macOS.
        let deadline = Instant::now() + START;
        loop {
            match open(&self.paths) {
                Ok(socket) => return Ok(socket),
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
    pub fn projects(&self) -> Result<Vec<crate::project::Project>> {
        self.request(Request::Projects)
    }
    pub fn open_project(&self, path: &Path, argv: Vec<String>) -> Result<crate::project::Project> {
        self.request(Request::OpenProject { path: path.into(), argv })
    }
    pub fn forget_project(&self, id: &str) -> Result<()> {
        self.request(Request::ForgetProject { id: id.into() })
    }
    pub fn run(&self, spec: Spec) -> Result<RunResult> {
        self.request(Request::Run { spec })
    }
    pub fn session(&self, id: &str, session_id: &str) -> Result<()> {
        self.request(Request::Session { id: id.into(), session_id: session_id.into() })
    }

    /// Record the turn number last shown to the user; it only moves forward.
    pub fn seen(&self, id: &str, turn: u64) -> Result<()> {
        self.request(Request::Seen { id: id.into(), turn })
    }

    /// Mark the task done (`true`) or clear the mark.
    pub fn mark(&self, id: &str, done: bool) -> Result<()> {
        self.request(Request::Mark { id: id.into(), done })
    }

    /// The settings and the revision to save against (ADR 0012 §1).
    pub fn settings(&self) -> Result<crate::settings::file::Snapshot> {
        self.request(Request::Settings)
    }

    /// Replace the settings; a save from an older revision comes back
    /// [`Stale`](crate::settings::file::Saved::Stale) with what is there now.
    pub fn set_settings(
        &self,
        settings: crate::settings::Settings,
        revision: u64,
    ) -> Result<crate::settings::file::Saved> {
        self.request(Request::SetSettings { settings, revision })
    }

    /// A saved provider's `/models` list, resolved and fetched by `deckd`.
    /// Its own connection: the request may wait on the provider.
    pub fn list_models(&self, provider: &str, refresh: bool) -> Result<crate::catalog::ModelListing> {
        Self::at(self.paths.clone(), None)?.request(Request::ListModels { provider: provider.into(), refresh })
    }

    /// Record the task's human title (ADR 0010 §9).
    pub fn title(&self, id: &str, title: &str) -> Result<()> {
        self.request(Request::Title { id: id.into(), title: title.into() })
    }

    /// Record the model gateway's latest totals for a task.
    pub fn model_usage(&self, id: &str, requests: u64, tokens: u64) -> Result<()> {
        self.request(Request::ModelUsage { task: id.into(), requests, tokens })
    }

    /// Record a successful GitHub publication on the task's record.
    pub fn publication(&self, id: &str, publication: crate::lifecycle::Publication) -> Result<()> {
        self.request::<serde_json::Value>(Request::Publication { task: id.into(), publication }).map(|_| ())
    }

    /// The task's publication and its live pull request state. `refresh`
    /// asks for a read sooner than the interval; the daemon still bounds it.
    /// Its own connection: a read waits on GitHub and must not hold up other
    /// requests on a shared client.
    pub fn pull_request(&self, id: &str, refresh: bool) -> Result<crate::pull_request::State> {
        Self::at(self.paths.clone(), None)?.request(Request::PullState { task: id.into(), refresh })
    }

    /// Replace a task's live model budget; `None` is unlimited.
    pub fn set_budget(&self, id: &str, requests: Option<u64>, tokens: Option<u64>) -> Result<()> {
        self.request(Request::SetBudget { task: id.into(), requests, tokens })
    }

    /// The names in a task's credential set with fingerprints and revoked
    /// state; never a value, so a window needs none (ADR 0004 §4.2). Store
    /// reads get their own connection: a Keychain dialog must not serialize
    /// grant controls behind the listing on a cloned window client.
    pub fn credentials(&self, task: &str) -> Result<Vec<crate::broker::Listed>> {
        Self::at(self.paths.clone(), None)?.request(Request::Credentials { task: task.into() })
    }

    /// All stored names and their current fingerprints, read by the daemon
    /// from its selected store. The CLI never opens another read path.
    pub fn stored_credentials(&self) -> Result<crate::broker::Listing> {
        Self::at(self.paths.clone(), None)?.request(Request::ListCredentials)
    }

    /// Permit a new OS read after cancellation/refusal, without fetching it.
    pub fn retry_credential(&self, name: &str) -> Result<bool> {
        Self::at(self.paths.clone(), None)?.request(Request::RetryCredential { name: name.into() })
    }

    /// Revoke a credential's current value. Running gateways close the
    /// operations that used it before their next read of the store.
    pub fn revoke_credential(&self, name: &str) -> Result<crate::credentials::revoked::Revocation> {
        Self::at(self.paths.clone(), None)?.request(Request::RevokeCredential { name: name.into() })
    }

    /// Store a value entered in a window (ADR 0012 §3) and return the name's
    /// listing: its new fingerprint, never the value. Its own connection, as
    /// an OS store may wait on a dialog.
    pub fn set_credential(&self, name: &str, value: crate::credentials::Secret) -> Result<crate::broker::Listed> {
        Self::at(self.paths.clone(), None)?.request(Request::SetCredential { name: name.into(), value })
    }

    /// Remove a name from the store; false when nothing was there.
    pub fn delete_credential(&self, name: &str) -> Result<bool> {
        Self::at(self.paths.clone(), None)?.request(Request::DeleteCredential { name: name.into() })
    }

    /// Ask the daemon's queue for a scope. This may hold for a decision when
    /// a capable window is attached; the gateway is the normal caller.
    pub fn request_approval(&self, task: &str, ask: Ask) -> Result<Approval> {
        self.request(Request::Approval { task: task.into(), ask })
    }

    pub fn approvals(&self, task: &str) -> Result<Vec<Approval>> {
        self.request(Request::Approvals { task: task.into() })
    }

    /// The task's effective GitHub approval mode, re-read from its project
    /// file; `None` when the task's project names no repository.
    pub fn github_approvals(&self, task: &str) -> Result<Option<crate::github::Approvals>> {
        self.request(Request::GithubApprovals { task: task.into() })
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

    pub fn access_grants(&self, task: &str) -> Result<Vec<Grant>> {
        self.request(Request::AccessGrants { task: task.into() })
    }

    /// Confirmed removal of a task grant or project rule. Project removal
    /// also ends matching task grants throughout that project.
    pub fn revoke_access_grant(&self, task: &str, id: &str) -> Result<()> {
        self.request(Request::RevokeAccessGrant { task: task.into(), id: id.into() })
    }

    /// Approve (`revoke = false`) or revoke a whole web profile for a project.
    /// `destinations` is the profile's expansion as the host compiled it, so a
    /// project's rule file names exactly those and the guest never defines
    /// what a profile means. Returns how many rules changed.
    pub fn web_profile(
        &self,
        task: &str,
        project: Option<&str>,
        profile: &str,
        destinations: &[String],
        revoke: bool,
    ) -> Result<u64> {
        let reply: serde_json::Value = self.request(Request::WebProfile {
            task: task.into(),
            project: project.map(str::to_owned),
            profile: profile.into(),
            destinations: destinations.to_vec(),
            revoke,
        })?;
        Ok(reply.get("changed").and_then(serde_json::Value::as_u64).unwrap_or(0))
    }

    /// Always-allow (`revoke = false`) or revoke the GitHub operations for a
    /// project. Returns how many rules changed.
    pub fn github_profile(
        &self,
        task: &str,
        project: Option<&str>,
        repository: &str,
        operations: &[String],
        revoke: bool,
    ) -> Result<u64> {
        let reply: serde_json::Value = self.request(Request::GitHubProfile {
            task: task.into(),
            project: project.map(str::to_owned),
            repository: repository.into(),
            operations: operations.to_vec(),
            revoke,
        })?;
        Ok(reply.get("changed").and_then(serde_json::Value::as_u64).unwrap_or(0))
    }

    pub fn attach(&self, id: &str) -> Result<Stream> {
        Request::Task { id: id.into() }.validate()?;
        let socket = UnixStream::connect(self.paths.task(id))?;
        Ok(Stream { reader: Box::new(BufReader::new(socket.try_clone()?)), writer: Box::new(socket) })
    }

    /// Attach this window to a task's boundary card and stream its approval
    /// changes. The daemon holds the lease for exactly this connection's life,
    /// so dropping the receiver detaches the window. A daemon restart ends the
    /// old lease; the watch reconnects and the new daemon replays the items
    /// that are still pending, so the card outlives `deck daemon restart`.
    pub fn watch(&self, task: &str) -> Result<(Watch, mpsc::UnboundedReceiver<Approval>)> {
        let mut input = self.watch_connection(task)?;
        let (tx, rx) = mpsc::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let watched = stop.clone();
        let client = self.clone();
        let task = task.to_owned();
        std::thread::spawn(move || {
            loop {
                if !forward_approvals(&mut input, &tx, &watched) {
                    return;
                }
                loop {
                    if tx.is_closed() || watched.load(Ordering::Acquire) {
                        return;
                    }
                    match client.watch_connection(&task) {
                        Ok(fresh) => {
                            // The daemon this client last spoke to is gone, and
                            // so is its shared control connection: let the next
                            // request open the new one.
                            *client.socket.lock().unwrap_or_else(|e| e.into_inner()) = None;
                            input = fresh;
                            break;
                        }
                        Err(_) => std::thread::sleep(REWATCH),
                    }
                }
            }
        });
        Ok((Watch { stop }, rx))
    }

    fn watch_connection(&self, task: &str) -> Result<BufReader<UnixStream>> {
        Request::Task { id: task.into() }.validate()?;
        let mut input = self.connect_control()?;
        input.get_mut().set_read_timeout(Some(Duration::from_secs(15)))?;
        wire::write(input.get_mut(), &Request::Watch { task: task.into() })?;
        let reply = wire::read(&mut input)?.context("deckd closed the watch before confirming it")?;
        decode::<()>(&reply)?;
        input.get_mut().set_read_timeout(Some(Duration::from_millis(100)))?;
        Ok(input)
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
        let mut retried = false;
        loop {
            if socket.is_none() {
                *socket = Some(self.connect_control()?);
            }
            let outcome = match socket.as_mut() {
                // `true` means the request reached the socket; a failed write
                // sent nothing, so even a mutation may safely be retried.
                Some(stream) => match wire::write(stream.get_mut(), &request) {
                    Ok(()) => wire::read(&mut *stream).map_err(|e| (true, e)),
                    Err(e) => Err((false, e)),
                },
                None => bail!("no control connection"),
            };
            match outcome {
                Ok(Some(line)) => return decode(&line),
                // The daemon went away, as a restart makes it. A read is
                // retried once on the new connection; a mutation is not,
                // because it may already have run and repeating it could
                // create a second task. The next request reconnects anyway.
                Ok(None) => {
                    *socket = None;
                    if request.read_only() && !retried {
                        retried = true;
                        continue;
                    }
                    bail!("deckd closed the control connection; it may have restarted")
                }
                Err((sent, e)) => {
                    *socket = None;
                    if !sent && !retried {
                        // A closed control connection, e.g. after the daemon
                        // was replaced. The request never left this process.
                        retried = true;
                        continue;
                    }
                    if request.read_only() && !retried {
                        retried = true;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }
}

/// Forward one watch connection's approval lines into `tx`. True when the
/// connection ended and may be re-established; false when the watcher is done.
fn forward_approvals(
    input: &mut BufReader<UnixStream>,
    tx: &mpsc::UnboundedSender<Approval>,
    stop: &AtomicBool,
) -> bool {
    use std::io::Read as _;
    let mut pending = Vec::new();
    loop {
        if tx.is_closed() || stop.load(Ordering::Acquire) {
            return false;
        }
        let mut bytes = [0; 4096];
        match input.read(&mut bytes) {
            Ok(0) => return true,
            Ok(n) => {
                pending.extend_from_slice(&bytes[..n]);
                while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<_> = pending.drain(..=end).collect();
                    let Ok(approval) = serde_json::from_slice(&line) else { return false };
                    if tx.unbounded_send(approval).is_err() {
                        return false;
                    }
                }
                if pending.len() > wire::LIMIT {
                    return false;
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(_) => return true,
        }
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

/// Refuse a daemon from another build. Staleness is reported only to clients
/// that will speak to it: the control commands must still reach it to stop it.
fn check(hello: &Hello, strict: bool) -> Result<()> {
    if hello.protocol != wire::PROTOCOL {
        return Err(Incompatible(format!(
            "the running deckd (pid {}, version {}) speaks control protocol {} but this Deck speaks {}. \
             Run `deck daemon restart` to replace it; its tasks keep running.",
            hello.pid,
            hello.version,
            hello.protocol,
            wire::PROTOCOL
        ))
        .into());
    }
    if strict && hello.stale {
        return Err(Incompatible(format!(
            "the running deckd (pid {}) started before {} was rebuilt, so it is still the old daemon. \
             Run `deck daemon restart` to use the current binary; its tasks keep running.",
            hello.pid,
            hello.executable.display()
        ))
        .into());
    }
    Ok(())
}

fn open(paths: &Paths) -> Result<BufReader<UnixStream>> {
    let socket = UnixStream::connect(paths.control())?;
    // A request can wait on a backend start (a holder or a VMM), so this
    // outlasts the longest of those rather than cutting them short.
    socket.set_read_timeout(Some(REPLY))?;
    socket.set_write_timeout(Some(REPLY))?;
    Ok(BufReader::new(socket))
}

/// Start the sibling deckd binary in its own session and reap the launcher.
fn launch(data: &Path) -> Result<()> {
    Paths::for_data(data)?.prepare()?;
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
    // Reap the launcher when this process stays open. Reparenting handles a
    // window that exits first; waiting never ties the daemon to Deck.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn executable() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("DECKD") {
        return Ok(path.into());
    }
    sibling_binary("deckd")
}

/// A workspace binary named `name` beside the running executable, or in the
/// nearest directory above it that has one. An installed `deck` finds its
/// `deckd` next to it. A test executable lives somewhere under the target
/// directory (`deps/` up to Rust 1.99, `build/<package>/<hash>/…` since
/// 1.100), while Cargo still places finished binaries in the profile
/// directory, so searching upward covers both layouts.
pub fn sibling_binary(name: &str) -> Result<PathBuf> {
    let current = std::env::current_exe()?;
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let dir = current.parent().context("executable has no parent directory")?;
    dir.ancestors()
        .take(6)
        .map(|d| d.join(&file))
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow!("{} is missing; build {name} alongside deck", dir.join(&file).display()))
}

#[cfg(test)]
mod tests;
