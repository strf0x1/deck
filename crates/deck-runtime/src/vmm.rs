//! The VM half of the VM backend: one process per task that owns a libkrun
//! microVM, holds the task lock, and relays the guest's ACP stream to the
//! runtime's socket. `deckd --vmm <config>` starts it; nothing else does.
//!
//! The design choice: the VMM is the task's detached holder (ADR 0001), so
//! it survives `deckd` and is re-adopted from the record. The guest opens the
//! ACP stream out to the VMM's forward listener as soon as it starts the
//! agent, so that connection is both the readiness signal and the stream the
//! VMM serves through the same relay the local holder uses. Closing Deck's
//! window does not close the agent's stdin. The guest cannot ask the host to
//! do anything else.

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
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
    /// The host listener the guest's ACP connection arrives on. The VMM binds
    /// it before boot; libkrun connects it when the guest opens the stream.
    pub forward: PathBuf,
    pub console: PathBuf,
    /// The project to copy into the guest's workspace on first boot.
    #[serde(default)]
    pub import: Option<PathBuf>,
    /// The guest entry script. `DECK_AGENT` in `env` names the agent it runs.
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
    /// The per-task egress gateway, started before boot (ADR 0004).
    pub gateway: Gateway,
}

/// The host-side gateway process and the sockets libkrun maps to guest vsock
/// ports. The upstream URL and model names are not secrets; the credential
/// value is never in this config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gateway {
    pub executable: PathBuf,
    pub model_socket: PathBuf,
    pub extension_socket: PathBuf,
    pub model_port: u32,
    pub extension_port: u32,
    pub upstream: String,
    pub models: Vec<String>,
    /// The environment variable the gateway reads the host key from.
    pub secret: String,
}

fn default_port() -> u32 {
    1024
}

const START: Duration = Duration::from_secs(30);
const GUEST_DEATH: Duration = Duration::from_secs(2);
/// The gateway binds two sockets and prints one line; this is generous for a
/// cold exec on macOS and still fails a wedged binary.
const GATEWAY_START: Duration = Duration::from_secs(10);

static SHUTDOWN_FD: AtomicI32 = AtomicI32::new(-1);
static EXITED: AtomicBool = AtomicBool::new(false);
static TERMINATED: AtomicBool = AtomicBool::new(false);
/// Set once the runtime has been told the task is ready. Before that, a
/// gateway death is reported through the startup pipe; after it, the VMM
/// exits and the runtime sees the task fail.
static READY: AtomicBool = AtomicBool::new(false);

/// Read the config, boot the guest, and serve until stopped.
pub fn run(config: &Path) -> Result<()> {
    let started = Instant::now();
    let bytes = std::fs::read(config).with_context(|| format!("reading {}", config.display()))?;
    let config: Config = serde_json::from_slice(&bytes).context("parsing the VMM configuration")?;
    let _sockets = Sockets(vec![
        config.socket.clone(),
        config.forward.clone(),
        control(&config.socket),
        config.gateway.model_socket.clone(),
        config.gateway.extension_socket.clone(),
    ]);
    let _lock = inherited_lock(config.lock_fd)?;

    for path in [&config.socket, &config.forward] {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    let listener = UnixListener::bind(control(&config.socket))?;
    listener.set_nonblocking(true)?;
    // Bind before boot: libkrun connects this path on the guest's first vsock
    // request, so a missing listener would refuse the agent's only stream.
    let forward = UnixListener::bind(&config.forward)?;

    // The gateway must be listening before libkrun maps its sockets: the
    // guest may connect as soon as it boots. Its stdin stays open for the
    // VMM's lifetime, so the gateway exits if the VMM dies without signalling.
    let _gateway_stdin = start_gateway(&config.gateway)?;

    let krun = Krun::load(&config.library)?;
    krun.init_log()?;
    let ctx = krun.create()?;
    // No implicit vsock means no TSI: the guest's AF_INET sockets are not
    // proxied through the host. Only the mapped ports below cross.
    krun.disable_implicit_vsock(ctx).context("disabling libkrun's implicit vsock (TSI)")?;
    krun.add_vsock(ctx, 0).context("adding an explicit vsock device")?;
    krun.vm_config(ctx, config.vcpus, config.ram_mib).context("configuring vCPUs and RAM")?;
    krun.disk(ctx, "root", &config.disk, false, SYNC_RELAXED).context("adding the root disk")?;
    krun.root_disk(ctx, "/dev/vda", "ext4").context("mounting the root disk")?;
    krun.disk(ctx, "journal", &config.journal, false, SYNC_FULL).context("adding the journal disk")?;
    if let Some(import) = &config.import {
        krun.import(ctx, "import", import).context("adding the import share")?;
    }
    krun.console(ctx, &config.console).context("setting the guest console")?;
    krun.guest_vsock_port(ctx, config.port, &config.forward).context("forwarding the guest's ACP port")?;
    krun.guest_vsock_port(ctx, config.gateway.model_port, &config.gateway.model_socket)
        .context("forwarding the model gateway port")?;
    krun.guest_vsock_port(ctx, config.gateway.extension_port, &config.gateway.extension_socket)
        .context("forwarding the extension port")?;
    krun.workdir(ctx, "/workspace")?;
    krun.exec(ctx, &config.guest, &config.argv, &guest_env(&config)).context("configuring the guest command")?;
    let shutdown = krun.shutdown_eventfd(ctx).context("enabling guest shutdown")?;
    install_signals(shutdown)?;
    eprintln!("vmm: configured at {} ms", started.elapsed().as_millis());

    let config = Arc::new(config);
    let (eof_tx, eof_rx) = mpsc::channel();
    std::thread::Builder::new().name("vmm-relay".into()).spawn({
        let config = config.clone();
        move || {
            if let Err(error) = serve(&config, forward, eof_tx, started) {
                // The runtime waits for one startup line; an error is better
                // than the 30 s silence a dead serve thread would leave.
                let _ = report(Err(format!("{error:#}")));
            }
        }
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

    eprintln!("vmm: booting at {} ms", started.elapsed().as_millis());
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

/// Accept the guest's ACP connection, serve it through the relay, and report
/// readiness. The guest opens the connection only after it has started the
/// agent, so the accept is the readiness event; nothing sleeps.
fn serve(config: &Config, forward: UnixListener, eof: mpsc::Sender<()>, started: Instant) -> Result<()> {
    let stream = accept_within(&forward, START)?;
    eprintln!("vmm: guest connected at {} ms", started.elapsed().as_millis());
    let reader = BufReader::new(stream.try_clone()?);
    let relay = Relay::serve(
        &config.socket,
        Stream { reader: Box::new(EofReader { inner: reader, eof }), writer: Box::new(stream) },
    )?;
    let identity = Identity { holder: Holder { pid: std::process::id(), socket: config.socket.clone() }, agent: None };
    READY.store(true, Ordering::Release);
    report(Ok(identity))?;
    eprintln!("vmm: relay ready at {} ms", started.elapsed().as_millis());
    // The relay owns the reader and writer; hold it until the process ends.
    let _relay = relay;
    loop {
        std::thread::park();
    }
}

/// Start the gateway, wait for its readiness line, and watch it for the rest
/// of the task's life. The returned stdin handle must stay alive: closing it
/// is how the gateway learns the VMM is gone.
fn start_gateway(gateway: &Gateway) -> Result<std::process::ChildStdin> {
    let mut command = Command::new(&gateway.executable);
    command
        .arg("--model-socket")
        .arg(&gateway.model_socket)
        .arg("--extension-socket")
        .arg(&gateway.extension_socket)
        .arg("--upstream")
        .arg(&gateway.upstream)
        .arg("--secret")
        .arg(&gateway.secret)
        .arg("--exit-on-stdin-eof")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    for model in &gateway.models {
        command.args(["--model", model]);
    }
    let mut child =
        command.spawn().with_context(|| format!("starting the egress gateway {}", gateway.executable.display()))?;
    let stdin = child.stdin.take().context("gateway stdin")?;
    let stdout = child.stdout.take().context("gateway stdout")?;
    let (line_tx, line_rx) = mpsc::channel();
    std::thread::Builder::new().name("vmm-gateway-ready".into()).spawn(move || {
        let mut line = String::new();
        let outcome = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = line_tx.send(outcome);
    })?;
    let line = match line_rx.recv_timeout(GATEWAY_START) {
        Ok(Ok(line)) if !line.trim().is_empty() => line,
        Ok(Ok(_)) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the egress gateway exited before it was ready");
        }
        Ok(Err(error)) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("reading the egress gateway's readiness line");
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the egress gateway did not report readiness within {} s", GATEWAY_START.as_secs());
        }
    };
    if let Err(error) = check_ready(&line, gateway) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    std::thread::Builder::new().name("vmm-gateway".into()).spawn(move || {
        let status = child.wait();
        if EXITED.load(Ordering::Acquire) {
            return;
        }
        let why = match status {
            Ok(status) => format!("exited with {status}"),
            Err(error) => format!("could not be waited for: {error}"),
        };
        if !READY.load(Ordering::Acquire) {
            let _ = report(Err(format!("the egress gateway {why} before the guest connected")));
        }
        eprintln!("vmm: egress gateway {why}; failing the task closed");
        std::process::exit(1);
    })?;
    Ok(stdin)
}

/// The gateway's readiness line must name this task's sockets. A binary that
/// reports another task's paths would be mapped into this VM, so it is
/// refused rather than trusted.
fn check_ready(line: &str, gateway: &Gateway) -> Result<()> {
    let ready: serde_json::Value = serde_json::from_str(line.trim()).context("parsing the gateway readiness line")?;
    let named = |key: &str| ready.get(key).and_then(|value| value.as_str()).map(Path::new);
    if named("model") != Some(gateway.model_socket.as_path())
        || named("extension") != Some(gateway.extension_socket.as_path())
    {
        bail!("the egress gateway reported sockets that are not this task's");
    }
    Ok(())
}

/// Wait for the guest's first vsock connection with poll(2), so the accept
/// wakes on the connection instead of on a polling interval.
fn accept_within(listener: &UnixListener, within: Duration) -> Result<UnixStream> {
    let deadline = Instant::now() + within;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("the guest did not connect its ACP stream within {} s", within.as_secs());
        }
        let mut request = libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let millis = i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX);
        // SAFETY: one initialised pollfd for the listener's own descriptor.
        let ready = unsafe { libc::poll(&mut request, 1, millis) };
        if ready > 0 {
            let (socket, _) = listener.accept()?;
            return Ok(socket);
        }
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("waiting for the guest's ACP connection");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("deck-vmm-{name}-{}", std::process::id()))
    }

    #[test]
    fn accept_within_wakes_on_the_guest_connection() {
        let dir = scratch("accept-wake");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("forward");
        let listener = UnixListener::bind(&path).unwrap();
        let peer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let mut stream = UnixStream::connect(&path).unwrap();
            stream.write_all(b"hello\n").unwrap();
        });
        let started = Instant::now();
        let mut stream = accept_within(&listener, Duration::from_secs(5)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2), "accept waited on the timeout");
        let mut line = String::new();
        BufReader::new(&mut stream).read_line(&mut line).unwrap();
        assert_eq!(line, "hello\n");
        peer.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn gateway() -> Gateway {
        Gateway {
            executable: PathBuf::from("/bin/true"),
            model_socket: PathBuf::from("/tmp/task.model"),
            extension_socket: PathBuf::from("/tmp/task.extension"),
            model_port: 1026,
            extension_port: 1027,
            upstream: "http://127.0.0.1:1/v1".into(),
            models: vec!["m".into()],
            secret: "KEY".into(),
        }
    }

    #[test]
    fn readiness_must_name_this_tasks_sockets() {
        let gateway = gateway();
        check_ready(r#"{"model":"/tmp/task.model","extension":"/tmp/task.extension"}"#, &gateway).unwrap();
        assert!(check_ready(r#"{"model":"/tmp/other.model","extension":"/tmp/task.extension"}"#, &gateway).is_err());
        assert!(check_ready(r#"{"model":"/tmp/task.model"}"#, &gateway).is_err());
        assert!(check_ready("not json", &gateway).is_err());
    }

    #[test]
    fn accept_within_times_out_without_a_connection() {
        let dir = scratch("accept-timeout");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let listener = UnixListener::bind(dir.join("forward")).unwrap();
        let started = Instant::now();
        let error = accept_within(&listener, Duration::from_millis(30)).unwrap_err();
        assert!(error.to_string().contains("did not connect"), "{error:#}");
        assert!(started.elapsed() >= Duration::from_millis(30));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
