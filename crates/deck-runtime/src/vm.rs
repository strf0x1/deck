//! The VM backend: a task is a detached libkrun VMM (`deckd --vmm`) that owns
//! the task's root and journal disks and serves the guest's ACP stream. The
//! runtime contract is unchanged: the VMM's pid is the task pid, its relay
//! socket is the holder socket, and stop is confirmed from the holder, not
//! assumed.
//!
//! Design choices: the guest image is a template cloned per task; the project
//! is copied into the guest's writable root disk on first boot through a
//! read-only import share, never mounted as the live workspace; Construct's
//! journal lives on its own disk so a workspace corruption cannot take the
//! transcript with it. The image and libkrun are named by environment, so a
//! missing VM setup fails at start with an explicit error, not a silent
//! local run.

use crate::backend::{Backend, Exited, Instance, Stream};
use crate::boundary;
use crate::holder::{Identity, Reply, Request, WAIT};
use crate::lifecycle::{Holder, Task};
use crate::paths::Paths;
use crate::record::RecordLock;
use crate::{reconcile, record::Store, vmm};
use anyhow::{Context as _, Result, bail};
use std::io::BufReader;
use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

/// How long the VMM gets to report its relay socket.
const START: Duration = Duration::from_secs(30);

/// How long the VMM's threads get to finish closing the file table after its
/// process group is gone. The record lock is shared by every thread, so it is
/// the only proof that nothing of the VMM still holds the task's disks.
const RELEASE: Duration = Duration::from_secs(5);

/// Where the image, journal template and libkrun come from.
pub const IMAGE_ENV: &str = "DECK_VM_IMAGE";
pub const JOURNAL_ENV: &str = "DECK_VM_JOURNAL";
pub const KRUN_ENV: &str = "DECK_VM_KRUN";
pub const GUEST_ENV: &str = "DECK_VM_GUEST";
#[cfg(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64")))]
const VCPUS_ENV: &str = "DECK_VM_VCPUS";
#[cfg(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64")))]
const RAM_ENV: &str = "DECK_VM_RAM_MIB";
/// The `deckd` binary that implements `--vmm`. Defaults to the running
/// executable, which is right for the daemon and wrong for a test harness.
pub const EXECUTABLE_ENV: &str = "DECK_VM_EXECUTABLE";
/// The egress gateway binary. Defaults to `deck-gateway` beside the `deckd`
/// named above (ADR 0004).
pub const GATEWAY_ENV: &str = "DECK_GATEWAY_EXECUTABLE";
/// Process-start model limits. A task's `budget.json` written by `deckd`
/// overrides them live.
pub const REQUEST_BUDGET_ENV: &str = "DECK_MODEL_REQUEST_BUDGET";
pub const TOKEN_BUDGET_ENV: &str = "DECK_MODEL_TOKEN_BUDGET";
/// The web and package profiles a task may use without asking. A comma- or
/// space-separated list of the names the gateway defines (`repo`,
/// `registry`, `package-mirrors`, `os-mirrors`, `docs`). Empty means the task
/// has a web gateway with no pre-approved destination, which is the closed
/// default; `none` disables it entirely.
pub const WEB_PROFILES_ENV: &str = "DECK_WEB_PROFILES";
/// The project's GitHub repository (`owner/name`). Absent starts no GitHub
/// gateway, which is the closed default: a task publishes only when its
/// project names where.
pub const GITHUB_REPOSITORY_ENV: &str = "DECK_GITHUB_REPOSITORY";
/// The branch a pull request targets. `main` when absent.
pub const GITHUB_BASE_ENV: &str = "DECK_GITHUB_BASE";
/// Branches beyond `deck/<task>` this project lets a task write.
pub const GITHUB_BRANCHES_ENV: &str = "DECK_GITHUB_BRANCHES";
/// The environment variable the GitHub token is read from; the value never
/// enters `vmm.json` or the guest (ADR 0007 §4).
pub const GITHUB_TOKEN_ENV: &str = "DECK_GITHUB_TOKEN_SECRET";
/// The default variable the gateway reads the token from.
pub const GITHUB_TOKEN: &str = "DECK_GITHUB_TOKEN";

/// The GitHub operation gateway's host policy (ADR 0007): the repository, the
/// base, and the branches a task may also write. The token's value is never
/// here; only the name of the environment variable it comes from.
#[derive(Debug, Clone)]
pub struct GitHub {
    pub repository: String,
    pub base: String,
    pub branches: Vec<String>,
    pub token: String,
}

pub struct Vm {
    data: PathBuf,
    paths: Paths,
    image: PathBuf,
    journal: PathBuf,
    library: PathBuf,
    guest: String,
    vcpus: u8,
    ram_mib: u32,
    executable: PathBuf,
    gateway: PathBuf,
    /// The provider endpoint and model this task may use. They are host
    /// configuration: the guest gets the relay URL and a placeholder key.
    upstream: String,
    model: String,
    /// The web and package profiles; `None` disables the web gateway.
    web_profiles: Option<Vec<String>>,
    /// The project's GitHub destination. `None` starts no GitHub gateway:
    /// a task whose project names no repository has no route to publish.
    github: Option<GitHub>,
    request_budget: Option<u64>,
    token_budget: Option<u64>,
}

impl Vm {
    pub fn from_env(data: &Path, paths: Paths) -> Result<Self> {
        #[cfg(not(any(
            all(target_os = "macos", target_arch = "aarch64"),
            all(target_os = "linux", target_arch = "x86_64")
        )))]
        {
            let _ = (data, paths);
            bail!("the vm backend requires macOS on Apple Silicon (libkrun/HVF) or Linux on x86_64 (libkrun/KVM)");
        }
        #[cfg(any(
            all(target_os = "macos", target_arch = "aarch64"),
            all(target_os = "linux", target_arch = "x86_64")
        ))]
        {
            let path = |key: &str| std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from);
            let image = path(IMAGE_ENV).with_context(|| format!("{IMAGE_ENV} names the guest root image"))?;
            let journal =
                path(JOURNAL_ENV).with_context(|| format!("{JOURNAL_ENV} names the journal disk template"))?;
            let library = path(KRUN_ENV).with_context(|| format!("{KRUN_ENV} names libkrun 1.x"))?;
            for (what, file) in [("guest image", &image), ("journal template", &journal), ("libkrun", &library)] {
                if !file.is_file() {
                    bail!("{what} {} does not exist", file.display());
                }
            }
            let vcpus = std::env::var(VCPUS_ENV).ok().map_or(Ok(2), |v| v.parse())?;
            let ram_mib = std::env::var(RAM_ENV).ok().map_or(Ok(2048), |v| v.parse())?;
            let executable = path(EXECUTABLE_ENV).map_or_else(std::env::current_exe, Ok)?;
            let gateway = match path(GATEWAY_ENV) {
                Some(path) => path,
                None => executable.with_file_name("deck-gateway"),
            };
            if !gateway.is_file() {
                bail!("{GATEWAY_ENV} names the egress gateway; {} does not exist", gateway.display());
            }
            let host = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
            let number = |key: &str| -> Result<Option<u64>> {
                match host(key) {
                    Some(value) => {
                        value.parse().map(Some).with_context(|| format!("{key} must be a whole number, not {value:?}"))
                    }
                    None => Ok(None),
                }
            };
            Ok(Self {
                data: data.to_owned(),
                paths,
                image,
                journal,
                library,
                guest: std::env::var(GUEST_ENV).unwrap_or_else(|_| "/usr/local/bin/deck-guest".into()),
                vcpus,
                ram_mib,
                executable,
                gateway,
                upstream: host("CONSTRUCT_BASE_URL").unwrap_or_else(|| boundary::DEFAULT_UPSTREAM.into()),
                model: host("CONSTRUCT_MODEL").unwrap_or_else(|| boundary::DEFAULT_MODEL.into()),
                request_budget: number(REQUEST_BUDGET_ENV)?,
                token_budget: number(TOKEN_BUDGET_ENV)?,
                web_profiles: web_profiles(host(WEB_PROFILES_ENV))?,
                github: host(GITHUB_REPOSITORY_ENV).map(|repository| GitHub {
                    repository,
                    base: host(GITHUB_BASE_ENV).unwrap_or_else(|| "main".into()),
                    branches: host(GITHUB_BRANCHES_ENV)
                        .map(|value| {
                            value
                                .split(|c: char| c == ',' || c.is_whitespace())
                                .filter(|branch| !branch.is_empty())
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default(),
                    token: host(GITHUB_TOKEN_ENV).unwrap_or_else(|| GITHUB_TOKEN.into()),
                }),
            })
        }
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.data.join("tasks").join(id)
    }

    fn disk(&self, id: &str) -> PathBuf {
        self.dir(id).join("disk.img")
    }

    fn journal(&self, id: &str) -> PathBuf {
        self.dir(id).join("journal.img")
    }

    fn config(&self, id: &str) -> PathBuf {
        self.dir(id).join("vmm.json")
    }

    fn console(&self, id: &str) -> PathBuf {
        self.dir(id).join("console.log")
    }

    fn forward(&self, id: &str) -> PathBuf {
        self.paths.task(id).with_extension("vsock")
    }
}

impl Backend for Vm {
    fn name(&self) -> &'static str {
        "vm"
    }

    fn start(&mut self, task: &Task, lock: RecordLock, exited: Exited) -> Result<Box<dyn Instance>> {
        self.reconcile(task)?;
        self.paths.prepare()?;
        std::fs::create_dir_all(self.dir(&task.id))?;
        for (from, to) in [(&self.image, self.disk(&task.id)), (&self.journal, self.journal(&task.id))] {
            clone_if_absent(from, &to)?;
        }
        let socket = self.paths.task(&task.id);
        let forward = self.forward(&task.id);
        for path in [&socket, &forward] {
            if path.exists() {
                std::fs::remove_file(path)?;
            }
        }
        let (prog, args) = task.spec.argv.split_first().context("no agent command")?;
        let guest = if prog.contains('/') { prog.clone() } else { format!("/usr/local/bin/{prog}") };
        let env = guest_environment(&guest, args, &self.model, self.web_profiles.is_some(), self.github.is_some());
        let file = lock.transfer().context("missing task lock")?;
        let fd = file.as_raw_fd();
        let config = vmm::Config {
            task: task.id.clone(),
            data: self.data.clone(),
            lock_fd: fd,
            disk: self.disk(&task.id),
            journal: self.journal(&task.id),
            socket: socket.clone(),
            forward,
            console: self.console(&task.id),
            import: task.spec.cwd.is_dir().then(|| task.spec.cwd.clone()),
            guest: self.guest.clone(),
            // libkrun's exec argv is everything after the executable path;
            // the guest script reads the vsock port as $1.
            argv: vec![boundary::ACP_PORT.to_string()],
            env,
            vcpus: self.vcpus,
            ram_mib: self.ram_mib,
            port: boundary::ACP_PORT,
            library: self.library.clone(),
            gateway: vmm::Gateway {
                executable: self.gateway.clone(),
                model_socket: socket.with_extension("model"),
                extension_socket: socket.with_extension("extension"),
                model_port: boundary::MODEL_PORT,
                extension_port: boundary::EXTENSION_PORT,
                web_socket: self.web_profiles.as_ref().map(|_| socket.with_extension("web")),
                web_port: self.web_profiles.as_ref().map(|_| boundary::WEB_VSOCK_PORT),
                web_profiles: self.web_profiles.clone().unwrap_or_default(),
                web_grants: vec![
                    self.dir(&task.id).join("grants.json"),
                    self.data.join("approvals").join("projects.json"),
                ],
                github_socket: self.github.as_ref().map(|_| socket.with_extension("github")),
                github_port: self.github.as_ref().map(|_| boundary::GITHUB_VSOCK_PORT),
                github_repository: self.github.as_ref().map(|github| github.repository.clone()),
                github_base: self.github.as_ref().map(|github| github.base.clone()).unwrap_or_default(),
                github_branches: self.github.as_ref().map(|github| github.branches.clone()).unwrap_or_default(),
                github_token: self.github.as_ref().map(|github| github.token.clone()).unwrap_or_default(),
                github_rules: vec![self.data.join("approvals").join("github.json")],
                github_scratch: self.dir(&task.id).join("github"),
                upstream: self.upstream.clone(),
                models: vec![self.model.clone()],
                request_budget: self.request_budget,
                token_budget: self.token_budget,
                usage: task.usage,
                budget_file: self.dir(&task.id).join("budget.json"),
                secret: boundary::SECRET.into(),
                control: self.paths.control(),
                task: task.id.clone(),
            },
        };
        write_atomic(&self.config(&task.id), &serde_json::to_vec(&config)?)?;

        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.dir(&task.id).join("vmm.log"))?;
        let mut command = Command::new(&self.executable);
        command
            .arg("--vmm")
            .arg(self.config(&task.id))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log));
        if let Some(paths) = library_path(&self.library) {
            command.env("DYLD_LIBRARY_PATH", paths.clone()).env("LD_LIBRARY_PATH", paths);
        }
        // SAFETY: setsid and fcntl are async-signal-safe. Only the VMM
        // inherits the cleared descriptor; the parent's copy stays CLOEXEC.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 || libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().context("launching the VMM")?;
        drop(file);
        let stdout = child.stdout.take().context("VMM startup pipe")?;
        let pid = child.id();
        let (startup_tx, startup_rx) = mpsc::channel();
        let startup = std::thread::spawn(move || {
            let outcome = (|| {
                let line = crate::wire::read(&mut BufReader::new(stdout))?.context("VMM did not report startup")?;
                let outcome: std::result::Result<Identity, String> = serde_json::from_slice(&line)?;
                outcome.map_err(anyhow::Error::msg)
            })();
            let _ = startup_tx.send(outcome);
        });
        // The VMM writes one line when its relay is listening for the window.
        // Wake on that line rather than polling the reader thread.
        let identity = match startup_rx.recv_timeout(START) {
            Ok(Ok(identity)) => identity,
            Ok(Err(error)) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("VMM startup failed");
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("VMM did not report a relay socket within {} s", START.as_secs());
            }
        };
        let _ = startup.join();
        if identity.holder.pid != pid || identity.holder.socket != socket {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "VMM reported {}:{} for pid {pid}, expected {pid}:{}",
                identity.holder.pid,
                identity.holder.socket.display(),
                socket.display()
            );
        }
        let done = Arc::new(AtomicBool::new(false));
        watch(self.paths.clone(), task.id.clone(), identity.holder.clone(), done.clone(), Some(child), exited);
        Ok(Box::new(VmInstance {
            data: self.data.clone(),
            paths: self.paths.clone(),
            id: task.id.clone(),
            holder: identity.holder,
            done,
        }))
    }

    fn reconcile(&mut self, task: &Task) -> Result<()> {
        let mut pids = Vec::new();
        if let Some(pid) = task.pid {
            pids.push(pid);
        }
        if let Some(holder) = &task.holder {
            pids.push(holder.pid);
        }
        for pid in pids {
            if !reconcile::group_running(pid)? {
                continue;
            }
            if !is_vmm(&self.data, &task.id, pid) {
                bail!("process {pid} for task {} is not this task's VMM; refusing to signal it", task.id);
            }
            kill_group(pid, libc::SIGKILL)?;
            reconcile::wait_for_group(pid)?;
        }
        cleanup(&self.paths, &task.id);
        Ok(())
    }

    fn adopt(&mut self, task: &Task, exited: Exited) -> Result<Option<Box<dyn Instance>>> {
        let holder = task.holder.as_ref().context("running task has no VMM holder")?;
        match exchange(holder, &Request::Identify, WAIT)? {
            Reply::Identity(identity) if identity.holder == *holder => {}
            _ => bail!("task {}: VMM identity did not match its record", task.id),
        }
        let done = Arc::new(AtomicBool::new(false));
        watch(self.paths.clone(), task.id.clone(), holder.clone(), done.clone(), None, exited);
        Ok(Some(Box::new(VmInstance {
            data: self.data.clone(),
            paths: self.paths.clone(),
            id: task.id.clone(),
            holder: holder.clone(),
            done,
        })))
    }

    fn destroy(&mut self, task: &Task) -> Result<()> {
        for path in [
            self.disk(&task.id),
            self.journal(&task.id),
            self.console(&task.id),
            self.config(&task.id),
            self.dir(&task.id).join("vmm.log"),
        ] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
            }
        }
        cleanup(&self.paths, &task.id);
        Ok(())
    }
}

struct VmInstance {
    data: PathBuf,
    paths: Paths,
    id: String,
    holder: Holder,
    done: Arc<AtomicBool>,
}

impl Instance for VmInstance {
    fn pid(&self) -> Option<u32> {
        Some(self.holder.pid)
    }

    fn holder(&self) -> Option<Holder> {
        Some(self.holder.clone())
    }

    fn attach(&mut self) -> Result<Stream> {
        let socket = UnixStream::connect(&self.holder.socket)?;
        Ok(Stream { reader: Box::new(BufReader::new(socket.try_clone()?)), writer: Box::new(socket) })
    }

    fn stop(&mut self, grace: Duration) -> Result<String> {
        if !reconcile::group_running(self.holder.pid)? {
            released(&self.data, &self.id, RELEASE)?;
            cleanup(&self.paths, &self.id);
            return Ok("VMM already exited".into());
        }
        if let Some(session) = Store::open(&self.data)?.observe(&self.id)?.session_id
            && let Ok(socket) = UnixStream::connect(&self.holder.socket)
        {
            // Best effort: an unresponsive agent must still stop.
            let _ = deck_acp::shutdown::close_session(&socket, &session, Duration::from_millis(500));
        }
        let request = Request::Stop { grace_ms: grace.as_millis().min(30_000) as u64 };
        let reason = match exchange(&self.holder, &request, grace + WAIT) {
            Ok(Reply::Stopped(reason)) => reason,
            Ok(_) => bail!("VMM did not confirm its stop"),
            Err(e) => {
                // The control socket is gone or wedged. The VMM is still
                // identified by the record; force it and confirm below.
                kill_group(self.holder.pid, libc::SIGKILL).ok();
                format!("force-stopped VMM ({e:#})")
            }
        };
        reconcile::wait_for_group(self.holder.pid)?;
        released(&self.data, &self.id, RELEASE)?;
        cleanup(&self.paths, &self.id);
        Ok(reason)
    }
}

impl Drop for VmInstance {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
    }
}

fn watch(paths: Paths, id: String, holder: Holder, done: Arc<AtomicBool>, mut child: Option<Child>, exited: Exited) {
    std::thread::spawn(move || {
        if let Some(child) = child.as_mut() {
            let _ = child.wait();
        } else {
            while reconcile::group_running(holder.pid).unwrap_or(false) {
                if done.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        if done.load(Ordering::Acquire) {
            return;
        }
        cleanup(&paths, &id);
        exited("VMM exited".into());
    });
}

fn exchange(holder: &Holder, request: &Request, timeout: Duration) -> Result<Reply> {
    let mut socket = UnixStream::connect(holder.socket.with_extension("holder"))?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;
    crate::wire::write(&mut socket, request)?;
    let line = crate::wire::read(&mut BufReader::new(socket))?.context("VMM closed control connection")?;
    let reply: std::result::Result<Reply, String> = serde_json::from_slice(&line)?;
    reply.map_err(anyhow::Error::msg)
}

/// Wait until the record lock is free. `group_running` treats a zombie leader
/// as gone, but the VMM's other threads still share its file table while they
/// exit, so the lock can outlive the process group by a few milliseconds.
fn released(data: &Path, id: &str, within: Duration) -> Result<()> {
    let store = Store::open(data)?;
    let deadline = Instant::now() + within;
    loop {
        if let Some(lock) = store.lock(id)? {
            drop(lock);
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("VMM still holds the task lock after its process group ended");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn cleanup(paths: &Paths, id: &str) {
    let socket = paths.task(id);
    for extension in ["holder", "vsock", "model", "extension"] {
        let _ = std::fs::remove_file(socket.with_extension(extension));
    }
    let _ = std::fs::remove_file(socket);
}

fn is_vmm(data: &Path, id: &str, pid: u32) -> bool {
    let Ok(output) = Command::new("ps").args(["-o", "command=", "-p", &pid.to_string()]).output() else {
        return false;
    };
    let command = String::from_utf8_lossy(&output.stdout);
    command.contains("--vmm") && command.contains(&data.join("tasks").join(id).display().to_string())
}

fn kill_group(pid: u32, signal: libc::c_int) -> Result<()> {
    let group = i32::try_from(pid).context("pid out of range")?;
    // SAFETY: kill(2) takes no pointers. The caller verified the process is
    // this task's VMM and its group leader (setsid).
    if unsafe { libc::kill(-group, signal) } == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("signalling the VMM group");
        }
    }
    Ok(())
}

fn clone_if_absent(source: &Path, target: &Path) -> Result<()> {
    if target.is_file() {
        return Ok(());
    }
    let temp = target.with_extension("img.tmp");
    let _ = std::fs::remove_file(&temp);
    clone(source, &temp).with_context(|| format!("cloning {} to {}", source.display(), temp.display()))?;
    std::fs::rename(&temp, target)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn clone(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    let invalid = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, what);
    let src = CString::new(source.as_os_str().as_bytes()).map_err(|_| invalid("source path has a NUL"))?;
    let dst = CString::new(target.as_os_str().as_bytes()).map_err(|_| invalid("target path has a NUL"))?;
    // SAFETY: clonefile takes two NUL-terminated paths and a flags word.
    if unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), 0) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EEXIST) {
        return Ok(());
    }
    // APFS cloning is unavailable (another filesystem, or an old image).
    std::fs::copy(source, target).map(|_| ())
}

#[cfg(target_os = "linux")]
fn clone(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    // FICLONE copies the extent map, so a sparse multi-gigabyte base costs
    // almost nothing on a copy-on-write filesystem and shares its pages.
    const FICLONE: std::os::raw::c_ulong = 0x4004_9409;
    let source_file = std::fs::File::open(source)?;
    let target_file = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(target)?;
    // SAFETY: both descriptors are valid and owned; FICLONE takes no pointer.
    if unsafe { libc::ioctl(target_file.as_raw_fd(), FICLONE, source_file.as_raw_fd()) } == 0 {
        return Ok(());
    }
    drop(target_file);
    let _ = std::fs::remove_file(target);
    std::fs::copy(source, target).map(|_| ())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn clone(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::copy(source, target).map(|_| ())
}

/// The VMM config carries the guest environment, including any model key, so
/// it is owner-only.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let temp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

/// The environment the guest agent starts with. Host policy, not the guest's
/// writable config: the VM is the sandbox, the gateway owns the provider key,
/// and `DECK_MANAGED` tells Construct every credential is host-owned. The
/// local backend does not set any of this.
fn guest_environment(guest: &str, args: &[String], model: &str, web: bool, github: bool) -> Vec<String> {
    // The guest gets the relay's address and a non-empty placeholder, so
    // Construct's environment-first secretspec never reaches for a keyring
    // (there is none) and never sees a real value. The host gateway adds the
    // provider key after discarding this one.
    let mut env = vec![
        format!("DECK_AGENT={guest}"),
        format!("DECK_ARGS={}", quote(args)),
        format!("CONSTRUCT_MODEL={model}"),
        format!("CONSTRUCT_SANDBOX={}", boundary::GUEST_SANDBOX),
        format!("OPENAI_API_KEY={}", boundary::PLACEHOLDER_KEY),
        format!("{}={}", boundary::MANAGED_ENV, boundary::MANAGED_VALUE),
        format!("DECK_RELAY_PORT={}", boundary::RELAY_PORT),
        format!("DECK_BOUNDARY_PORT={}", boundary::BOUNDARY_PORT),
        format!("DECK_MODEL_PORT={}", boundary::MODEL_PORT),
        format!("DECK_EXTENSION_PORT={}", boundary::EXTENSION_PORT),
        format!("DECK_BOUNDARY_KIND={}", boundary::BOUNDARY_KIND),
        format!("DECK_WEB_PORT={}", boundary::WEB_PORT),
        format!("DECK_MIRROR_PORT={}", boundary::MIRROR_PORT),
        format!("DECK_WEB_VSOCK_PORT={}", boundary::WEB_VSOCK_PORT),
    ];
    // The guest script starts the web relays only for a task with a web
    // gateway; without them a proxy variable would point at a closed port,
    // which is worse for a package manager than no proxy at all.
    env.push(format!("DECK_WEB_ENABLED={}", u8::from(web)));
    // The GitHub relays exist only for a task whose project named a
    // repository; a URL pointing at a closed port would be worse than none.
    env.push(format!("DECK_GITHUB_ENABLED={}", u8::from(github)));
    env.push(format!("DECK_GITHUB_PORT={}", boundary::GITHUB_PORT));
    env.push(format!("DECK_GITHUB_VSOCK_PORT={}", boundary::GITHUB_VSOCK_PORT));
    env
}

/// `DECK_WEB_PROFILES`: absent means every built-in profile but the OS
/// mirrors, which a project that builds on Debian or Ubuntu wants and a task
/// that does not use it does not need. `none` disables the web gateway.
fn web_profiles(value: Option<String>) -> Result<Option<Vec<String>>> {
    let Some(value) = value else {
        return Ok(Some(vec!["repo".into(), "registry".into(), "package-mirrors".into(), "docs".into()]));
    };
    let names: Vec<String> =
        value.split([',', ' ']).map(str::trim).filter(|name| !name.is_empty()).map(str::to_owned).collect();
    if names.is_empty() {
        return Ok(Some(Vec::new()));
    }
    if names.iter().any(|name| name.eq_ignore_ascii_case("none")) {
        if names.len() > 1 {
            bail!("{WEB_PROFILES_ENV}: `none` cannot be combined with a profile");
        }
        return Ok(None);
    }
    Ok(Some(names))
}

/// Turn the agent arguments into one shell word list for socat's `EXEC:`.
fn quote(args: &[String]) -> String {
    args.iter().map(|arg| format!("'{}'", arg.replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ")
}

fn library_path(library: &Path) -> Option<std::ffi::OsString> {
    let mut paths = vec![PathBuf::from("/opt/homebrew/opt/libkrunfw/lib"), PathBuf::from("/opt/homebrew/opt/llvm/lib")];
    if let Some(parent) = library.parent() {
        paths.push(parent.to_owned());
    }
    // Linux names the loader variable differently; libkrun finds libkrunfw
    // through it, so both are forwarded whatever host this is.
    for key in ["DYLD_LIBRARY_PATH", "LD_LIBRARY_PATH"] {
        if let Some(existing) = std::env::var_os(key) {
            paths.extend(std::env::split_paths(&existing));
        }
    }
    std::env::join_paths(paths).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guest_environment_marks_the_task_managed() {
        let env = guest_environment("/usr/local/bin/construct", &["acp".into()], "openrouter/free", true, true);
        let has = |line: &str| env.iter().any(|e| e == line);
        assert!(has("DECK_MANAGED=1"), "a VM task's credentials are host-owned: {env:?}");
        assert!(has("OPENAI_API_KEY=deck-placeholder"), "the guest never gets a real key: {env:?}");
        assert!(has("CONSTRUCT_SANDBOX=off"), "the VM is the sandbox: {env:?}");
        assert!(has("CONSTRUCT_MODEL=openrouter/free"));
        assert!(has("DECK_AGENT=/usr/local/bin/construct"));
        assert!(has("DECK_ARGS='acp'"));
        assert!(has("DECK_WEB_ENABLED=1"), "the relay is started for a task with a web gateway: {env:?}");
        assert!(has("DECK_WEB_PORT=8082") && has("DECK_MIRROR_PORT=8083"));
        assert!(has("DECK_GITHUB_ENABLED=1"), "the GitHub relays start for a project with a repository: {env:?}");
        assert!(has("DECK_GITHUB_PORT=8084") && has("DECK_GITHUB_VSOCK_PORT=1029"));
    }

    #[test]
    fn a_task_without_a_web_gateway_starts_no_relay() {
        let env = guest_environment("/usr/local/bin/construct", &["acp".into()], "openrouter/free", false, false);
        assert!(env.iter().any(|line| line == "DECK_WEB_ENABLED=0"), "{env:?}");
        assert!(env.iter().any(|line| line == "DECK_GITHUB_ENABLED=0"), "{env:?}");
    }

    #[test]
    fn the_web_profiles_are_named_or_disabled() {
        assert_eq!(
            web_profiles(None).unwrap().unwrap(),
            vec!["repo".to_string(), "registry".into(), "package-mirrors".into(), "docs".into()],
            "a task gets the reads and registries by default, never the OS mirrors"
        );
        assert_eq!(
            web_profiles(Some("repo, os-mirrors".into())).unwrap().unwrap(),
            vec!["repo".to_string(), "os-mirrors".into()]
        );
        assert!(web_profiles(Some("none".into())).unwrap().is_none(), "`none` disables the web gateway");
        assert!(web_profiles(Some("repo,none".into())).is_err(), "`none` cannot be combined");
        assert_eq!(web_profiles(Some(" ".into())).unwrap(), Some(Vec::new()));
    }
}
