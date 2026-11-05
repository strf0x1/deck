//! The gateway's listeners: one Unix socket per boundary kind, bound by the
//! runtime's VMM and mapped to a guest vsock port.
//!
//! The design choice: the listener set is fixed at process start because the
//! VM's port map is. The web and package gateway gets a socket of its own,
//! because a CONNECT has to reach a proxy on the port the task's proxy
//! environment names; the reserved extension port accepts a framed kind, where
//! `web` and `boundary` both answer, and an unknown kind is refused. Adding a
//! kind later is a gateway update that running tasks pick up without a VM
//! restart.
//!
//! One web socket serves both shapes a client sends: a proxy client writes
//! `CONNECT host:port HTTP/1.1`, a mirror client writes `GET http://host/…`.
//! The first request line decides which handler sees the connection, and the
//! bytes read for that decision are handed to it, never read twice.
//!
//! The process fails closed: any listener that stops returns an error, and the
//! VMM treats the gateway's exit as the task's failure.

use crate::control::Control;
use crate::credential::Broker;
use crate::github::GitHub;
use crate::github::status::Events as GitHubEvents;
use crate::grants::Grants;
use crate::http;
use crate::model::{Model, Settings};
use crate::policy::{Budget, Models, Usage};
use crate::status::Status;
use crate::web::connect::Connector;
use crate::web::policy::Profiles;
use crate::web::resolve::Resolver;
use crate::web::status::Events as WebEvents;
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::{self, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
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
/// The web and package kind, served on its own port and on the extension port.
pub const WEB_KIND: &str = crate::web::KIND;
/// The GitHub operation kind, served on its own port and on the extension port.
pub const GITHUB_KIND: &str = crate::github::KIND;

#[derive(Debug, Clone)]
pub struct Config {
    /// Provider base URL including `/v1`, e.g. `https://openrouter.ai/api/v1`.
    pub upstream: String,
    pub models: Vec<String>,
    /// The credential name the provider key is fetched under, per request,
    /// from `deckd` (ADR 0004 §4.4).
    pub secret: String,
    /// The daemon queue that decides a model the task was not configured with.
    pub control: Option<Control>,
    /// Host identities and persisted grant files, independent of web egress.
    pub task: String,
    pub grants: Vec<PathBuf>,
    pub project: Option<String>,
    /// Process-start limits; `budget_file` overrides them when present.
    pub budget: Budget,
    /// Counts already on the task's record, so a resumed gateway continues
    /// rather than resetting the budget.
    pub usage: Usage,
    /// The task's live budget, rewritten by `deckd`.
    pub budget_file: Option<PathBuf>,
    pub model_socket: PathBuf,
    pub extension_socket: PathBuf,
    /// The web and package gateway. `None` serves no web traffic: the task
    /// has no proxy and no mirror read, which is the closed default.
    pub web: Option<WebSettings>,
    /// The GitHub operation gateway. `None` serves no publication: the task's
    /// project names no repository, and the guest gets no route.
    pub github: Option<GitHubSettings>,
}

/// The GitHub gateway's policy: the repository, the refs and the project rule
/// file. The guest can name an operation; the rest is host state.
#[derive(Debug, Clone)]
pub struct GitHubSettings {
    pub socket: PathBuf,
    /// The API base a test points at its own listener; production is
    /// `https://api.github.com`.
    pub api: String,
    /// The clone URL a publish pushes to.
    pub repository_url: String,
    pub policy: crate::github::policy::Policy,
    /// The task this gateway serves. Every branch rule is derived from it, so
    /// it is required even when no approval queue is configured.
    pub task: String,
    /// The credential name the token is fetched under, once per operation,
    /// from `deckd`. Never written anywhere.
    pub secret: String,
    /// The gateway's own scratch directory for the bundle it fetches.
    pub scratch: PathBuf,
    /// A `git` other than the one on `PATH`.
    pub git: Option<PathBuf>,
}

/// The web and package gateway's policy. The guest can name a profile and a
/// destination; the table, the grant files and the daemon stay host state.
#[derive(Debug, Clone)]
pub struct WebSettings {
    pub socket: PathBuf,
    /// The profiles the task may use without asking, by name.
    pub profiles: Vec<String>,
    /// A reserved name for a test or a task whose destinations are pinned;
    /// production resolution is the host's own.
    pub hosts: Option<PathBuf>,
    /// Where the dial goes after the policy admitted a resolved address. Only
    /// a test sets it, so the tunnel path is provable without a route to a
    /// public host; the binary never passes it.
    pub test_connect: Option<std::net::IpAddr>,
}

/// The startup line the VMM waits for. It names the sockets so the VMM can
/// verify the map it is about to install.
#[derive(Debug, Serialize, Deserialize)]
pub struct Ready {
    pub model: PathBuf,
    pub extension: PathBuf,
    /// Names the web socket when the task has one, so the VMM can verify the
    /// map it installs rather than trusting the argument it passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<PathBuf>,
}

/// Bind both sockets and announce readiness, then serve until a listener
/// fails. The announcement comes from this function so a caller cannot map a
/// socket that is not listening yet.
pub fn serve(config: Config) -> Result<()> {
    let model_listener = bind(&config.model_socket)?;
    let extension_listener = bind(&config.extension_socket)?;
    let web_listener = match &config.web {
        Some(web) => Some(bind(&web.socket)?),
        None => None,
    };
    let github_listener = match &config.github {
        Some(github) => Some(bind(&github.socket)?),
        None => None,
    };
    let ready = Ready {
        model: config.model_socket.clone(),
        extension: config.extension_socket.clone(),
        web: config.web.as_ref().map(|web| web.socket.clone()),
        github: config.github.as_ref().map(|github| github.socket.clone()),
    };
    println!("{}", serde_json::to_string(&ready)?);
    io::stdout().flush().context("announcing gateway readiness")?;

    let budget = config.budget_file.as_ref().and_then(|path| Budget::load(path)).unwrap_or(config.budget);
    let status = Status::new(config.control.clone());
    let grants = Arc::new(Grants::new(config.grants, config.task, config.project));
    grants.watch().context("starting the live grant monitor")?;
    let web = web_settings(config.web.as_ref(), config.control.clone(), grants.clone())?;
    let events = Arc::new(WebEvents::new());
    let web = web.map(|connector| Arc::new(connector.with_events(events.clone())));
    // One broker for the process: the model and GitHub operations register
    // with it, and its watch closes the ones whose value is revoked.
    let credentials = Broker::new(config.control.clone());
    credentials.watch().context("starting the credential watch")?;
    let github = github_settings(config.github.as_ref(), config.control.clone(), credentials.clone());
    let model = Arc::new(Model::new(Settings {
        upstream: config.upstream,
        models: Models::new(config.models),
        grants,
        credentials,
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
        let web = web.clone();
        let github = github.clone();
        move || {
            let kinds = Kinds { status, web, events, github };
            let reason = accept(extension_listener, limits, move |stream| kinds.extension(stream))
                .err()
                .map(|error| format!("{error:#}"));
            let _ = failed.send(("extension", reason));
        }
    })?;
    if let (Some(listener), Some(connector)) = (web_listener, web.clone()) {
        std::thread::Builder::new().name("gateway-web".into()).spawn({
            let limits = limits.clone();
            let failed = failed.clone();
            move || {
                let reason = accept(listener, limits, move |stream| Kinds::web(stream, connector.clone()))
                    .err()
                    .map(|error| format!("{error:#}"));
                let _ = failed.send(("web", reason));
            }
        })?;
    }
    if let (Some(listener), Some(gateway)) = (github_listener, github.clone()) {
        std::thread::Builder::new().name("gateway-github".into()).spawn({
            let limits = limits.clone();
            let failed = failed.clone();
            move || {
                let reason = accept(listener, limits, move |stream| gateway.handle(stream))
                    .err()
                    .map(|error| format!("{error:#}"));
                let _ = failed.send(("github", reason));
            }
        })?;
    }
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

/// The web gateway's host state, or `None` when the task was started without
/// one. An undeclared profile is a startup error rather than a silently
/// narrower or wider policy.
fn web_settings(
    settings: Option<&WebSettings>,
    control: Option<Control>,
    grants: Arc<Grants>,
) -> Result<Option<Connector>> {
    let Some(settings) = settings else { return Ok(None) };
    // The web gateway asks for destinations only; the model jobs (usage,
    // waiting) are the model gateway's, so it takes the destination half.
    let control =
        control.map(|control| crate::web::Control::new(control.socket().to_owned(), control.task().to_owned()));
    let profiles = Profiles::declared(settings.profiles.iter().cloned()).map_err(anyhow::Error::msg)?;
    Ok(Some(Connector {
        resolver: match settings.test_connect {
            Some(address) => Resolver::at(settings.hosts.as_deref()).redirect_to(address),
            None => Resolver::at(settings.hosts.as_deref()),
        },
        grants,
        profiles: Arc::new(profiles),
        control,
        status: Arc::new(WebEvents::new()),
    }))
}

/// The GitHub gateway's host state, or `None` when the task's project names no
/// repository. No token is read here: each operation fetches one from `deckd`
/// after its policy check, and one `deckd` cannot supply refuses that
/// operation instead of the gateway's start.
fn github_settings(
    settings: Option<&GitHubSettings>,
    control: Option<Control>,
    credentials: Arc<Broker>,
) -> Option<Arc<GitHub>> {
    let settings = settings?;
    let control =
        control.map(|control| crate::github::Control::new(control.socket().to_owned(), control.task().to_owned()));
    let client = crate::github::Client::new(crate::github::client::Settings {
        api: settings.api.clone(),
        policy: settings.policy.clone(),
        task: settings.task.clone(),
        repository_url: settings.repository_url.clone(),
        token: None,
        scratch: settings.scratch.clone(),
        git: settings.git.clone(),
    });
    Some(Arc::new(
        GitHub::new(client, control, Arc::new(GitHubEvents::new()))
            .with_credentials(credentials, settings.secret.clone()),
    ))
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

/// The handlers a framed connection may reach, with the state they share.
struct Kinds {
    status: Arc<Status>,
    web: Option<Arc<Connector>>,
    events: Arc<WebEvents>,
    github: Option<Arc<GitHub>>,
}

impl Kinds {
    /// The extension port's dispatch: the framed kind names the gateway the
    /// connection wants. The frame is read without a `BufReader`, because a
    /// buffered read could consume the first HTTP bytes and strand them from
    /// the handler.
    fn extension(&self, mut stream: UnixStream) -> io::Result<()> {
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
        match (kind.as_ref(), &self.web, &self.github) {
            (BOUNDARY_KIND, ..) => self.status.handle(stream, &self.events),
            (WEB_KIND, Some(web), _) => Kinds::web(stream, web.clone()),
            (WEB_KIND, None, _) => answer(&mut stream, 1, "this task has no web gateway"),
            (GITHUB_KIND, _, Some(github)) => github.handle(stream),
            (GITHUB_KIND, _, None) => answer(&mut stream, 1, "this task's project names no GitHub repository"),
            _ => answer(&mut stream, 1, &format!("unsupported kind {kind:?}")),
        }
    }
}

impl Kinds {
    /// The web socket's dispatch. A proxy client's first bytes are either
    /// `CONNECT host:port` or an absolute-URI request (`GET http://…`); the
    /// method alone does not tell them apart, so the peek waits for the first
    /// space and looks at what follows. The bytes are only peeked, so
    /// whichever handler runs reads the request once.
    fn web(mut stream: UnixStream, web: Arc<Connector>) -> io::Result<()> {
        match target(&stream) {
            Some(Target::Absolute) => web.handle_plain(stream),
            Some(Target::NotUtf8) => {
                http::write_error(&mut stream, 400, "deck_bad_request", "the request target is not UTF-8")
            }
            _ => web.handle(stream),
        }
    }
}

/// What the first line says the connection wants.
enum Target {
    /// `GET http://host/path`: a mirror read.
    Absolute,
    /// `CONNECT host:port`, or anything the tunnel handler should refuse.
    Tunnel,
    /// A request line whose target is not UTF-8.
    NotUtf8,
}

/// Whether the request line's target is an absolute `http://` URI, which only
/// a plain-HTTP proxy request has. `None` is a closed peer; the caller then
/// hands the stream to the tunnel handler, which reports the EOF.
fn target(stream: &UnixStream) -> Option<Target> {
    const LINE: usize = 96;
    let mut buffer = [0u8; LINE];
    let mut tries = 0;
    while tries < 4 {
        tries += 1;
        // SAFETY: one initialised buffer with the length passed.
        let read = unsafe { libc::recv(stream.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len(), libc::MSG_PEEK) };
        if read <= 0 {
            return None;
        }
        let Ok(read) = usize::try_from(read) else { return None };
        let peeked = &buffer[..read];
        // The first request line is one small write; before its space arrives
        // there is no method, and guessing would misroute the connection.
        let Some(space) = peeked.iter().position(|&byte| byte == b' ') else {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        };
        return Some(match std::str::from_utf8(&peeked[space + 1..]) {
            Ok(rest) if rest.starts_with("http://") => Target::Absolute,
            Ok(_) => Target::Tunnel,
            Err(_) => Target::NotUtf8,
        });
    }
    // No space within the first bytes is not a request this gateway speaks;
    // the tunnel handler refuses it with the same bounded answer.
    Some(Target::Tunnel)
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
        let kinds = Kinds { status: Status::new(None), web: None, events: Arc::new(WebEvents::new()), github: None };
        kinds.extension(right).unwrap();
        let body = writer.join().unwrap();
        assert!(body.contains("unsupported kind"), "{body}");
    }
}
