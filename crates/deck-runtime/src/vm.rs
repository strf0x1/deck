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
        let mut env = Vec::new();
        env.push(format!("DECK_AGENT={guest}"));
        env.push(format!("DECK_ARGS={}", quote(args)));
        // The guest gets the relay's address and a non-empty placeholder, so
        // Construct's environment-first secretspec never reaches for a keyring
        // (there is none) and never sees a real value. The host gateway adds
        // the provider key after discarding this one.
        env.push(format!("CONSTRUCT_MODEL={}", self.model));
        // The VM is the sandbox: Construct's process sandbox is off, and its
        // value is host policy, never the guest's writable config. The local
        // backend does not override it.
        env.push(format!("CONSTRUCT_SANDBOX={}", boundary::GUEST_SANDBOX));
        env.push(format!("OPENAI_API_KEY={}", boundary::PLACEHOLDER_KEY));
        env.push(format!("DECK_RELAY_PORT={}", boundary::RELAY_PORT));
        env.push(format!("DECK_MODEL_PORT={}", boundary::MODEL_PORT));
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
                upstream: self.upstream.clone(),
                models: vec![self.model.clone()],
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
