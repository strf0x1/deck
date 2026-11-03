//! The libkrun 1.x C API, loaded at run time so the crate builds and tests on
//! hosts without it. Only the VMM process loads it; nothing else in deck
//! links a hypervisor.
//!
//! The design choice: the library is pinned to the 1.x ABI by the symbols we
//! require. `krun_add_disk3` (per-disk sync mode) is absent from 2.0, so
//! loading a 2.0 library fails here instead of at VM start with a wrong ABI.
//! `krun_get_shutdown_eventfd` is built only for macOS/aarch64 upstream, so it
//! is resolved when present and the guest-power-key path is skipped elsewhere.

use anyhow::{Context as _, Result, bail};
use libloading::Library;
use std::ffi::{CString, c_char};
use std::io;
use std::path::Path;

type CreateCtx = unsafe extern "C" fn() -> i32;
type InitLog = unsafe extern "C" fn(i32, u32, u32, u32) -> i32;
type SetVmConfig = unsafe extern "C" fn(u32, u8, u32) -> i32;
type AddDisk3 = unsafe extern "C" fn(u32, *const c_char, *const c_char, u32, bool, bool, u32) -> i32;
type SetRootDiskRemount = unsafe extern "C" fn(u32, *const c_char, *const c_char, *const c_char) -> i32;
type AddVirtiofs3 = unsafe extern "C" fn(u32, *const c_char, *const c_char, u64, bool) -> i32;
type DisableImplicitVsock = unsafe extern "C" fn(u32) -> i32;
type AddVsock = unsafe extern "C" fn(u32, u32) -> i32;
type AddVsockPort2 = unsafe extern "C" fn(u32, u32, *const c_char, bool) -> i32;
type SetWorkdir = unsafe extern "C" fn(u32, *const c_char) -> i32;
type SetExec = unsafe extern "C" fn(u32, *const c_char, *const *const c_char, *const *const c_char) -> i32;
type SetConsoleOutput = unsafe extern "C" fn(u32, *const c_char) -> i32;
type GetShutdownEventfd = unsafe extern "C" fn(u32) -> i32;
type StartEnter = unsafe extern "C" fn(u32) -> i32;

/// Supported disk image formats.
pub const DISK_RAW: u32 = 0;
/// Do not honour guest flush requests.
pub const SYNC_NONE: u32 = 0;
/// Honour flush requests by flushing the host's buffers, but not the drive's.
pub const SYNC_RELAXED: u32 = 1;
/// Honour flush requests down to the host drive.
pub const SYNC_FULL: u32 = 2;

pub struct Krun {
    _library: Library,
    create_ctx: CreateCtx,
    init_log: InitLog,
    set_vm_config: SetVmConfig,
    add_disk3: AddDisk3,
    set_root_disk_remount: SetRootDiskRemount,
    add_virtiofs3: AddVirtiofs3,
    disable_implicit_vsock: DisableImplicitVsock,
    add_vsock: AddVsock,
    add_vsock_port2: AddVsockPort2,
    set_workdir: SetWorkdir,
    set_exec: SetExec,
    set_console_output: SetConsoleOutput,
    get_shutdown_eventfd: Option<GetShutdownEventfd>,
    start_enter: StartEnter,
}

impl Krun {
    pub fn load(path: &Path) -> Result<Self> {
        // SAFETY: loading a dynamic library runs its initialisers. The path is
        // the pinned file the caller named, and the symbols are resolved with
        // the signatures from libkrun 1.19's `include/libkrun.h`.
        unsafe {
            let library = Library::new(path).with_context(|| format!("loading {}", path.display()))?;
            macro_rules! symbol {
                ($name:literal, $ty:ty) => {
                    *library
                        .get::<$ty>($name)
                        .with_context(|| format!("{} lacks {}", path.display(), String::from_utf8_lossy($name)))?
                };
            }
            Ok(Self {
                create_ctx: symbol!(b"krun_create_ctx", CreateCtx),
                init_log: symbol!(b"krun_init_log", InitLog),
                set_vm_config: symbol!(b"krun_set_vm_config", SetVmConfig),
                add_disk3: symbol!(b"krun_add_disk3", AddDisk3),
                set_root_disk_remount: symbol!(b"krun_set_root_disk_remount", SetRootDiskRemount),
                add_virtiofs3: symbol!(b"krun_add_virtiofs3", AddVirtiofs3),
                disable_implicit_vsock: symbol!(b"krun_disable_implicit_vsock", DisableImplicitVsock),
                add_vsock: symbol!(b"krun_add_vsock", AddVsock),
                add_vsock_port2: symbol!(b"krun_add_vsock_port2", AddVsockPort2),
                set_workdir: symbol!(b"krun_set_workdir", SetWorkdir),
                set_exec: symbol!(b"krun_set_exec", SetExec),
                set_console_output: symbol!(b"krun_set_console_output", SetConsoleOutput),
                get_shutdown_eventfd: library.get(b"krun_get_shutdown_eventfd").ok().map(|s| *s),
                start_enter: symbol!(b"krun_start_enter", StartEnter),
                _library: library,
            })
        }
    }

    pub fn create(&self) -> Result<u32> {
        let ctx = self.rc(unsafe { (self.create_ctx)() })?;
        u32::try_from(ctx).context("libkrun returned a negative context")
    }

    pub fn init_log(&self) -> Result<()> {
        // Errors only by default, no colour: the VMM's stderr is the task's
        // log. `KRUN_LOG_LEVEL=5` is the spike's window into boot failures.
        let level = std::env::var("KRUN_LOG_LEVEL").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
        self.rc(unsafe { (self.init_log)(2, level, 2, 0) })?;
        Ok(())
    }

    pub fn vm_config(&self, ctx: u32, vcpus: u8, ram_mib: u32) -> Result<()> {
        self.rc(unsafe { (self.set_vm_config)(ctx, vcpus, ram_mib) })?;
        Ok(())
    }

    pub fn disk(&self, ctx: u32, id: &str, path: &Path, direct: bool, sync: u32) -> Result<()> {
        let id = cstring(id)?;
        let path = cstring(&path.to_string_lossy())?;
        self.rc(unsafe { (self.add_disk3)(ctx, id.as_ptr(), path.as_ptr(), DISK_RAW, false, direct, sync) })?;
        Ok(())
    }

    pub fn root_disk(&self, ctx: u32, device: &str, fstype: &str) -> Result<()> {
        let device = cstring(device)?;
        let fstype = cstring(fstype)?;
        self.rc(unsafe { (self.set_root_disk_remount)(ctx, device.as_ptr(), fstype.as_ptr(), std::ptr::null()) })?;
        Ok(())
    }

    pub fn import(&self, ctx: u32, tag: &str, path: &Path) -> Result<()> {
        let tag = cstring(tag)?;
        let path = cstring(&path.to_string_lossy())?;
        self.rc(unsafe { (self.add_virtiofs3)(ctx, tag.as_ptr(), path.as_ptr(), 0, true) })?;
        Ok(())
    }

    /// Turn off libkrun's implicit vsock device. The default is implicit, and
    /// its implicit mode enables TSI's `HIJACK_INET` when the VM has no network
    /// device, which proxies the guest's AF_INET sockets through the host. Must
    /// be called before [`Krun::add_vsock`].
    pub fn disable_implicit_vsock(&self, ctx: u32) -> Result<()> {
        self.rc(unsafe { (self.disable_implicit_vsock)(ctx) })?;
        Ok(())
    }

    /// Add an explicit vsock device. `tsi_features` 0 means no transparent
    /// socket interception: the guest keeps AF_VSOCK and mapped ports, and has
    /// no IP egress. Call after [`Krun::disable_implicit_vsock`]; libkrun
    /// refuses while the context still has an implicit device.
    pub fn add_vsock(&self, ctx: u32, tsi_features: u32) -> Result<()> {
        self.rc(unsafe { (self.add_vsock)(ctx, tsi_features) })?;
        Ok(())
    }

    /// A path libkrun connects to when the guest connects out to `port`. The
    /// host side must already be listening on `socket`: the guest opens the
    /// ACP stream, so the connection also proves the agent is up.
    pub fn guest_vsock_port(&self, ctx: u32, port: u32, socket: &Path) -> Result<()> {
        let socket = cstring(&socket.to_string_lossy())?;
        self.rc(unsafe { (self.add_vsock_port2)(ctx, port, socket.as_ptr(), false) })?;
        Ok(())
    }

    pub fn workdir(&self, ctx: u32, dir: &str) -> Result<()> {
        let dir = cstring(dir)?;
        self.rc(unsafe { (self.set_workdir)(ctx, dir.as_ptr()) })?;
        Ok(())
    }

    pub fn exec(&self, ctx: u32, path: &str, argv: &[String], env: &[String]) -> Result<()> {
        let path = cstring(path)?;
        // Keep the owned strings alive until the call returns; the pointers
        // into them must not outlive this scope.
        let (argv_owned, argv) = cstrings(argv)?;
        let (env_owned, env) = cstrings(env)?;
        self.rc(unsafe { (self.set_exec)(ctx, path.as_ptr(), argv.as_ptr(), env.as_ptr()) })?;
        drop((argv_owned, env_owned));
        Ok(())
    }

    /// The guest console's output, and no guest input.
    pub fn console(&self, ctx: u32, path: &Path) -> Result<()> {
        let path = cstring(&path.to_string_lossy())?;
        self.rc(unsafe { (self.set_console_output)(ctx, path.as_ptr()) })?;
        Ok(())
    }

    /// An eventfd that asks the guest to power off. Writing any value to it
    /// triggers the guest's power key; it does not confirm the guest obeyed.
    /// Returns -1 when the host has no power-key device (x86_64 Linux); stop
    /// then relies on the VMM's grace instead.
    pub fn shutdown_eventfd(&self, ctx: u32) -> Result<i32> {
        let Some(get_shutdown_eventfd) = self.get_shutdown_eventfd else {
            return Ok(-1);
        };
        let fd = self.rc(unsafe { get_shutdown_eventfd(ctx) })?;
        if fd < 0 {
            bail!("libkrun returned shutdown fd {fd}");
        }
        Ok(fd)
    }

    /// Boot and run until the guest powers off. Blocks.
    pub fn start(&self, ctx: u32) -> Result<()> {
        self.rc(unsafe { (self.start_enter)(ctx) })?;
        Ok(())
    }

    fn rc(&self, rc: i32) -> Result<i32> {
        if rc < 0 {
            return Err(io::Error::from_raw_os_error(-rc)).context("libkrun");
        }
        Ok(rc)
    }
}

fn cstring(value: &str) -> Result<CString> {
    CString::new(value).with_context(|| format!("{value:?} contains a NUL byte"))
}

/// `argv`/`envp` are NULL-terminated. The returned pointers borrow the
/// returned `CString`s, so both must stay together.
fn cstrings(values: &[String]) -> Result<(Vec<CString>, Vec<*const c_char>)> {
    let owned = values.iter().map(|v| cstring(v)).collect::<Result<Vec<_>>>()?;
    let mut pointers: Vec<*const c_char> = owned.iter().map(|s| s.as_ptr()).collect();
    pointers.push(std::ptr::null());
    Ok((owned, pointers))
}
