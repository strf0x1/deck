//! The host authority for B1 tasks. A service lock serializes control
//! operations over the existing Runtime; ACP relays never take that lock or
//! interpret JSON. The kernel data lock is held for the daemon's whole life.

use crate::approvals::{Approval, Boundary};
use crate::detached::Detached;
use crate::paths::Paths;
use crate::wire::{self, Reply, Request, RunResult};
use crate::{Runtime, Spec, State, Task};
use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::{BufReader, Read as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

pub const IDLE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);

pub struct Options {
    pub data: PathBuf,
    pub paths: Paths,
    pub idle: Duration,
}

struct Service {
    runtime: Runtime,
    boundary: Arc<Boundary>,
}

/// Run until there are no running tasks and no control clients for `idle`.
pub fn serve(options: Options) -> Result<()> {
    std::fs::create_dir_all(&options.data)?;
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(options.data.join("deckd.lock"))?;
    lock.try_lock().context("deckd is already running for this data root")?;
    let _lock = crate::record::RecordLock::held(lock);
    options.paths.prepare()?;
    let path = options.paths.control();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    let _socket = SocketPath(path);
    listener.set_nonblocking(true)?;
    let runtime = if std::env::var_os(crate::vm::IMAGE_ENV).is_some() {
        Runtime::open(&options.data, crate::vm::Vm::from_env(&options.data, options.paths.clone())?)?
    } else {
        let backend =
            Detached { executable: std::env::current_exe()?, data: options.data.clone(), paths: options.paths.clone() };
        Runtime::open(&options.data, backend)?
    };
    let boundary = Boundary::open(&options.data)?;
    let service = Arc::new(Mutex::new(Service { runtime, boundary: boundary.clone() }));
    let clients = Arc::new(AtomicUsize::new(0));
    let mut idle_since = Instant::now();
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                clients.fetch_add(1, Ordering::AcqRel);
                let client_count = clients.clone();
                let service = service.clone();
                let boundary = boundary.clone();
                std::thread::spawn(move || {
                    let _connected = Connected(client_count);
                    if let Err(e) = connection(socket, service, boundary) {
                        eprintln!("deckd: client: {e:#}");
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e).context("accepting daemon client"),
        }
        let running = {
            let service = service.lock().unwrap_or_else(|e| e.into_inner());
            service.runtime.has_live_tasks()
        };
        if running || clients.load(Ordering::Acquire) != 0 {
            idle_since = Instant::now();
        } else if idle_since.elapsed() >= options.idle {
            break;
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}

fn connection(mut socket: UnixStream, service: Arc<Mutex<Service>>, boundary: Arc<Boundary>) -> Result<()> {
    socket.set_nonblocking(false)?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut input = BufReader::new(socket.try_clone()?);
    while let Some(line) = wire::read(&mut input)? {
        let request = serde_json::from_slice::<Request>(&line);
        match request {
            Ok(Request::Subscribe) => {
                let rx = service.lock().unwrap_or_else(|e| e.into_inner()).runtime.subscribe();
                wire::write(&mut socket, &Reply { ok: Some(json!(null)), error: None })?;
                return subscribe(socket, rx);
            }
            Ok(Request::Watch { task }) => {
                let exists = service.lock().unwrap_or_else(|e| e.into_inner()).runtime.task(&task);
                if let Err(error) = exists {
                    wire::write(&mut socket, &failed(error))?;
                    continue;
                }
                let (lease, rx) = boundary.watch(&task);
                wire::write(&mut socket, &Reply { ok: Some(json!(null)), error: None })?;
                return watch(socket, lease, rx, task);
            }
            Ok(request)
                if matches!(
                    &request,
                    Request::Approval { .. }
                        | Request::Approvals { .. }
                        | Request::ApprovalStatus { .. }
                        | Request::ApprovalWait { .. }
                        | Request::WithdrawApproval { .. }
                        | Request::DecideApproval { .. }
                        | Request::Grants { .. }
                        | Request::RevokeGrant { .. }
                ) =>
            {
                let result = request.validate().and_then(|()| approval(&service, &boundary, request));
                wire::write(&mut socket, &reply(result))?;
            }
            request => {
                let result = request.map_err(anyhow::Error::from).and_then(|request| {
                    request.validate()?;
                    service.lock().unwrap_or_else(|e| e.into_inner()).execute(request)
                });
                wire::write(&mut socket, &reply(result))?;
            }
        }
    }
    Ok(())
}

fn reply(result: Result<Value>) -> Reply {
    match result {
        Ok(v) => Reply { ok: Some(v), error: None },
        Err(e) => failed(e),
    }
}

fn failed(error: anyhow::Error) -> Reply {
    Reply { ok: None, error: Some(format!("{error:#}")) }
}

/// Approval requests do not take the service lock while they may hold for a
/// decision; the task record is read briefly and the queue does the waiting.
fn approval(service: &Mutex<Service>, boundary: &Arc<Boundary>, request: Request) -> Result<Value> {
    match request {
        Request::Approval { task, ask } => {
            let record = service.lock().unwrap_or_else(|e| e.into_inner()).runtime.task(&task)?;
            let project = record.spec.cwd.to_string_lossy().into_owned();
            let incoming = ask.into_incoming(task, project, record.session_id.clone());
            Ok(serde_json::to_value(boundary.submit(incoming)?)?)
        }
        Request::Approvals { task } => Ok(serde_json::to_value(boundary.pending(&task)?)?),
        Request::ApprovalStatus { task, id } => Ok(serde_json::to_value(boundary.status(&task, &id)?)?),
        Request::ApprovalWait { task, id, wait_ms } => {
            let wait = Duration::from_millis(wait_ms.min(300_000));
            Ok(serde_json::to_value(boundary.wait(&task, &id, wait)?)?)
        }
        Request::WithdrawApproval { task, id } => Ok(serde_json::to_value(boundary.withdraw(&task, &id)?)?),
        Request::DecideApproval { id, decision } => Ok(serde_json::to_value(boundary.decide(&id, decision)?)?),
        Request::Grants { task } => Ok(serde_json::to_value(boundary.grants(&task)?)?),
        Request::RevokeGrant { task, id } => {
            boundary.revoke(&task, &id)?;
            Ok(json!(null))
        }
        _ => unreachable!("approval is called only for approval requests"),
    }
}

/// Stream this task's approval changes until the window goes away. The lease
/// lives exactly as long as the connection, so a dropped window detaches.
fn watch(
    mut socket: UnixStream,
    _lease: crate::approvals::Lease,
    mut rx: futures::channel::mpsc::UnboundedReceiver<Approval>,
    task: String,
) -> Result<()> {
    socket.set_nonblocking(true)?;
    loop {
        let mut byte = [0];
        match socket.read(&mut byte) {
            Ok(0) => return Ok(()),
            Ok(_) => bail!("a watch connection takes no further requests"),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e).context("reading the watch connection"),
        }
        while let Ok(approval) = rx.try_recv() {
            if approval.task == task
                && let Err(error) = wire::write(&mut socket, &approval)
            {
                return match error.downcast_ref::<std::io::Error>().map(std::io::Error::kind) {
                    Some(std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset) => Ok(()),
                    _ => Err(error),
                };
            }
        }
        std::thread::sleep(POLL);
    }
}

fn subscribe(mut socket: UnixStream, mut rx: futures::channel::mpsc::UnboundedReceiver<crate::Notice>) -> Result<()> {
    socket.set_nonblocking(true)?;
    loop {
        let mut byte = [0];
        match socket.read(&mut byte) {
            Ok(0) => return Ok(()),
            Ok(_) => bail!("subscription takes no further requests"),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e).context("reading subscription"),
        }
        while let Ok(notice) = rx.try_recv() {
            wire::write(&mut socket, &notice)?;
        }
        std::thread::sleep(POLL);
    }
}

impl Service {
    fn execute(&mut self, request: Request) -> Result<Value> {
        match request {
            Request::Create { spec } => {
                let id = self.runtime.create(spec)?;
                Ok(serde_json::to_value(self.runtime.task(&id)?)?)
            }
            Request::Start { id } => {
                self.launch(&id, false)?;
                Ok(serde_json::to_value(self.runtime.task(&id)?)?)
            }
            Request::Resume { id } => {
                self.launch(&id, true)?;
                Ok(serde_json::to_value(self.runtime.task(&id)?)?)
            }
            Request::Stop { id } => {
                self.runtime.stop(&id)?;
                self.boundary.clear_task(&id)?;
                Ok(json!(null))
            }
            Request::Destroy { id } => {
                self.runtime.destroy(&id)?;
                self.boundary.clear_task(&id)?;
                Ok(json!(null))
            }
            Request::Task { id } => Ok(serde_json::to_value(self.runtime.task(&id)?)?),
            Request::Tasks => Ok(serde_json::to_value(self.runtime.tasks()?)?),
            Request::Run { spec } => Ok(serde_json::to_value(self.run(spec)?)?),
            Request::Session { id, session_id } => {
                self.runtime.session(&id, &session_id)?;
                Ok(json!(null))
            }
            Request::Subscribe
            | Request::Watch { .. }
            | Request::Approval { .. }
            | Request::Approvals { .. }
            | Request::ApprovalStatus { .. }
            | Request::ApprovalWait { .. }
            | Request::WithdrawApproval { .. }
            | Request::DecideApproval { .. }
            | Request::Grants { .. }
            | Request::RevokeGrant { .. } => unreachable!("handled before service dispatch"),
        }
    }

    fn run(&mut self, spec: Spec) -> Result<RunResult> {
        let tasks = self.runtime.tasks()?;
        let fits = |t: &&Task| t.backend == "local" && t.spec == spec;
        if let Some(task) = tasks.iter().filter(fits).find(|t| t.state == State::Running) {
            return Ok(RunResult { task: task.clone(), started: false });
        }
        let id = match tasks.iter().filter(fits).find(|t| t.resumable() || t.startable()) {
            Some(task) => task.id.clone(),
            None => self.runtime.create(spec)?,
        };
        let resume = self.runtime.task(&id)?.resumable();
        self.launch(&id, resume)?;
        Ok(RunResult { task: self.runtime.task(&id)?, started: true })
    }

    fn launch(&mut self, id: &str, resume: bool) -> Result<()> {
        // A run starts with no items or grants from an earlier one; project
        // rules are the user's and live outside the task.
        self.boundary.clear_task(id)?;
        if resume {
            self.runtime.resume(id)?;
        } else {
            self.runtime.start(id)?;
        }
        Ok(())
    }
}

struct Connected(Arc<AtomicUsize>);
impl Drop for Connected {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
struct SocketPath(PathBuf);
impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
