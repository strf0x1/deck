//! The gateway's listeners: one Unix socket per boundary kind, bound by the
//! runtime's VMM and mapped to a guest vsock port.
//!
//! The design choice: the listener set is fixed at process start because the
//! VM's port map is. The reserved extension port accepts a framed kind: the
//! boundary status endpoint is the first, and an unknown kind is refused.
//! Adding another gateway kind later is a gateway update that running tasks
//! pick up without a VM restart.
//!
//! The process fails closed: any listener that stops returns an error, and the
//! VMM treats the gateway's exit as the task's failure.

use crate::control::Control;
use crate::model::{Model, Settings};
use crate::policy::{Budget, Models, Usage};
use crate::status::Status;
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

/// The most concurrent guest connections one kind accepts. Excess connections
/// wait in the listener's backlog, which bounds the gateway's threads.
const CONNECTIONS: usize = 64;
/// The extension port's kind header is a 4-byte big-endian length plus its
/// UTF-8 name. Four KiB is far more than any kind needs.
const MAX_KIND: usize = 4 * 1024;
/// The first kind on the extension port: the boundary status endpoint. The
/// guest relay writes this name before forwarding plain HTTP.
pub const BOUNDARY_KIND: &str = "boundary";

#[derive(Debug, Clone)]
pub struct Config {
    /// Provider base URL including `/v1`, e.g. `https://openrouter.ai/api/v1`.
    pub upstream: String,
    pub models: Vec<String>,
    /// The host-side key for `upstream`. Never written anywhere.
    pub key: Option<String>,
    /// The name the key came from, for a denial message.
    pub secret: String,
    /// The daemon queue that decides a model the task was not configured with.
    pub control: Option<Control>,
    /// Process-start limits; `budget_file` overrides them when present.
    pub budget: Budget,
    /// Counts already on the task's record, so a resumed gateway continues
    /// rather than resetting the budget.
    pub usage: Usage,
    /// The task's live budget, rewritten by `deckd`.
    pub budget_file: Option<PathBuf>,
    pub model_socket: PathBuf,
    pub extension_socket: PathBuf,
}

/// The startup line the VMM waits for. It names the sockets so the VMM can
/// verify the map it is about to install.
#[derive(Debug, Serialize, Deserialize)]
pub struct Ready {
    pub model: PathBuf,
    pub extension: PathBuf,
}

/// Bind both sockets and announce readiness, then serve until a listener
/// fails. The announcement comes from this function so a caller cannot map a
/// socket that is not listening yet.
pub fn serve(config: Config) -> Result<()> {
    let model_listener = bind(&config.model_socket)?;
    let extension_listener = bind(&config.extension_socket)?;
    let ready = Ready { model: config.model_socket.clone(), extension: config.extension_socket.clone() };
    println!("{}", serde_json::to_string(&ready)?);
    io::stdout().flush().context("announcing gateway readiness")?;

    let budget = config.budget_file.as_ref().and_then(|path| Budget::load(path)).unwrap_or(config.budget);
    let status = Status::new(config.control.clone());
    let model = Arc::new(Model::new(Settings {
        upstream: config.upstream,
        models: Models::new(config.models),
        key: config.key,
        secret: config.secret,
        control: config.control,
        status: status.clone(),
        budget,
        usage: config.usage,
        budget_file: config.budget_file,
    }));
    let limits = Arc::new(Limit::new(CONNECTIONS));
    let (failed, failure) = mpsc::channel();

    std::thread::Builder::new().name("gateway-extension".into()).spawn({
        let failed = failed.clone();
        let limits = limits.clone();
        move || {
            let reason = accept(extension_listener, limits, move |stream| extension(stream, status.clone()))
                .err()
                .map(|error| format!("{error:#}"));
            let _ = failed.send(("extension", reason));
        }
    })?;
    std::thread::Builder::new().name("gateway-model".into()).spawn({
        let limits = limits.clone();
        move || {
            let reason = accept(model_listener, limits, move |stream| model.handle(stream))
                .err()
                .map(|error| format!("{error:#}"));
            let _ = failed.send(("model", reason));
        }
    })?;

    match failure.recv() {
        Ok((kind, Some(reason))) => bail!("the {kind} listener stopped: {reason}"),
        Ok((kind, None)) => bail!("the {kind} listener stopped"),
        Err(_) => bail!("both listeners stopped"),
    }
}

fn bind(path: &Path) -> Result<UnixListener> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("removing the stale socket {}", path.display())),
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding the gateway socket {}", path.display()))?;
    // The VMM runs as the same user, and libkrun connects to this path. No
    // other user on the host needs it.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn accept<F>(listener: UnixListener, limits: Arc<Limit>, handle: F) -> Result<()>
where
    F: Fn(UnixStream) -> io::Result<()> + Send + Sync + 'static,
{
    let handle = Arc::new(handle);
    loop {
        // A slot is taken before accept, so the backlog provides backpressure
        // instead of an unbounded thread per connection.
        let slot = limits.acquire();
        let (stream, _) = listener.accept()?;
        let handle = handle.clone();
        std::thread::Builder::new().name("gateway-connection".into()).spawn(move || {
            let _slot = slot;
            if let Err(error) = handle(stream) {
                // A client that goes away mid-request is normal; a handler
                // refusal is answered inside the handler and is not an error.
                if error.kind() != io::ErrorKind::BrokenPipe && error.kind() != io::ErrorKind::ConnectionReset {
                    eprintln!("deck-gateway: connection ended: {error}");
                }
            }
        })?;
    }
}

/// The extension port's dispatch: the framed kind names the gateway the
/// connection wants. The boundary kind speaks HTTP from that point on; any
/// other kind is refused, and no request on this port can allow anything.
///
/// The frame is read without a `BufReader`, because a buffered read could
/// consume the first HTTP bytes and strand them from the status handler.
fn extension(mut stream: UnixStream, status: Arc<Status>) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(MAX_WAIT_SECS + 5)))?;
    let mut length = [0u8; 4];
    (&stream).read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_KIND {
        return answer(&mut stream, 0, "kind header too large");
    }
    let mut kind = vec![0; length];
    (&stream).read_exact(&mut kind)?;
    let kind = String::from_utf8_lossy(&kind);
    if kind == BOUNDARY_KIND {
        return status.handle(stream);
    }
    answer(&mut stream, 1, &format!("unsupported kind {kind:?}"))
}

/// Matches the status endpoint's host cap; the listener's write timeout must
/// outlast a long poll.
const MAX_WAIT_SECS: u64 = 300;

fn answer(stream: &mut UnixStream, error: u8, message: &str) -> io::Result<()> {
    let body = serde_json::json!({ "error": error, "message": message }).to_string();
    stream.write_all(&u32::try_from(body.len()).unwrap_or(0).to_be_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// Counts connections without a thread per waiter beyond the fixed set of
/// handler threads the acceptor spawns.
struct Limit {
    capacity: usize,
    active: Mutex<usize>,
    free: Condvar,
}

impl Limit {
    fn new(capacity: usize) -> Self {
        Self { capacity, active: Mutex::new(0), free: Condvar::new() }
    }

    fn acquire(self: &Arc<Self>) -> Slot {
        let mut active = self.active.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while *active >= self.capacity {
            active = self.free.wait(active).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *active += 1;
        Slot { limit: self.clone() }
    }
}

struct Slot {
    limit: Arc<Limit>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut active = self.limit.active.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = active.saturating_sub(1);
        self.limit.free.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_is_released_when_its_connection_ends() {
        let limit = Arc::new(Limit::new(1));
        let first = limit.acquire();
        drop(first);
        let second = limit.acquire();
        drop(second);
    }

    #[test]
    fn the_kind_answer_is_length_framed_json() {
        let (mut left, right) = UnixStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            left.write_all(&4u32.to_be_bytes()).unwrap();
            left.write_all(b"test").unwrap();
            let mut length = [0u8; 4];
            left.read_exact(&mut length).unwrap();
            let mut body = vec![0; u32::from_be_bytes(length) as usize];
            left.read_exact(&mut body).unwrap();
            String::from_utf8(body).unwrap()
        });
        extension(right, Status::new(None)).unwrap();
        let body = writer.join().unwrap();
        assert!(body.contains("unsupported kind"), "{body}");
    }
}
