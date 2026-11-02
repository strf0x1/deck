//! The VM half of the VM backend: one process per task that owns a libkrun
//! microVM, holds the task lock, and relays the guest's ACP stream to the
//! runtime's socket. `deckd --vmm <config>` starts it; nothing else does.
//!
//! The design choice: the VMM is the task's detached holder (ADR 0001), so
//! it survives `deckd` and is re-adopted from the record. It connects once to
//! the guest's vsock port and serves that single stream through the same
//! relay the local holder uses, so closing Deck's window does not close the
//! agent's stdin. The guest cannot ask the host to do anything else.

use crate::backend::Stream;
use crate::holder::{Identity, Reply, Request};
use crate::krun::{Krun, SYNC_FULL, SYNC_RELAXED};
use crate::lifecycle::Holder;
use crate::relay::Relay;
use crate::wire;
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::ffi::c_int;
use std::io::{BufRead, BufReader, Read, Write as _};
use std::os::fd::FromRawFd as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI32, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

/// What the runtime writes before launching the VMM. It is the complete
/// description of one guest, so an adopted VMM never needs to re-read it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub task: String,
    pub data: PathBuf,
    /// The liveness lock, inherited through exec from the launcher.
    pub lock_fd: i32,
    /// Root filesystem disk, writable, relaxed flush on macOS.
    pub disk: PathBuf,
    /// Construct's home, on its own disk so a workspace corruption cannot
    /// take the transcript with it.
    pub journal: PathBuf,
    /// The ACP endpoint the runtime attaches to.
    pub socket: PathBuf,
    /// The internal host end of libkrun's vsock forward; the VMM consumes it.
    pub forward: PathBuf,
    /// The guest connects out to this port when ACP is actually listening.
    pub ready: PathBuf,
    pub console: PathBuf,
    /// The project to copy into the guest's workspace on first boot.
    #[serde(default)]
    pub import: Option<PathBuf>,
    /// The agent's path inside the guest.
    pub guest: String,
    pub argv: Vec<String>,
    /// Guest environment: `K=V`, already filtered by the runtime.
    pub env: Vec<String>,
    pub vcpus: u8,
    pub ram_mib: u32,
    #[serde(default = "default_port")]
    pub port: u32,
    /// The pinned libkrun dylib.
    pub library: PathBuf,
}

fn default_port() -> u32 {
    1024
}

const START: Duration = Duration::from_secs(30);
const GUEST_DEATH: Duration = Duration::from_secs(2);
/// The guest connects out here after its ACP listener is bound. A separate
/// port exists because a host connection to the ACP forward can be accepted
/// before the guest listener exists, and that connection is then reset.
pub const READY_PORT: u32 = 1025;

static SHUTDOWN_FD: AtomicI32 = AtomicI32::new(-1);
static EXITED: AtomicBool = AtomicBool::new(false);
static TERMINATED: AtomicBool = AtomicBool::new(false);

/// Read the config, boot the guest, and serve until stopped.
pub fn run(config: &Path) -> Result<()> {
    let bytes = std::fs::read(config).with_context(|| format!("reading {}", config.display()))?;
    let config: Config = serde_json::from_slice(&bytes).context("parsing the VMM configuration")?;
    let _sockets =
        Sockets(vec![config.socket.clone(), config.forward.clone(), config.ready.clone(), control(&config.socket)]);
    let _lock = inherited_lock(config.lock_fd)?;

    for path in [&config.socket, &config.forward, &config.ready] {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    let listener = UnixListener::bind(control(&config.socket))?;
    listener.set_nonblocking(true)?;
    let ready = UnixListener::bind(&config.ready)?;
    ready.set_nonblocking(true)?;

    let krun = Krun::load(&config.library)?;
    krun.init_log()?;
    let ctx = krun.create()?;
    krun.vm_config(ctx, config.vcpus, config.ram_mib).context("configuring vCPUs and RAM")?;
    krun.disk(ctx, "root", &config.disk, false, SYNC_RELAXED).context("adding the root disk")?;
    krun.root_disk(ctx, "/dev/vda", "ext4").context("mounting the root disk")?;
    krun.disk(ctx, "journal", &config.journal, false, SYNC_FULL).context("adding the journal disk")?;
    if let Some(import) = &config.import {
        krun.import(ctx, "import", import).context("adding the import share")?;
    }
    krun.console(ctx, &config.console).context("setting the guest console")?;
    krun.vsock_port(ctx, config.port, &config.forward).context("forwarding the ACP port")?;
    krun.guest_vsock_port(ctx, READY_PORT, &config.ready).context("mapping the readiness port")?;
    krun.workdir(ctx, "/workspace")?;
    krun.exec(ctx, &config.guest, &config.argv, &guest_env(&config)).context("configuring the guest command")?;
    let shutdown = krun.shutdown_eventfd(ctx).context("enabling guest shutdown")?;
    install_signals(shutdown)?;

    let config = Arc::new(config);
    let (eof_tx, eof_rx) = mpsc::channel();
    std::thread::Builder::new().name("vmm-relay".into()).spawn({
        let config = config.clone();
        move || serve(&config, ready, eof_tx)
    })?;
    std::thread::Builder::new().name("vmm-control".into()).spawn({
        let config = config.clone();
        move || control_loop(&config, listener, shutdown)
    })?;
    std::thread::Builder::new().name("vmm-signal".into()).spawn(move || {
        // A signal asks the guest to power off; the guest may have no power-key
        // handler (ours is a shell script), so the VMM must still leave after
        // the same grace the control socket uses. HVF then ends the guest.
        while !TERMINATED.load(Ordering::Acquire) {
            if EXITED.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let deadline = Instant::now() + GUEST_DEATH;
        while !EXITED.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                std::process::exit(0);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })?;
    std::thread::Builder::new().name("vmm-watch".into()).spawn(move || {
        if eof_rx.recv().is_err() {
            return;
        }
        // The guest closed the ACP stream: either the agent exited or the
        // guest is going away. Ask it to power off, and stop the VMM if it
        // does not, so the runtime sees an unclean stop instead of a live
        // task with no agent.
        request_shutdown(shutdown);
        let deadline = Instant::now() + GUEST_DEATH;
        while !EXITED.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                std::process::exit(1);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })?;

    let outcome = krun.start(ctx);
    EXITED.store(true, Ordering::Release);
    outcome?;
    Ok(())
}

/// The launcher cleared CLOEXEC on this descriptor before exec, so the
/// VMM holds the same open file description, and therefore the same flock,
/// the launcher did. Never reopen the lock file: a second flock would not be
/// the same liveness token.
fn inherited_lock(fd: i32) -> Result<crate::record::RecordLock> {
    if fd < 3 {
        bail!("VMM needs its inherited lock descriptor, not {fd}");
    }
    // SAFETY: fcntl validates without dereferencing a pointer. Restore
    // CLOEXEC so no later child can keep the lock alive after this process.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error()).context("protecting the VMM lock descriptor");
    }
    // SAFETY: the launcher transferred one owned, open descriptor.
    Ok(crate::record::RecordLock::held(unsafe { std::fs::File::from_raw_fd(fd) }))
}

/// Connect to the host end of the vsock forward once libkrun has created it,
/// then serve the guest stream and report readiness.
fn serve(config: &Config, ready: UnixListener, eof: mpsc::Sender<()>) -> Result<()> {
    await_ready(&ready, START)?;
    let stream = connect_for(&config.forward, START)?;
    let reader = BufReader::new(stream.try_clone()?);
    let relay = Relay::serve(
        &config.socket,
        Stream { reader: Box::new(EofReader { inner: reader, eof }), writer: Box::new(stream) },
    )?;
    let identity = Identity { holder: Holder { pid: std::process::id(), socket: config.socket.clone() }, agent: None };
    report(Ok(identity))?;
    // The relay owns the reader and writer; hold it until the process ends.
    let _relay = relay;
    loop {
        std::thread::park();
    }
}

/// The guest connects out to `READY_PORT` only after its ACP listener is
/// bound. Seeing that connection is the one reliable readiness signal.
fn await_ready(listener: &UnixListener, within: Duration) -> Result<()> {
    let deadline = Instant::now() + within;
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                socket.set_read_timeout(Some(Duration::from_secs(1)))?;
                let mut line = String::new();
                BufReader::new(socket).read_line(&mut line)?;
                if line.trim() == "ready" {
                    return Ok(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e).context("accepting the guest readiness connection"),
        }
        if Instant::now() >= deadline {
            bail!("guest did not signal ACP readiness within {} s", within.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn connect_for(path: &Path, within: Duration) -> Result<UnixStream> {
    let deadline = Instant::now() + within;
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(e) if Instant::now() >= deadline => {
                return Err(e).with_context(|| format!("connecting to {}", path.display()));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn control_loop(config: &Config, listener: UnixListener, shutdown: i32) -> Result<()> {
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                if request(socket, config, shutdown).is_err() {
                    break;
                }
                if EXITED.load(Ordering::Acquire) {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if EXITED.load(Ordering::Acquire) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e).context("accepting VMM control"),
        }
    }
    Ok(())
}

fn request(mut socket: UnixStream, config: &Config, shutdown: i32) -> Result<()> {
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    socket.set_write_timeout(Some(Duration::from_secs(1)))?;
    let line = wire::read(&mut BufReader::new(socket.try_clone()?))?.context("empty VMM request")?;
    match serde_json::from_slice::<Request>(&line)? {
        Request::Identify => {
            let identity =
                Identity { holder: Holder { pid: std::process::id(), socket: config.socket.clone() }, agent: None };
            let reply: std::result::Result<Reply, String> = Ok(Reply::Identity(identity));
            let _ = wire::write(&mut socket, &reply);
        }
        Request::Stop { grace_ms } => {
            let grace = Duration::from_millis(grace_ms.min(30_000));
            request_shutdown(shutdown);
            let reason = if wait_exit(grace) { "guest powered off" } else { "VMM exited after grace" };
            let reply: std::result::Result<Reply, String> = Ok(Reply::Stopped(reason.into()));
            let _ = wire::write(&mut socket, &reply);
            // The guest cannot confirm its own death; the VMM's exit is the
            // confirmation the runtime waits for.
            std::process::exit(0);
        }
    }
    Ok(())
}

/// libkrun's shutdown eventfd is a request to the guest, not proof it acted.
fn request_shutdown(fd: i32) {
    if fd >= 0 {
        let value = 1u64.to_le_bytes();
        // SAFETY: write(2) with a valid stack buffer; a failed write only
        // means the guest was already going away.
        let _ = unsafe { libc::write(fd, value.as_ptr().cast(), value.len()) };
    }
}

fn wait_exit(within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while !EXITED.load(Ordering::Acquire) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

fn install_signals(fd: i32) -> Result<()> {
    SHUTDOWN_FD.store(fd, Ordering::Release);
    let handler: extern "C" fn(c_int) = on_term;
    // SAFETY: the handler is async-signal-safe (an atomic load and write(2)).
    unsafe {
        if libc::signal(libc::SIGTERM, handler as libc::sighandler_t) == libc::SIG_ERR {
            bail!("installing SIGTERM handler: {}", std::io::Error::last_os_error());
        }
        if libc::signal(libc::SIGINT, handler as libc::sighandler_t) == libc::SIG_ERR {
            bail!("installing SIGINT handler: {}", std::io::Error::last_os_error());
        }
    }
    Ok(())
}

extern "C" fn on_term(_: c_int) {
    TERMINATED.store(true, Ordering::Release);
    let fd = SHUTDOWN_FD.load(Ordering::Acquire);
    if fd >= 0 {
        let value = 1u64.to_le_bytes();
        // SAFETY: async-signal-safe write to the eventfd.
        let _ = unsafe { libc::write(fd, value.as_ptr().cast(), value.len()) };
    }
}

fn guest_env(config: &Config) -> Vec<String> {
    let mut env = vec![
        "PATH=/usr/local/bin:/usr/bin:/bin".into(),
        "HOME=/root".into(),
        "CONSTRUCT_HOME=/root/.construct".into(),
        "TERM=dumb".into(),
        format!("DECK_AGENT={}", config.guest),
    ];
    env.extend(config.env.iter().cloned());
    env
}

fn report(result: std::result::Result<Identity, String>) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &result)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn control(socket: &Path) -> PathBuf {
    socket.with_extension("holder")
}

struct Sockets(Vec<PathBuf>);

impl Drop for Sockets {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Notices EOF on the guest stream so the VMM can stop when the agent does.
struct EofReader {
    inner: BufReader<UnixStream>,
    eof: mpsc::Sender<()>,
}

impl Read for EofReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 {
            let _ = self.eof.send(());
        }
        Ok(n)
    }
}

impl BufRead for EofReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        let buf = self.inner.fill_buf()?;
        if buf.is_empty() {
            let _ = self.eof.send(());
        }
        Ok(buf)
    }

    fn consume(&mut self, n: usize) {
        self.inner.consume(n);
    }
}
