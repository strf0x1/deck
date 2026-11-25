//! The host authority for B1 tasks. A service lock serializes control
//! operations over the existing Runtime; ACP relays never take that lock or
//! interpret JSON. The kernel data lock is held for the daemon's whole life.

use crate::approvals::{Approval, Boundary};
use crate::broker::{Broker, Refusal};
use crate::credentials::{Store, revoked::Revocation};
use crate::detached::Detached;
use crate::paths::Paths;
use crate::settings::file::{Saved, Snapshot};
use crate::wire::{self, Reply, Request, RunResult};
use crate::{Runtime, Spec, State, Task};
use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::{BufReader, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

mod control;
mod control_create;
mod control_integrate;
mod coordinator;

pub const IDLE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);

/// What this process is, for the compatibility handshake. The executable is
/// captured at startup: after a rebuild, `current_exe` can name a file that
/// was replaced, while this path still resolves to the binary now on disk.
struct Host {
    executable: PathBuf,
    started: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |age| u64::try_from(age.as_millis()).unwrap_or(u64::MAX))
}

/// The handshake reply, including whether the binary on disk is newer than
/// this process: a `cargo build` does not replace a running daemon.
fn hello(host: &Host) -> Value {
    let started = std::time::UNIX_EPOCH + Duration::from_millis(host.started);
    let stale = host.executable.metadata().and_then(|meta| meta.modified()).is_ok_and(|modified| modified > started);
    json!(wire::Hello {
        protocol: wire::PROTOCOL,
        version: env!("CARGO_PKG_VERSION").into(),
        pid: std::process::id(),
        executable: host.executable.clone(),
        started: host.started,
        stale,
    })
}

pub struct Options {
    pub data: PathBuf,
    pub paths: Paths,
    pub idle: Duration,
}

struct Service {
    data: PathBuf,
    runtime: Runtime,
    projects: crate::project::Projects,
    boundary: Arc<Boundary>,
    broker: Arc<Broker>,
    settings: crate::settings::file::Store,
    /// Workers whose coordinator brief is not delivered yet (deck#152).
    briefs: Arc<Mutex<std::collections::HashSet<String>>>,
}

/// Run until there are no running tasks and no control clients for `idle`.
pub fn serve(options: Options) -> Result<()> {
    std::fs::create_dir_all(&options.data)?;
    // A credential file that other users can read is refused before anything
    // else starts (ADR 0004 §4.2), so the daemon never serves from it.
    let store = Store::from_env()?;
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
    // Every task picks its backend (ADR 0016). The local one is always
    // here; a configured VM is the default for a task that names none.
    let local =
        Detached { executable: std::env::current_exe()?, data: options.data.clone(), paths: options.paths.clone() };
    let mut backends: Vec<Box<dyn crate::Backend>> = vec![Box::new(local)];
    if std::env::var_os(crate::vm::IMAGE_ENV).is_some() {
        backends.insert(0, Box::new(crate::vm::Vm::from_env(&options.data, options.paths.clone())?));
    }
    let runtime = Runtime::open_with(&options.data, backends)?;
    let (wake, mut events) = UnixStream::pair()?;
    wake.set_nonblocking(true)?;
    events.set_nonblocking(true)?;
    let wake = Arc::new(wake);
    let mut notices = runtime.subscribe();
    let notice_wake = wake.clone();
    std::thread::spawn(move || {
        while futures::executor::block_on(notices.recv()).is_ok() {
            wake_loop(&notice_wake);
        }
    });
    let projects = crate::project::Projects::open(&options.data)?;
    let boundary = Boundary::open(&options.data)?;
    let broker = Arc::new(Broker::open(&options.data, store)?);
    eprintln!("deckd: credentials come from the {}", broker.describe());
    let settings = crate::settings::file::Store::open(&options.data, crate::settings::Env::from_process())?;
    for note in settings.snapshot()?.overrides {
        eprintln!("deckd: {note} (settings in {})", settings.path().display());
    }
    let service = Arc::new(Mutex::new(Service {
        data: options.data.clone(),
        runtime,
        projects,
        boundary: boundary.clone(),
        broker: broker.clone(),
        settings,
        briefs: Arc::default(),
    }));
    let reader = Arc::new(crate::pull_request::Reader::default());
    let clients = Arc::new(AtomicUsize::new(0));
    let host = Arc::new(Host {
        executable: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("deckd")),
        started: now_ms(),
    });
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut idle_since = Instant::now();
    let mut was_busy = false;
    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        match listener.accept() {
            Ok((socket, _)) => {
                clients.fetch_add(1, Ordering::AcqRel);
                let client_count = clients.clone();
                let service = service.clone();
                let boundary = boundary.clone();
                let broker = broker.clone();
                let reader = reader.clone();
                let host = host.clone();
                let shutdown = shutdown.clone();
                let wake = wake.clone();
                std::thread::spawn(move || {
                    let _connected = Connected(client_count, wake.clone());
                    if let Err(e) = connection(socket, service, boundary, broker, reader, host, (shutdown, wake)) {
                        eprintln!("deckd: client: {e:#}");
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            // A peer may connect and close before the accept; macOS reports
            // that as ECONNABORTED. It is not a reason to end the daemon.
            Err(e) if matches!(e.kind(), std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionAborted) => {}
            Err(e) => return Err(e).context("accepting daemon client"),
        }
        let running = {
            let service = service.lock().unwrap_or_else(|e| e.into_inner());
            service.runtime.has_live_tasks() || !service.briefs.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
        };
        let busy = running || clients.load(Ordering::Acquire) != 0;
        if busy || was_busy {
            idle_since = Instant::now();
        } else if idle_since.elapsed() >= options.idle {
            break;
        }
        was_busy = busy;
        let timeout = if busy {
            Duration::from_secs(1)
        } else {
            options.idle.saturating_sub(idle_since.elapsed()).min(Duration::from_secs(1))
        };
        wait_for_activity(&listener, &events, timeout)?;
        let mut bytes = [0; 256];
        let _ = events.read(&mut bytes);
    }
    Ok(())
}

fn connection(
    mut socket: UnixStream,
    service: Arc<Mutex<Service>>,
    boundary: Arc<Boundary>,
    broker: Arc<Broker>,
    reader: Arc<crate::pull_request::Reader>,
    host: Arc<Host>,
    shutdown: (Arc<std::sync::atomic::AtomicBool>, Arc<UnixStream>),
) -> Result<()> {
    socket.set_nonblocking(false)?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut input = BufReader::new(socket.try_clone()?);
    while let Some(line) = wire::read(&mut input)? {
        let request = serde_json::from_slice::<Request>(&line);
        match request {
            Ok(Request::Hello) => wire::write(&mut socket, &reply(Ok(hello(&host))))?,
            Ok(Request::Shutdown) => {
                // Tasks live in their holders, so exiting ends no agent; the
                // next daemon adopts them (docs/deckd.md).
                wire::write(&mut socket, &reply(Ok(json!(null))))?;
                shutdown.0.store(true, Ordering::Release);
                wake_loop(&shutdown.1);
            }
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
            // A credential read may wait on the store (a Keychain dialog), so
            // it holds the service lock only to read the task, never across
            // the read.
            Ok(request @ Request::Credential { .. }) => {
                let result = request.validate().and_then(|()| match request {
                    Request::Credential { task, name } => credential(&service, &broker, &task, &name),
                    _ => unreachable!("matched a credential request"),
                });
                wire::write(&mut socket, &reply(result))?;
            }
            Ok(request @ Request::CredentialWatch { .. }) => {
                if let Err(error) = request.validate() {
                    wire::write(&mut socket, &failed(error))?;
                    continue;
                }
                let Request::CredentialWatch { task } = request else { unreachable!("matched a watch request") };
                let names = {
                    let service = service.lock().unwrap_or_else(|e| e.into_inner());
                    crate::vm::credential_set(&service.data, &task).unwrap_or_default()
                };
                let (snapshot, rx) = broker.watch(names);
                wire::write(&mut socket, &Reply { ok: Some(json!({ "revoked": snapshot })), error: None })?;
                return credential_watch(socket, rx);
            }
            // A prompt waits for the turn to start and a wait for a change:
            // neither may hold the service lock meanwhile.
            Ok(request @ (Request::Prompt { .. } | Request::Wait { .. })) => {
                let runtime = service.lock().unwrap_or_else(|e| e.into_inner()).runtime.clone();
                let result = request.validate().and_then(|()| match request {
                    Request::Prompt { task, text, from } => {
                        Ok(serde_json::to_value(crate::daemon_turns::prompt(&runtime, &task, &text, from.as_deref())?)?)
                    }
                    Request::Wait { task, after_turn, wait_ms } => {
                        Ok(serde_json::to_value(crate::daemon_turns::wait(&runtime, &task, after_turn, wait_ms)?)?)
                    }
                    _ => unreachable!("matched a turn request"),
                });
                wire::write(&mut socket, &reply(result))?;
            }
            // A coordinator's operation may wait (a `wait`, a prompt's turn
            // start), so it takes the service lock only for what it reads.
            Ok(request @ Request::Control { .. }) => {
                let result = request.validate().and_then(|()| match request {
                    Request::Control { task, request } => {
                        let (runtime, briefs) = {
                            let service = service.lock().unwrap_or_else(|e| e.into_inner());
                            (service.runtime.clone(), service.briefs.clone())
                        };
                        let cx = control::Context {
                            service: service.clone(),
                            boundary: boundary.clone(),
                            reader: reader.clone(),
                            runtime,
                            briefs,
                        };
                        Ok(serde_json::to_value(control::control(&cx, &task, request))?)
                    }
                    _ => unreachable!("matched a control request"),
                });
                wire::write(&mut socket, &reply(result))?;
            }
            Ok(request @ Request::PullState { .. }) => {
                let result = request.validate().and_then(|()| match request {
                    Request::PullState { task, refresh } => pull_request(&service, &reader, &task, refresh),
                    _ => unreachable!("matched a pull request read"),
                });
                wire::write(&mut socket, &reply(result))?;
            }
            Ok(
                request @ (Request::WorktreeStatus { .. }
                | Request::WorktreeDiff { .. }
                | Request::RemoveWorktree { .. }
                | Request::WorktreeSource { .. }),
            ) => {
                let result = request.validate().and_then(|()| worktree(&service, request));
                wire::write(&mut socket, &reply(result))?;
            }
            Ok(request @ Request::Credentials { .. }) => {
                let result = request.validate().and_then(|()| match request {
                    Request::Credentials { task } => {
                        let names = {
                            let service = service.lock().unwrap_or_else(|e| e.into_inner());
                            crate::vm::credential_set(&service.data, &task).unwrap_or_default()
                        };
                        Ok(serde_json::to_value(broker.list_names(&names))?)
                    }
                    _ => unreachable!("matched a credentials request"),
                });
                wire::write(&mut socket, &reply(result))?;
            }
            Ok(Request::ListCredentials) => {
                let result = broker.list().and_then(|credentials| {
                    Ok(serde_json::to_value(crate::broker::Listing { store: broker.describe(), credentials })?)
                });
                wire::write(&mut socket, &reply(result))?;
            }
            Ok(request @ Request::RetryCredential { .. }) => {
                let result = request.validate().and_then(|()| match request {
                    Request::RetryCredential { name } => Ok(json!(broker.retry(&name)?)),
                    _ => unreachable!("matched a retry request"),
                });
                wire::write(&mut socket, &reply(result))?;
            }
            Ok(request @ Request::RevokeCredential { .. }) => {
                let result = request.validate().and_then(|()| match request {
                    Request::RevokeCredential { name } => Ok(serde_json::to_value(broker.revoke(&name)?)?),
                    _ => unreachable!("matched a revoke request"),
                });
                wire::write(&mut socket, &reply(result))?;
            }
            Ok(request @ (Request::SetCredential { .. } | Request::DeleteCredential { .. })) => {
                // The store write happens here, off the service lock, like a
                // revoke: an OS store may wait on a dialog.
                let result = request.validate().and_then(|()| match request {
                    Request::SetCredential { name, value } => Ok(serde_json::to_value(broker.set(&name, &value)?)?),
                    Request::DeleteCredential { name } => Ok(json!(broker.delete(&name)?)),
                    _ => unreachable!("matched a store write"),
                });
                wire::write(&mut socket, &reply(result))?;
            }
            Ok(request @ Request::ListModels { .. }) => {
                // The fetch waits on the provider, so the lock is held only to
                // copy what the listing needs; the settings store re-reads its
                // file on every call.
                let (settings, data) = {
                    let service = service.lock().unwrap_or_else(|e| e.into_inner());
                    (service.settings.clone(), service.data.clone())
                };
                let result = request.validate().and_then(|()| match request {
                    Request::ListModels { provider, refresh } => {
                        let settings = settings.effective()?;
                        let Some(provider) = settings.provider(&provider).cloned() else {
                            bail!("no provider is named {provider:?}");
                        };
                        let key = match &provider.secret {
                            Some(name) => {
                                Some(broker.value(name).map_err(|refusal| anyhow::anyhow!(refusal.reply(name)))?)
                            }
                            None => None,
                        };
                        Ok(serde_json::to_value(crate::catalog::listing(&data, &provider, key.as_deref(), refresh)?)?)
                    }
                    _ => unreachable!("matched a models request"),
                });
                wire::write(&mut socket, &reply(result))?;
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
                        | Request::AccessGrants { .. }
                        | Request::GithubApprovals { .. }
                        | Request::RevokeAccessGrant { .. }
                ) =>
            {
                let result =
                    request.validate().and_then(|()| approval(&service, &boundary, request, || connected(&socket)));
                wire::write(&mut socket, &reply(result))?;
            }
            // A profile decision writes the project's rule file, which the
            // running gateways re-read; it takes the service lock only long
            // enough to name the project, exactly as an approval does.
            Ok(Request::WebProfile { task, project, destinations, revoke, .. }) => {
                // The project a rule belongs to is the task's, unless the
                // window names another (a shared data root, several windows).
                let project = match project {
                    Some(project) if !project.is_empty() => Some(project),
                    _ => service
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .runtime
                        .task(&task)
                        .ok()
                        .map(|record| record.spec.project_key()),
                };
                let changed = boundary.profile(project.as_deref(), &destinations, revoke);
                wire::write(&mut socket, &reply(changed.map(|changed| json!({ "changed": changed }))))?;
            }
            Ok(Request::GitHubProfile { task, project, repository, operations, revoke }) => {
                let record = service.lock().unwrap_or_else(|e| e.into_inner()).runtime.task(&task).ok();
                let project = match project {
                    Some(project) if !project.is_empty() => Some(project),
                    _ => record.as_ref().map(|record| record.spec.project_key()),
                };
                // A VM task's credential name is in its recorded policy; a
                // local task has no policy and keeps the development default.
                let credential = record
                    .as_ref()
                    .and_then(|record| record.spec.github.as_ref().map(|github| github.credential.clone()))
                    .or_else(|| std::env::var(crate::github::TOKEN_ENV).ok().filter(|name| !name.is_empty()))
                    .unwrap_or_else(|| crate::github::TOKEN.into());
                let changed =
                    boundary.github_profile(project.as_deref(), &repository, &operations, &credential, revoke);
                wire::write(&mut socket, &reply(changed.map(|changed| json!({ "changed": changed }))))?;
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

/// Whether the caller can still receive a reply, without blocking.
fn connected(socket: &UnixStream) -> bool {
    use std::os::fd::AsRawFd as _;

    // POLLOUT detects a closed recipient while preserving a peer's deliberate
    // write-half-close. Checking readable EOF would cancel clients which have
    // finished sending their request but are still waiting for its reply.
    let mut descriptor = libc::pollfd { fd: socket.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
    // SAFETY: descriptor names the socket borrowed for this call, and poll
    // receives one initialized descriptor with a zero timeout.
    let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
    if result < 0 {
        return std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted;
    }
    descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) == 0
}

/// Approval requests do not take the service lock while they may hold for a
/// decision; the task record is read briefly and the queue does the waiting.
fn approval(
    service: &Mutex<Service>,
    boundary: &Arc<Boundary>,
    request: Request,
    connected: impl Fn() -> bool,
) -> Result<Value> {
    match request {
        Request::Approval { task, ask } => {
            let record = service.lock().unwrap_or_else(|e| e.into_inner()).runtime.task(&task)?;
            let project = record.spec.project_key();
            // The recorded mode is the base; the project file is re-read so a
            // switch to `ask` governs the next operation without a restart.
            let approvals = record
                .spec
                .github
                .as_ref()
                .map(|policy| crate::github::live_approvals(&record.spec.cwd, policy.approvals));
            let incoming = ask.into_incoming(task, project, record.session_id.clone(), approvals);
            Ok(serde_json::to_value(boundary.submit_while(incoming, connected)?)?)
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
        Request::AccessGrants { task } => {
            let project = service.lock().unwrap_or_else(|e| e.into_inner()).runtime.task(&task)?.spec.project_key();
            Ok(serde_json::to_value(boundary.access_grants(&task, &project)?)?)
        }
        Request::GithubApprovals { task } => {
            let record = service.lock().unwrap_or_else(|e| e.into_inner()).runtime.task(&task)?;
            let mode = record
                .spec
                .github
                .as_ref()
                .map(|policy| crate::github::live_approvals(&record.spec.cwd, policy.approvals));
            Ok(serde_json::to_value(mode)?)
        }
        Request::RevokeAccessGrant { task, id } => {
            // Keep task creation serialized until the project revoke ends,
            // so a new task cannot be missed by the matching-grant removal.
            let service = service.lock().unwrap_or_else(|e| e.into_inner());
            let project = service.runtime.task(&task)?.spec.project_key();
            let peers: Vec<_> = service
                .runtime
                .tasks()?
                .into_iter()
                .filter(|record| record.spec.project_key() == project)
                .map(|record| record.id)
                .collect();
            boundary.revoke_access_grant(&task, &project, &peers, &id)?;
            Ok(json!(null))
        }
        _ => unreachable!("approval is called only for approval requests"),
    }
}

/// A task's publication and live pull request state. The record is read under
/// the service lock; the gateway read happens without it.
fn pull_request(
    service: &Mutex<Service>,
    reader: &crate::pull_request::Reader,
    task: &str,
    refresh: bool,
) -> Result<Value> {
    let (dir, publication, socket) = {
        let service = service.lock().unwrap_or_else(|e| e.into_inner());
        let record = service.runtime.task(task)?;
        let socket = (record.state == State::Running).then(|| crate::vm::github_socket(&service.data, task)).flatten();
        (service.data.join("tasks").join(task), record.publication, socket)
    };
    Ok(serde_json::to_value(reader.state(task, &dir, publication, socket, refresh))?)
}

/// Git inspection does not take the service lock while it reads a checkout.
fn worktree(service: &Mutex<Service>, request: Request) -> Result<Value> {
    if let Request::WorktreeSource { project } = request {
        let checkout = {
            let service = service.lock().unwrap_or_else(|error| error.into_inner());
            let project = service.projects.get(&project).context("Unknown project.")?;
            project.checkouts.first().cloned()
        };
        let preview =
            checkout.as_deref().map(|path| crate::worktree::SourcePreview::read(path, &project)).transpose()?.flatten();
        return Ok(serde_json::to_value(preview)?);
    }
    let runtime = service.lock().unwrap_or_else(|error| error.into_inner()).runtime.clone();
    let (id, diff) = match request {
        Request::RemoveWorktree { id } => return Ok(serde_json::to_value(runtime.remove_worktree(&id)?)?),
        Request::WorktreeStatus { id } => (id, false),
        Request::WorktreeDiff { id } => (id, true),
        _ => unreachable!("matched a worktree request"),
    };
    let task = runtime.task(&id)?;
    let workspace = task.workspace.as_ref().context("This task has no managed worktree.")?;
    Ok(if diff {
        serde_json::to_value(workspace.diff(&task)?)?
    } else {
        serde_json::to_value(workspace.status(&task))?
    })
}

/// One credential value for one gateway operation. The task must be running
/// and the name in its `vmm.json` set; the store is read without the service
/// lock. The reply carries the value and is never logged; the log line names
/// the task, the credential and the fingerprint.
fn credential(service: &Mutex<Service>, broker: &Broker, task: &str, name: &str) -> Result<Value> {
    let allowed = {
        let service = service.lock().unwrap_or_else(|e| e.into_inner());
        match service.runtime.task(task) {
            Ok(record) if record.state == State::Running => {
                crate::vm::credential_set(&service.data, task).unwrap_or_default()
            }
            _ => Vec::new(),
        }
    };
    match broker.fetch(task, name, &allowed) {
        Ok(fetched) => {
            eprintln!("deckd: credential {name} {} for task {task}", fetched.fingerprint);
            if fetched.first_use {
                let service = service.lock().unwrap_or_else(|e| e.into_inner());
                if let Err(error) = service.runtime.credential_used(task, name, &fetched.fingerprint) {
                    eprintln!("deckd: recording credential use for task {task}: {error:#}");
                }
            }
            Ok(json!({ "value": fetched.value, "fingerprint": fetched.fingerprint }))
        }
        Err(refusal) => {
            if let Refusal::Revoked(fingerprint) = &refusal {
                eprintln!("deckd: credential {name} {fingerprint} for task {task}: revoked");
            } else {
                eprintln!("deckd: credential {name} for task {task}: {}", refusal.code());
            }
            bail!(refusal.reply(name))
        }
    }
}

/// Push each revocation to a gateway until it goes away. The gateway reopens
/// the watch after a daemon restart and gets the snapshot again.
fn credential_watch(
    mut socket: UnixStream,
    mut rx: futures::channel::mpsc::UnboundedReceiver<Revocation>,
) -> Result<()> {
    socket.set_nonblocking(true)?;
    loop {
        let mut byte = [0];
        match socket.read(&mut byte) {
            Ok(0) => return Ok(()),
            Ok(_) => bail!("a credential watch takes no further requests"),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e).context("reading the credential watch"),
        }
        while let Ok(revocation) = rx.try_recv() {
            if let Err(error) = wire::write(&mut socket, &revocation) {
                return match error.downcast_ref::<std::io::Error>().map(std::io::Error::kind) {
                    Some(std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset) => Ok(()),
                    _ => Err(error),
                };
            }
        }
        std::thread::sleep(POLL);
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
            // A profile decision is handled beside the boundary, not here: it
            // writes the project's rule file, which the gateways re-read.
            Request::WebProfile { .. } => unreachable!("a profile decision is routed to deckd's boundary"),
            Request::GitHubProfile { .. } => unreachable!("a GitHub profile decision is routed to deckd's boundary"),
            Request::Create { mut spec } => {
                self.coordinate(&mut spec)?;
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
            Request::ChooseWorktreeSource { project, path } => {
                if !project.starts_with("git:") || self.projects.get(&project).is_none() {
                    bail!("This project has no Git checkout to replace.");
                }
                if self.managed_workspace_at(&path)?.is_some() {
                    bail!("Choose a project checkout. This path belongs to a managed worktree.");
                }
                let preview = crate::worktree::SourcePreview::read(&path, &project)?
                    .context("Choose a Git checkout with a committed starting point.")?;
                self.projects.select_checkout(&project, &preview.path)?;
                Ok(serde_json::to_value(preview)?)
            }
            Request::Tasks => Ok(serde_json::to_value(self.runtime.tasks()?)?),
            Request::Projects => Ok(serde_json::to_value(self.projects.list())?),
            Request::OpenProject { path, argv } => {
                let task = self.managed_workspace_at(&path)?;
                let source = task.and_then(|task| task.workspace).map_or(path, |workspace| workspace.source);
                Ok(serde_json::to_value(self.projects.open_project(&source, argv)?)?)
            }
            Request::ForgetProject { id } => {
                self.projects.forget(&id)?;
                Ok(json!(null))
            }
            Request::Run { spec } if spec.kind == crate::TaskKind::Coordinator => {
                Ok(serde_json::to_value(self.run_coordinator(spec)?)?)
            }
            Request::Run { spec } => Ok(serde_json::to_value(self.run(spec)?)?),
            Request::CoordinatorPolicy { task, set } => Ok(serde_json::to_value(self.coordinator_policy(&task, set)?)?),
            Request::SessionRefused { id, error } => {
                self.runtime.session_refused(&id, &error)?;
                self.boundary.clear_task(&id)?;
                Ok(serde_json::to_value(self.runtime.task(&id)?)?)
            }
            Request::Session { id, session_id } => {
                self.runtime.session(&id, &session_id)?;
                Ok(json!(null))
            }
            Request::ModelUsage { task, requests, tokens } => {
                self.runtime.usage(&task, requests, tokens)?;
                Ok(json!(null))
            }
            Request::PullState { .. } => unreachable!("handled before service dispatch"),
            Request::WorktreeStatus { .. }
            | Request::WorktreeDiff { .. }
            | Request::RemoveWorktree { .. }
            | Request::WorktreeSource { .. } => unreachable!("handled before service dispatch"),
            Request::Publication { task, publication } => {
                self.runtime.publication(&task, publication)?;
                Ok(json!(null))
            }
            Request::TurnChanged { id } => {
                self.runtime.turn_changed(&id)?;
                Ok(json!(null))
            }
            Request::Prompts { task } => Ok(serde_json::to_value(self.runtime.prompts(&task)?)?),
            Request::Prompt { .. } | Request::Wait { .. } | Request::Control { .. } => {
                unreachable!("handled before service dispatch")
            }
            Request::Seen { id, turn } => {
                self.runtime.seen(&id, turn)?;
                Ok(json!(null))
            }
            Request::Mark { id, done } => {
                self.runtime.mark(&id, done)?;
                Ok(json!(null))
            }
            Request::Title { id, title } => {
                self.runtime.title(&id, &title)?;
                Ok(json!(null))
            }
            Request::SetBudget { task, requests, tokens } => {
                self.runtime.set_budget(&task, requests, tokens)?;
                Ok(json!(null))
            }
            Request::Settings => Ok(serde_json::to_value(self.offered(self.settings.snapshot()?))?),
            Request::SetSettings { settings, revision } => {
                let saved = match self.settings.replace(settings, revision)? {
                    Saved::Saved(snapshot) => Saved::Saved(self.offered(snapshot)),
                    Saved::Stale(snapshot) => Saved::Stale(self.offered(snapshot)),
                };
                Ok(serde_json::to_value(saved)?)
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
            | Request::RevokeGrant { .. }
            | Request::AccessGrants { .. }
            | Request::GithubApprovals { .. }
            | Request::RevokeAccessGrant { .. }
            | Request::Credential { .. }
            | Request::CredentialWatch { .. }
            | Request::Credentials { .. }
            | Request::ListCredentials
            | Request::ListModels { .. }
            | Request::RetryCredential { .. }
            | Request::RevokeCredential { .. }
            | Request::SetCredential { .. }
            | Request::DeleteCredential { .. }
            | Request::Hello
            | Request::Shutdown => unreachable!("handled before service dispatch"),
        }
    }

    /// A settings snapshot with the backends a new session may run on.
    fn offered(&self, mut snapshot: Snapshot) -> Snapshot {
        snapshot.backends = self.runtime.backends().into_iter().map(str::to_owned).collect();
        snapshot
    }

    fn run(&mut self, spec: Spec) -> Result<RunResult> {
        if let Some(task) = self.managed_workspace_at(&spec.cwd)? {
            if task.state == State::Destroyed {
                anyhow::bail!(
                    "This worktree belongs to a destroyed task. The branch remains in the source repository."
                );
            }
            if task.state == State::Running {
                return Ok(RunResult { task, started: false });
            }
            self.launch(&task.id, task.resumable())?;
            return Ok(RunResult { task: self.runtime.task(&task.id)?, started: true });
        }
        let tasks = self.runtime.tasks()?;
        let backend = spec.backend.clone().unwrap_or_else(|| self.runtime.backend().to_owned());
        let fits = |t: &&Task| t.integration.is_none() && t.backend == backend && t.spec.same_command(&spec);
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

    fn managed_workspace_at(&self, path: &std::path::Path) -> Result<Option<Task>> {
        let Ok(path) = path.canonicalize() else { return Ok(None) };
        Ok(self
            .runtime
            .tasks()?
            .into_iter()
            .find(|task| task.workspace.as_ref().is_some_and(|workspace| workspace.contains_path(task, &path))))
    }

    fn launch(&mut self, id: &str, resume: bool) -> Result<()> {
        // A run starts with no items or grants from an earlier one; project
        // rules are the user's and live outside the task.
        self.boundary.clear_task(id)?;
        self.broker.forget_task(id);
        if resume {
            self.runtime.resume(id)?;
        } else {
            self.runtime.start(id)?;
        }
        Ok(())
    }
}

fn wake_loop(mut wake: &UnixStream) {
    let _ = wake.write(&[1]);
}

fn wait_for_activity(listener: &UnixListener, wake: &UnixStream, timeout: Duration) -> Result<()> {
    let mut fds = [
        libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: wake.as_raw_fd(), events: libc::POLLIN, revents: 0 },
    ];
    let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: both borrowed descriptors and the poll array remain live.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
    if ready < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
        return Err(std::io::Error::last_os_error()).context("waiting for daemon activity");
    }
    Ok(())
}

struct Connected(Arc<AtomicUsize>, Arc<UnixStream>);
impl Drop for Connected {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
        wake_loop(&self.1);
    }
}
struct SocketPath(PathBuf);
impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::connected;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;

    #[test]
    fn a_write_half_close_keeps_the_reply_recipient_connected() {
        let (server, client) = UnixStream::pair().unwrap();
        assert!(connected(&server));
        client.shutdown(Shutdown::Write).unwrap();
        assert!(connected(&server));
        drop(client);
        assert!(!connected(&server));
    }
}
