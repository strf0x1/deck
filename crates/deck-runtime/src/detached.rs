//! The daemon's backend for detached holders. Start transfers the task lock
//! through exec, adoption verifies the holder over its private control socket,
//! and supervision observes the lock and process groups without signalling a
//! recorded pid. Dropping a proxy only detaches from supervision.

use crate::backend::{Backend, Exited, Instance, Stream};
use crate::holder::{self, Identity, Reply, Request, STARTUP, WAIT};
use crate::lifecycle::{Holder, Task};
use crate::paths::Paths;
use crate::record::{RecordLock, Store};
use crate::{reconcile, wire};
use anyhow::{Context as _, Result, bail};
use std::io::BufReader;
use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

pub(crate) struct Detached {
    pub executable: PathBuf,
    pub data: PathBuf,
    pub paths: Paths,
}

impl Backend for Detached {
    fn name(&self) -> &'static str {
        "local"
    }

    fn start(&mut self, task: &Task, lock: RecordLock, exited: Exited) -> Result<Box<dyn Instance>> {
        self.reconcile(task)?;
        let file = lock.transfer().context("missing task lock")?;
        let fd = file.as_raw_fd();
        let socket = self.paths.task(&task.id);
        let mut command = Command::new(&self.executable);
        command
            .arg("--holder")
            .arg(&self.data)
            .arg(&task.id)
            .arg(&socket)
            .arg(fd.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // SAFETY: setsid and fcntl are async-signal-safe. Only the holder
        // inherits this descriptor; the parent's copy remains CLOEXEC.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 || libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().context("launching task holder")?;
        drop(file); // close, without unlocking the child's shared flock
        let stdout = child.stdout.take().context("holder startup pipe")?;
        let pid = child.id();
        let (ready, result) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let startup = (|| -> Result<Identity> {
                let line = wire::read(&mut BufReader::new(stdout))?.context("holder did not report startup")?;
                let outcome: std::result::Result<Identity, String> = serde_json::from_slice(&line)?;
                outcome.map_err(anyhow::Error::msg)
            })();
            let _ = ready.send(startup);
        });
        let identity = match result.recv_timeout(STARTUP) {
            Ok(Ok(identity)) if identity.holder.pid == pid && identity.holder.socket == socket => identity,
            outcome => {
                let _ = child.kill();
                let _ = child.wait();
                self.reconcile(task)?;
                return match outcome {
                    Ok(Err(e)) => Err(e),
                    _ => anyhow::bail!("holder startup failed or timed out"),
                };
            }
        };
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(Box::new(Proxy::watch(self.data.clone(), task.id.clone(), identity, exited)))
    }

    fn reconcile(&mut self, task: &Task) -> Result<()> {
        if let Some(identity) = holder::remembered(&self.data, &task.id)? {
            if let Some(pid) = identity.agent {
                reconcile::wait_for_group(pid)?;
            }
            reconcile::wait_for_group(identity.holder.pid)?;
            cleanup(&self.data, &task.id, &identity);
        }
        if let Some(pid) = task.pid {
            reconcile::wait_for_group(pid)?;
        }
        if let Some(holder) = &task.holder {
            reconcile::wait_for_group(holder.pid)?;
            cleanup(&self.data, &task.id, &Identity { holder: holder.clone(), agent: task.pid });
        }
        Ok(())
    }

    fn adopt(&mut self, task: &Task, exited: Exited) -> Result<Option<Box<dyn Instance>>> {
        let holder = task.holder.as_ref().context("running task has no detached holder")?;
        let identity = match exchange(holder, &Request::Identify, WAIT)? {
            Reply::Identity(identity) if identity.holder == *holder && identity.agent == task.pid => identity,
            _ => bail!("task {}: holder identity did not match its record", task.id),
        };
        Ok(Some(Box::new(Proxy::watch(self.data.clone(), task.id.clone(), identity, exited))))
    }

    fn destroy(&mut self, _task: &Task) -> Result<()> {
        Ok(())
    }
}

struct Proxy {
    data: PathBuf,
    id: String,
    identity: Identity,
    done: Arc<AtomicBool>,
}

impl Proxy {
    fn watch(data: PathBuf, id: String, identity: Identity, exited: Exited) -> Self {
        let proxy = Self { data, id, identity, done: Arc::new(AtomicBool::new(false)) };
        let data = proxy.data.clone();
        let id = proxy.id.clone();
        let identity = proxy.identity.clone();
        let done = proxy.done.clone();
        std::thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                if let Ok(Some(lock)) = stopped(&data, &id, &identity) {
                    // Cancellation is checked while holding the task lock, so
                    // an old watcher cannot erase a newer launch's ownership.
                    if done.load(Ordering::Acquire) {
                        return;
                    }
                    cleanup(&data, &id, &identity);
                    drop(lock);
                    exited("detached holder exited".into());
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        proxy
    }
}

impl Instance for Proxy {
    fn pid(&self) -> Option<u32> {
        self.identity.agent
    }
    fn holder(&self) -> Option<Holder> {
        Some(self.identity.holder.clone())
    }
    fn attach(&mut self) -> Result<Stream> {
        let socket = UnixStream::connect(&self.identity.holder.socket)?;
        Ok(Stream { reader: Box::new(BufReader::new(socket.try_clone()?)), writer: Box::new(socket) })
    }
    fn stop(&mut self, grace: Duration) -> Result<String> {
        if let Some(_lock) = stopped(&self.data, &self.id, &self.identity)? {
            cleanup(&self.data, &self.id, &self.identity);
            return Ok("detached holder already exited".into());
        }
        let request = Request::Stop { grace_ms: grace.as_millis().min(30_000) as u64 };
        let reason = match exchange(&self.identity.holder, &request, grace + WAIT)? {
            Reply::Stopped(reason) => reason,
            _ => bail!("holder did not confirm agent stop"),
        };
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(_lock) = stopped(&self.data, &self.id, &self.identity)? {
                cleanup(&self.data, &self.id, &self.identity);
                break;
            }
            if Instant::now() >= deadline {
                bail!("holder shutdown was not confirmed");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(reason)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
    }
}

fn exchange(holder: &Holder, request: &Request, timeout: Duration) -> Result<Reply> {
    let mut socket = UnixStream::connect(holder::control(&holder.socket))?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;
    wire::write(&mut socket, request)?;
    let line = wire::read(&mut BufReader::new(socket))?.context("holder closed control connection")?;
    let reply: std::result::Result<Reply, String> = serde_json::from_slice(&line)?;
    reply.map_err(anyhow::Error::msg)
}

fn stopped(data: &std::path::Path, id: &str, identity: &Identity) -> Result<Option<RecordLock>> {
    let store = Store::open(data)?;
    let Some(lock) = store.lock(id)? else { return Ok(None) };
    if reconcile::group_running(identity.holder.pid)?
        || identity.agent.map(reconcile::group_running).transpose()?.unwrap_or(false)
    {
        return Ok(None);
    }
    Ok(Some(lock))
}

fn cleanup(data: &std::path::Path, id: &str, identity: &Identity) {
    let _ = std::fs::remove_file(&identity.holder.socket);
    let _ = std::fs::remove_file(holder::control(&identity.holder.socket));
    let _ = std::fs::remove_file(data.join("tasks").join(id).join("holder.json"));
}
