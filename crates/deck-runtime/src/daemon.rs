//! The host authority for B1 tasks. A service lock serializes control
//! operations over the existing Runtime; ACP relays never take that lock or
//! interpret JSON. The kernel data lock is held for the daemon's whole life.

use crate::local_guard::GuardedLocal;
use crate::paths::Paths;
use crate::relay::Relay;
use crate::wire::{self, Reply, Request, RunResult};
use crate::{Runtime, Spec, State, Task};
use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
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
    paths: Paths,
    relays: HashMap<String, Relay>,
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
    let backend = GuardedLocal { executable: std::env::current_exe()? };
    crate::reconcile::wait_for_children(&options.data)?;
    let runtime = Runtime::open(&options.data, backend)?;
    let service = Arc::new(Mutex::new(Service { runtime, paths: options.paths, relays: HashMap::new() }));
    let clients = Arc::new(AtomicUsize::new(0));
    let mut idle_since = Instant::now();
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                clients.fetch_add(1, Ordering::AcqRel);
                let client_count = clients.clone();
                let service = service.clone();
                std::thread::spawn(move || {
                    let _connected = Connected(client_count);
                    if let Err(e) = connection(socket, service) {
                        eprintln!("deckd: client: {e:#}");
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e).context("accepting daemon client"),
        }
        let running = {
            let mut service = service.lock().unwrap_or_else(|e| e.into_inner());
            let tasks = service.runtime.tasks()?;
            service.relays.retain(|id, _| tasks.iter().any(|t| t.id == *id && t.state == State::Running));
            tasks.iter().any(|t| t.state == State::Running)
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

fn connection(mut socket: UnixStream, service: Arc<Mutex<Service>>) -> Result<()> {
    socket.set_nonblocking(false)?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut input = BufReader::new(socket.try_clone()?);
    while let Some(line) = wire::read(&mut input)? {
        let request = serde_json::from_slice::<Request>(&line);
        if matches!(request, Ok(Request::Subscribe)) {
            let rx = service.lock().unwrap_or_else(|e| e.into_inner()).runtime.subscribe();
            wire::write(&mut socket, &Reply { ok: Some(json!(null)), error: None })?;
            return subscribe(socket, rx);
        }
        let result = request.map_err(anyhow::Error::from).and_then(|request| {
            request.validate()?;
            service.lock().unwrap_or_else(|e| e.into_inner()).execute(request)
        });
        let reply = match result {
            Ok(v) => Reply { ok: Some(v), error: None },
            Err(e) => Reply { ok: None, error: Some(format!("{e:#}")) },
        };
        wire::write(&mut socket, &reply)?;
    }
    Ok(())
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
                self.relays.remove(&id);
                Ok(json!(null))
            }
            Request::Destroy { id } => {
                self.runtime.destroy(&id)?;
                self.relays.remove(&id);
                Ok(json!(null))
            }
            Request::Task { id } => Ok(serde_json::to_value(self.runtime.task(&id)?)?),
            Request::Tasks => Ok(serde_json::to_value(self.runtime.tasks()?)?),
            Request::Run { spec } => Ok(serde_json::to_value(self.run(spec)?)?),
            Request::Session { id, session_id } => {
                self.runtime.session(&id, &session_id)?;
                Ok(json!(null))
            }
            Request::Subscribe => unreachable!("subscription is handled before service dispatch"),
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
        if resume {
            self.runtime.resume(id)?;
        } else {
            self.runtime.start(id)?;
        }
        self.relays.remove(id);
        let path = self.paths.task(id);
        let result = (|| {
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            Relay::serve(&path, self.runtime.attach(id)?)
        })();
        match result {
            Ok(relay) => {
                self.relays.insert(id.to_string(), relay);
                Ok(())
            }
            Err(e) => {
                let _ = self.runtime.stop(id);
                Err(e).context("starting task holder")
            }
        }
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
