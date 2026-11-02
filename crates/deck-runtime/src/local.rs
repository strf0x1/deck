//! The local backend: the agent is a child process on this machine, spoken
//! to over its stdio. It lives as long as the runtime that started it.
//!
//! The design choice: one lock guards the child, and only whoever holds it
//! and has not yet reaped the child may signal it. A watcher thread polls for
//! an exit nobody asked for; a stop marks the child as stopping first, so the
//! watcher stands down and the stop alone reaps it. A signal therefore never
//! reaches a pid that was reaped and reused. On Unix the agent leads its own
//! process group, and signals go to the group, so its tools stop with it.

use crate::backend::{Backend, Exited, Instance, Stream};
use crate::lifecycle::{Spec, Task};
use crate::record::RecordLock;
use anyhow::{Context as _, Result, anyhow, bail};
use std::io::BufReader;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How often the watcher and a stop look for an exit.
const POLL: Duration = Duration::from_millis(20);
/// How long a SIGKILL gets before the stop gives up on confirming.
const KILL_WAIT: Duration = Duration::from_secs(2);

pub struct Local;

impl Backend for Local {
    fn name(&self) -> &'static str {
        "local"
    }

    fn start(&mut self, task: &Task, lock: RecordLock, exited: Exited) -> Result<Box<dyn Instance>> {
        Self::spawn(&task.spec, exited, Some(lock))
    }

    fn destroy(&mut self, _spec: &Spec) -> Result<()> {
        Ok(())
    }
}

impl Local {
    pub(crate) fn spawn(spec: &Spec, exited: Exited, record_lock: Option<RecordLock>) -> Result<Box<dyn Instance>> {
        let (prog, args) = spec.argv.split_first().context("no agent command")?;
        let mut cmd = Command::new(prog);
        cmd.args(args).current_dir(&spec.cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd.spawn().with_context(|| format!("launching {prog}"))?;
        let stdin = child.stdin.take().context("agent stdin")?;
        let stdout = child.stdout.take().context("agent stdout")?;
        let pid = child.id();
        let proc = Arc::new(Mutex::new(Proc { child, exit: None, stopping: false }));
        watch(proc.clone(), exited);
        Ok(Box::new(Process { proc, pid, stdio: Some((stdout, stdin)), _lock: record_lock }))
    }
}

struct Proc {
    child: Child,
    /// How it ended, once reaped.
    exit: Option<String>,
    /// A stop has begun: the watcher must not reap or report.
    stopping: bool,
}

struct Process {
    _lock: Option<RecordLock>,
    proc: Arc<Mutex<Proc>>,
    pid: u32,
    stdio: Option<(ChildStdout, ChildStdin)>,
}

fn lock(proc: &Mutex<Proc>) -> MutexGuard<'_, Proc> {
    // A poisoned lock means a panic mid-poll; the child handle is still sound.
    proc.lock().unwrap_or_else(|e| e.into_inner())
}

fn watch(proc: Arc<Mutex<Proc>>, exited: Exited) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(POLL);
            let mut p = lock(&proc);
            if p.stopping {
                return;
            }
            match p.child.try_wait() {
                Ok(Some(status)) => {
                    let how = describe(status);
                    p.exit = Some(how.clone());
                    drop(p);
                    exited(how);
                    return;
                }
                Ok(None) => {}
                Err(_) => return,
            }
        }
    });
}

impl Process {
    /// Wait up to `within` for the child to end, reaping it if it does.
    fn reap(&self, within: Duration) -> Result<Option<String>> {
        let deadline = Instant::now() + within;
        loop {
            {
                let mut p = lock(&self.proc);
                if let Some(how) = &p.exit {
                    return Ok(Some(how.clone()));
                }
                if let Some(status) = p.child.try_wait().context("waiting for the agent")? {
                    let how = describe(status);
                    p.exit = Some(how.clone());
                    return Ok(Some(how));
                }
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(POLL);
        }
    }
}

impl Instance for Process {
    fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }

    fn attach(&mut self) -> Result<Stream> {
        let (stdout, stdin) =
            self.stdio.take().ok_or_else(|| anyhow!("already attached: a local agent's stdio can be taken once"))?;
        Ok(Stream { reader: Box::new(BufReader::new(stdout)), writer: Box::new(stdin) })
    }

    fn stop(&mut self, grace: Duration) -> Result<String> {
        {
            let mut p = lock(&self.proc);
            p.stopping = true;
            if let Some(how) = &p.exit {
                return Ok(how.clone());
            }
            terminate(&mut p.child)?;
        }
        if let Some(how) = self.reap(grace)? {
            return Ok(how);
        }
        {
            let mut p = lock(&self.proc);
            if p.exit.is_none() {
                kill(&mut p.child)?;
            }
        }
        match self.reap(KILL_WAIT)? {
            Some(how) => Ok(format!("{how}, after {} s without exiting", grace.as_secs_f32())),
            None => bail!("pid {} was still running after SIGKILL", self.pid),
        }
    }
}

impl Drop for Process {
    /// A runtime that goes away without stopping its task still leaves no
    /// process behind.
    fn drop(&mut self) {
        let mut p = lock(&self.proc);
        if p.stopping || p.exit.is_some() {
            return;
        }
        p.stopping = true;
        let _ = kill(&mut p.child);
        let _ = p.child.wait();
    }
}

#[cfg(unix)]
fn signal(child: &Child, sig: libc::c_int) -> Result<()> {
    let group = i32::try_from(child.id()).context("pid out of range")?;
    // SAFETY: kill(2) takes no pointers. The child is not yet reaped (the
    // callers hold its lock and checked), so its pid, which is also its
    // process group id, still names it.
    if unsafe { libc::kill(-group, sig) } == 0 {
        return Ok(());
    }
    match std::io::Error::last_os_error() {
        // The group is already empty; the reap that follows confirms it.
        e if e.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        e => Err(e).context("signalling the agent"),
    }
}

#[cfg(unix)]
fn terminate(child: &mut Child) -> Result<()> {
    signal(child, libc::SIGTERM)
}

#[cfg(unix)]
fn kill(child: &mut Child) -> Result<()> {
    signal(child, libc::SIGKILL)
}

#[cfg(not(unix))]
fn terminate(child: &mut Child) -> Result<()> {
    child.kill().context("stopping the agent")
}

#[cfg(not(unix))]
fn kill(child: &mut Child) -> Result<()> {
    child.kill().context("killing the agent")
}

fn describe(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exited with code {code}");
    }
    #[cfg(unix)]
    if let Some(sig) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return format!("killed by signal {sig}");
    }
    format!("ended: {status}")
}
