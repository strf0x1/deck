//! A detached task supervisor. It owns the liveness lock and unchanged ACP
//! relay; a separate control socket permits adoption without attaching ACP.
//! The agent runs through the child lifeline, so a hard holder crash still
//! kills its agent and tools. Only deckd writes lifecycle records.

use crate::lifecycle::Holder;
use crate::local_guard::GuardedLocal;
use crate::record::{RecordLock, Store};
use crate::{State, relay::Relay, wire};
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub(crate) const WAIT: Duration = Duration::from_secs(5);
/// How long deckd waits for a new holder to report. It covers three cold
/// execs (holder, child helper, agent), and the first run of a freshly
/// linked binary on macOS can take seconds, so it outlasts the helper's own
/// startup wait and never shortens a successful start.
pub(crate) const STARTUP: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Identity {
    pub holder: Holder,
    pub agent: Option<u32>,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Request {
    Identify,
    Stop { grace_ms: u64 },
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Reply {
    Identity(Identity),
    Stopped(String),
}

pub(crate) fn control(socket: &Path) -> PathBuf {
    socket.with_extension("holder")
}

/// Internal entry point. `fd` is the liveness lock inherited at exec; it must
/// never be supplied by a caller other than the detached backend.
pub fn run(data: &Path, id: &str, socket: &Path, fd: RawFd) -> Result<()> {
    // SAFETY: the internal launcher passes one owned, open lock descriptor.
    // Restore CLOEXEC before starting threads or children so they cannot hold it.
    if fd < 3 {
        bail!("holder needs a separate lock descriptor");
    }
    // SAFETY: fcntl validates the descriptor without dereferencing pointers.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error()).context("protecting holder lock descriptor");
    }
    // SAFETY: the internal launcher transferred this now-validated descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    let _lock = RecordLock::held(file);
    let store = Store::open(data)?;
    let task = store.observe(id)?;
    let (exited, exit) = mpsc::channel();
    let mut instance = match (GuardedLocal { executable: std::env::current_exe()? }).start(
        &task.spec,
        Box::new(move |reason| {
            let _ = exited.send(reason);
        }),
        |agent| {
            remember(
                data,
                id,
                &Identity { holder: Holder { pid: std::process::id(), socket: socket.into() }, agent: Some(agent) },
            )
        },
    ) {
        Ok(instance) => instance,
        Err(error) => {
            report(Err(format!("{error:#}")))?;
            return Err(error);
        }
    };
    let endpoint = control(socket);
    // A free task lock proves the old holder is gone. Startup reconciliation
    // already confirmed its agent group stopped before allowing this launch.
    for path in [socket, endpoint.as_path()] {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    let listener = UnixListener::bind(&endpoint)?;
    let _control = SocketPath(endpoint);
    listener.set_nonblocking(true)?;
    let relay = Relay::serve(socket, instance.attach()?)?;
    let identity =
        Identity { holder: Holder { pid: std::process::id(), socket: socket.into() }, agent: instance.pid() };
    report(Ok(identity.clone()))?;

    // Before Started is durable, deckd is still launching this process. If it
    // dies in that gap, do not leave an unrecorded agent holding the task.
    let deadline = Instant::now() + WAIT;
    loop {
        let task = store.observe(id)?;
        if task.state == State::Running && task.holder.as_ref() == Some(&identity.holder) {
            break;
        }
        if Instant::now() >= deadline {
            bail!("deckd did not record holder startup");
        }
        std::thread::sleep(POLL);
    }
    loop {
        if exit.try_recv().is_ok() {
            return Ok(());
        }
        match listener.accept() {
            Ok((socket, _)) => {
                if let Some(grace) = request(socket, &identity)? {
                    let (mut socket, grace) = grace;
                    // Reserve ACP through termination. Closing a session is best
                    // effort; an unresponsive agent must still be stopped.
                    let closing = relay.take_over()?;
                    if let Some(session) = store.observe(id)?.session_id {
                        match deck_acp::shutdown::close_session(&closing, &session, Duration::from_millis(500)) {
                            Ok(_) => {}
                            Err(error) => eprintln!("closing task {id} session: {error:#}"),
                        }
                    }
                    let stopped = instance
                        .stop(grace)
                        .and_then(|reason| {
                            if let Some(pid) = identity.agent {
                                crate::reconcile::wait_for_group(pid)?;
                            }
                            Ok(Reply::Stopped(reason))
                        })
                        .map_err(|e| format!("{e:#}"));
                    if stopped.is_ok() {
                        drop(relay);
                        wire::write(&mut socket, &stopped)?;
                        return Ok(());
                    }
                    wire::write(&mut socket, &stopped)?;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                crate::wait::readable(listener.as_raw_fd(), POLL)?;
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionAborted) => {}
            Err(e) => return Err(e).context("accepting holder control"),
        }
    }
}

fn report(result: std::result::Result<Identity, String>) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &result)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn request(mut socket: UnixStream, identity: &Identity) -> Result<Option<(UnixStream, Duration)>> {
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    socket.set_write_timeout(Some(Duration::from_secs(1)))?;
    let result = (|| -> Result<Request> {
        let line = wire::read(&mut BufReader::new(socket.try_clone()?))?.context("empty holder request")?;
        Ok(serde_json::from_slice(&line)?)
    })();
    match result {
        Ok(Request::Identify) => {
            let reply: std::result::Result<Reply, String> = Ok(Reply::Identity(identity.clone()));
            // A client going away must never bring down the holder.
            let _ = wire::write(&mut socket, &reply);
        }
        Ok(Request::Stop { grace_ms }) => return Ok(Some((socket, Duration::from_millis(grace_ms.min(30_000))))),
        Err(_) => {}
    }
    Ok(None)
}

struct SocketPath(PathBuf);
impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Startup ownership exists before the agent may execute, including launches
/// whose deckd dies before it can append Started to the lifecycle log.
pub(crate) fn remembered(data: &Path, id: &str) -> Result<Option<Identity>> {
    match std::fs::read(data.join("tasks").join(id).join("holder.json")) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("reading startup holder identity"),
    }
}

fn remember(data: &Path, id: &str, identity: &Identity) -> Result<()> {
    let dir = data.join("tasks").join(id);
    let temp = dir.join("holder.json.tmp");
    let mut file = File::create(&temp)?;
    file.write_all(&serde_json::to_vec(identity)?)?;
    file.sync_all()?;
    std::fs::rename(temp, dir.join("holder.json"))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}
