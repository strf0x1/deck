//! The VM half of the VM backend: one process per task that owns a libkrun
//! microVM, holds the task lock, and relays the guest's ACP stream to the
//! runtime's socket. `deckd --vmm <config>` starts it; nothing else does.
//!
//! The design choice: the VMM is the task's detached holder (ADR 0001), so
//! it survives `deckd` and is re-adopted from the record. The guest opens the
//! ACP stream out to the VMM's forward listener as soon as its workspace is
//! ready, and its transport then becomes the agent with that stream as stdio.
//! The connection is therefore both the readiness signal and the stream the
//! VMM serves through the same relay the local holder uses; the ACP handshake
//! is what proves the agent. Closing Deck's
//! window does not close the agent's stdin. The guest cannot ask the host to
//! do anything else.

use crate::backend::Stream;
use crate::holder::{Identity, Reply, Request};
use crate::krun::{Krun, SYNC_FULL, SYNC_RELAXED};
use crate::lifecycle::{Holder, Usage};
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
    /// Whether a first boot must find the import. A resume has no share but
    /// the guest's marker, so the flag only matters when provisioning failed.
    #[serde(default)]
    pub import_required: bool,
    /// The checkout the guest prepares after import (ADR 0008 §4). It travels
    /// as the guest entry's argv, never the agent's environment.
    #[serde(default)]
    pub checkout: Option<Checkout>,
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

/// What the guest entry needs to prepare the imported checkout: the branch to
/// create at the imported HEAD, the repository `origin` is rewritten to, and
/// the host identity commits carry. The gateway still refuses any other ref,
/// so a wrong branch makes publication fail, not widen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkout {
    pub branch: String,
    pub repository: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
}

/// The host-side gateway process and the sockets libkrun maps to guest vsock
/// ports. The upstream URL and model names are not secrets; credential names
/// are configuration and their values are never in this config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gateway {
    pub executable: PathBuf,
    pub model_socket: PathBuf,
    pub extension_socket: PathBuf,
    pub model_port: u32,
    pub extension_port: u32,
    /// The web and package gateway's socket and port. `None` serves no web
    /// traffic, which is the closed default for a task with no profile.
    #[serde(default)]
    pub web_socket: Option<PathBuf>,
    #[serde(default)]
    pub web_port: Option<u32>,
    /// The web profiles the task may use.
    #[serde(default)]
    pub web_profiles: Vec<String>,
    /// Task and project grant files, shared by model and web policy.
    #[serde(default, alias = "web_grants")]
    pub grants: Vec<PathBuf>,
    /// Host project identity. An older config without it admits no project
    /// rule from the shared file. New task grants still work.
    #[serde(default, alias = "web_project")]
    pub project: Option<String>,
    /// The GitHub operation gateway's socket and port. `None` serves no
    /// publication: the task's project names no repository.
    #[serde(default)]
    pub github_socket: Option<PathBuf>,
    #[serde(default)]
    pub github_port: Option<u32>,
    /// `owner/name`, the base a pull request targets, and any branch the
    /// project allows a task to write beyond its own.
    #[serde(default)]
    pub github_repository: Option<String>,
    #[serde(default)]
    pub github_base: String,
    #[serde(default)]
    pub github_branches: Vec<String>,
    /// The credential name the GitHub token is fetched under. Its value is
    /// never in this config.
    #[serde(default)]
    pub github_token: String,
    /// The gateway's scratch directory, where a publish fetches its bundle.
    pub github_scratch: PathBuf,
    pub upstream: String,
    pub models: Vec<String>,
    /// Process-start model limits; the task's budget file overrides them.
    #[serde(default)]
    pub request_budget: Option<u64>,
    #[serde(default)]
    pub token_budget: Option<u64>,
    /// Totals already recorded for this task, so a resumed run continues.
    #[serde(default)]
    pub usage: Usage,
    /// The live budget file the gateway re-reads before each admission.
    pub budget_file: PathBuf,
    /// The credential name the model gateway fetches the provider key under;
    /// `None` for a keyless provider (ADR 0012).
    #[serde(default)]
    pub secret: Option<String>,
    /// Every credential name this task's gateway may fetch (ADR 0004 §4.3).
    /// `deckd` refuses any other name without reading the store.
    #[serde(default)]
    pub credentials: Vec<String>,
    /// The daemon's control socket and this task, so a model the task was not
    /// configured with can ask the approval queue (ADR 0004 §5).
    pub control: PathBuf,
    pub task: String,
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
    let _sockets = Sockets(
        vec![
            config.socket.clone(),
            config.forward.clone(),
            control(&config.socket),
            config.gateway.model_socket.clone(),
            config.gateway.extension_socket.clone(),
        ]
        .into_iter()
        .chain(config.gateway.web_socket.iter().cloned())
        .chain(config.gateway.github_socket.iter().cloned())
        .collect::<Vec<_>>(),
    );
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
    if let (Some(port), Some(socket)) = (config.gateway.web_port, &config.gateway.web_socket) {
        krun.guest_vsock_port(ctx, port, socket).context("forwarding the web gateway port")?;
    }
    if let (Some(port), Some(socket)) = (config.gateway.github_port, &config.gateway.github_socket) {
        krun.guest_vsock_port(ctx, port, socket).context("forwarding the GitHub gateway port")?;
    }
    krun.workdir(ctx, "/workspace")?;
    krun.exec(ctx, &config.guest, &guest_argv(&config), &guest_env(&config))
        .context("configuring the guest command")?;
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
/// readiness. The guest opens the connection only after its workspace is
/// prepared; the transport becomes the agent immediately after, and the ACP
/// handshake proves the agent. The accept itself is the readiness event;
/// nothing sleeps.
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
        .arg("--budget-file")
        .arg(&gateway.budget_file)
        .arg("--used-requests")
        .arg(gateway.usage.requests.to_string())
        .arg("--used-tokens")
        .arg(gateway.usage.tokens.to_string())
        .arg("--control")
        .arg(&gateway.control)
        .arg("--task")
        .arg(&gateway.task)
        .arg("--exit-on-stdin-eof")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    for model in &gateway.models {
        command.args(["--model", model]);
    }
    if let Some(socket) = &gateway.github_socket {
        command.arg("--github-socket").arg(socket);
        if let Some(repository) = &gateway.github_repository {
            command.arg("--github-repository").arg(repository);
        }
        command.arg("--github-scratch").arg(&gateway.github_scratch);
        command.arg("--github-token").arg(&gateway.github_token);
        if !gateway.github_base.is_empty() {
            command.arg("--github-base").arg(&gateway.github_base);
        }
        for branch in &gateway.github_branches {
            command.args(["--github-branch", branch]);
        }
    }
    if let Some(socket) = &gateway.web_socket {
        command.arg("--web-socket").arg(socket);
        for profile in &gateway.web_profiles {
            command.args(["--web-profile", profile]);
        }
    }
    for grant in &gateway.grants {
        command.arg("--grants").arg(grant);
    }
    if let Some(project) = &gateway.project {
        command.arg("--project").arg(project);
    }

    if let Some(limit) = gateway.request_budget {
        command.arg("--request-budget").arg(limit.to_string());
    }
    if let Some(limit) = gateway.token_budget {
        command.arg("--token-budget").arg(limit.to_string());
    }
    // The gateway fetches every value from deckd per operation; none reaches
    // it through the environment, even one the runtime failed to strip.
    match &gateway.secret {
        Some(name) => command.arg("--secret").arg(name),
        None => command.arg("--keyless"),
    };
    for name in gateway.credentials.iter().chain(gateway.secret.iter()).chain([&gateway.github_token]) {
        if !name.is_empty() {
            command.env_remove(name);
        }
    }
    command.env_remove(crate::credentials::FILE_ENV);
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
        || named("web") != gateway.web_socket.as_deref()
        || named("github") != gateway.github_socket.as_deref()
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
                crate::wait::readable(listener.as_raw_fd(), Duration::from_millis(20))
                    .context("waiting for VMM control")?;
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionAborted) => {}
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

/// The guest entry's argv: the ACP port, then the checkout labels. They are
/// the entry's own arguments, so the agent never inherits the branch name.
fn guest_argv(config: &Config) -> Vec<String> {
    checkout_argv(&config.argv, config.checkout.as_ref(), config.import_required)
}

fn checkout_argv(argv: &[String], checkout: Option<&Checkout>, import: bool) -> Vec<String> {
    let mut argv = argv.to_vec();
    if let Some(checkout) = checkout {
        argv.extend(["--branch".into(), checkout.branch.clone()]);
        argv.extend(["--repository".into(), checkout.repository.clone()]);
        if !checkout.name.is_empty() {
            argv.extend(["--name".into(), checkout.name.clone()]);
        }
        if !checkout.email.is_empty() {
            argv.extend(["--email".into(), checkout.email.clone()]);
        }
    }
    if import {
        argv.push("--import-required".into());
    }
    argv
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
            web_socket: None,
            web_port: None,
            web_profiles: Vec::new(),
            grants: Vec::new(),
            project: None,
            github_socket: None,
            github_port: None,
            github_repository: None,
            github_base: String::new(),
            github_branches: Vec::new(),
            github_token: String::new(),
            github_scratch: PathBuf::from("/tmp/task.github"),
            upstream: "http://127.0.0.1:1/v1".into(),
            models: vec!["m".into()],
            request_budget: None,
            token_budget: None,
            usage: Usage::default(),
            budget_file: PathBuf::from("/tmp/task.budget.json"),
            secret: Some("KEY".into()),
            credentials: vec!["KEY".into()],
            control: PathBuf::from("/tmp/task.control"),
            task: "task".into(),
        }
    }

    #[test]
    fn readiness_must_name_this_tasks_sockets() {
        let base = gateway();
        check_ready(r#"{"model":"/tmp/task.model","extension":"/tmp/task.extension"}"#, &base).unwrap();
        assert!(check_ready(r#"{"model":"/tmp/other.model","extension":"/tmp/task.extension"}"#, &base).is_err());
        assert!(check_ready(r#"{"model":"/tmp/task.model"}"#, &base).is_err());
        assert!(check_ready("not json", &base).is_err());
        // A task without a web gateway cannot be handed a socket it did not
        // ask for, and one with a web gateway must name it.
        assert!(
            check_ready(r#"{"model":"/tmp/task.model","extension":"/tmp/task.extension","web":"/tmp/x.web"}"#, &base)
                .is_err()
        );
        let with_web = Gateway { web_socket: Some(PathBuf::from("/tmp/task.web")), web_port: Some(1028), ..base };
        check_ready(
            r#"{"model":"/tmp/task.model","extension":"/tmp/task.extension","web":"/tmp/task.web"}"#,
            &with_web,
        )
        .unwrap();
    }

    #[test]
    fn older_gateway_records_keep_their_grant_policy() {
        let mut saved = serde_json::to_value(gateway()).unwrap();
        let fields = saved.as_object_mut().unwrap();
        fields.remove("grants");
        fields.remove("project");
        fields.insert("web_grants".into(), serde_json::json!(["/task/grants.json"]));
        fields.insert("web_project".into(), serde_json::json!("/project"));
        let restored: Gateway = serde_json::from_value(saved).unwrap();
        assert_eq!(restored.grants, vec![PathBuf::from("/task/grants.json")]);
        assert_eq!(restored.project.as_deref(), Some("/project"));
        assert!(restored.web_socket.is_none());
    }

    #[test]
    fn the_checkout_travels_as_the_guest_entrys_argv() {
        let checkout = Checkout {
            branch: "deck/t-1".into(),
            repository: "owner/name".into(),
            name: "Ada".into(),
            email: "ada@example.com".into(),
        };
        let argv = checkout_argv(&["1024".into()], Some(&checkout), true);
        assert_eq!(
            argv,
            [
                "1024",
                "--branch",
                "deck/t-1",
                "--repository",
                "owner/name",
                "--name",
                "Ada",
                "--email",
                "ada@example.com",
                "--import-required",
            ]
        );
        assert_eq!(checkout_argv(&["1024".into()], None, false), ["1024"]);
        let bare = Checkout { name: String::new(), email: String::new(), ..checkout };
        assert_eq!(
            checkout_argv(&["1024".into()], Some(&bare), false),
            ["1024", "--branch", "deck/t-1", "--repository", "owner/name"]
        );
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
