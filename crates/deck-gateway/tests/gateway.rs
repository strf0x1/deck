//! The real gateway binary against a fake provider on host loopback. These
//! exercise the boundary the guest sees: a Unix socket, a guest request with
//! its own credential headers, and a host-side key added only after they were
//! discarded. Every gateway runs behind a fake `deckd` ([`Deckd`]) that hands
//! out credential values the way the broker does; the gateway's own
//! environment carries a decoy value that must never be used.

#![expect(clippy::unwrap_used, reason = "test fixture setup must succeed")]

use deck_gateway::http::{self, Request};
use deck_gateway::server::Ready;
use serde_json::Value;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The decoy every gateway's environment carries under each credential name.
/// A request that ever carries it read the environment, which ADR 0004 §4.2
/// forbids.
const DECOY: &str = "env-value-must-not-be-used";

#[path = "gateway/mirror_body.rs"]
mod mirror_body;

#[path = "gateway/mirror_redirect.rs"]
mod mirror_redirect;

#[path = "gateway/model_grants.rs"]
mod model_grants;

#[path = "gateway/identity.rs"]
mod identity;

/// A fake `deckd` in front of a test's own queue fixture: it answers
/// `credential` and `credential_watch` as the broker does, and forwards every
/// other line to `upstream`, so each fixture stays one concern.
#[derive(Clone)]
struct Deckd {
    socket: PathBuf,
    keys: Arc<Mutex<Keys>>,
}

#[derive(Default)]
struct Keys {
    values: std::collections::HashMap<String, String>,
    revoked: Vec<(String, String)>,
    watchers: Vec<UnixStream>,
    fetched: Vec<String>,
    down: bool,
}

/// An opaque stand-in for the broker's fingerprint; the gateway never looks
/// inside one.
fn fingerprint(value: &str) -> String {
    let hash = value.bytes().fold(0xcbf29ce484222325u64, |n, b| (n ^ u64::from(b)).wrapping_mul(0x100000001b3));
    format!("sha256:{:012x}", hash & 0xffff_ffff_ffff)
}

impl Deckd {
    fn start(dir: &Path, values: &[(&str, &str)], upstream: Option<&Path>) -> Self {
        let socket = dir.join("front.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let keys = Arc::new(Mutex::new(Keys {
            values: values.iter().map(|(name, value)| ((*name).to_string(), (*value).to_string())).collect(),
            ..Keys::default()
        }));
        let upstream = upstream.map(Path::to_path_buf);
        let shared = keys.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let keys = shared.clone();
                let upstream = upstream.clone();
                std::thread::spawn(move || serve_front(stream, &keys, upstream.as_deref()));
            }
        });
        Self { socket, keys }
    }

    fn set(&self, name: &str, value: &str) {
        self.keys.lock().unwrap().values.insert(name.into(), value.into());
    }

    /// Revoke `name`'s current value and push it to every watching gateway.
    fn revoke(&self, name: &str) -> String {
        let mut keys = self.keys.lock().unwrap();
        let fingerprint = fingerprint(&keys.values[name]);
        keys.revoked.push((name.into(), fingerprint.clone()));
        let line = format!("{}\n", serde_json::json!({ "name": name, "fingerprint": fingerprint, "at": 0 }));
        keys.watchers.retain_mut(|watcher| watcher.write_all(line.as_bytes()).is_ok());
        fingerprint
    }

    fn fetched(&self) -> Vec<String> {
        self.keys.lock().unwrap().fetched.clone()
    }

    /// Close every connection at once from now on, as a stopped daemon would.
    fn stop(&self) {
        let mut keys = self.keys.lock().unwrap();
        keys.down = true;
        for watcher in keys.watchers.drain(..) {
            let _ = watcher.shutdown(std::net::Shutdown::Both);
        }
    }

    fn resume(&self) {
        self.keys.lock().unwrap().down = false;
    }

    /// The `--control` arguments that put this fake in front of the gateway.
    fn args(&self, command: &mut Command, task: &str) {
        command.arg("--control").arg(&self.socket).arg("--task").arg(task);
    }
}

fn serve_front(mut stream: UnixStream, keys: &Mutex<Keys>, upstream: Option<&Path>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if keys.lock().unwrap().down || reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let Ok(request) = serde_json::from_str::<Value>(&line) else { return };
    let reply = match request["op"].as_str() {
        Some("credential") => {
            let name = request["name"].as_str().unwrap_or_default().to_string();
            let mut keys = keys.lock().unwrap();
            keys.fetched.push(name.clone());
            match keys.values.get(&name) {
                None => {
                    serde_json::json!({ "error": format!("deck_credential_missing: the host has no value for {name}") })
                }
                Some(value) if keys.revoked.iter().any(|(_, revoked)| *revoked == fingerprint(value)) => {
                    serde_json::json!({ "error": format!("deck_credential_revoked: {name} was revoked") })
                }
                Some(value) => serde_json::json!({ "ok": { "value": value, "fingerprint": fingerprint(value) } }),
            }
        }
        Some("credential_watch") => {
            let mut keys = keys.lock().unwrap();
            let revoked: Vec<Value> = keys
                .revoked
                .iter()
                .map(|(name, fingerprint)| serde_json::json!({ "name": name, "fingerprint": fingerprint, "at": 0 }))
                .collect();
            let _ = writeln!(stream, "{}", serde_json::json!({ "ok": { "revoked": revoked } }));
            keys.watchers.push(stream);
            return;
        }
        _ => match upstream.and_then(|path| UnixStream::connect(path).ok()) {
            Some(mut queue) => {
                let _ = queue.write_all(line.as_bytes());
                let mut answer = String::new();
                let _ = BufReader::new(queue).read_line(&mut answer);
                if answer.is_empty() {
                    return;
                }
                let _ = stream.write_all(answer.as_bytes());
                return;
            }
            None => serde_json::json!({ "error": "the approval queue is unreachable: no queue in this test" }),
        },
    };
    let _ = writeln!(stream, "{reply}");
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("deck-gateway-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[derive(Clone, Copy, Default)]
struct Limits {
    requests: Option<u64>,
    tokens: Option<u64>,
    used_requests: u64,
    used_tokens: u64,
}

fn start(name: &str, upstream: &str, models: &[&str], key: Option<&str>) -> Gateway {
    start_with(name, upstream, models, key, None)
}

fn start_with(
    name: &str,
    upstream: &str,
    models: &[&str],
    key: Option<&str>,
    control: Option<(&Path, &str)>,
) -> Gateway {
    start_full(name, upstream, models, key, control, Limits::default(), None)
}

fn start_full(
    name: &str,
    upstream: &str,
    models: &[&str],
    key: Option<&str>,
    control: Option<(&Path, &str)>,
    limits: Limits,
    budget_file: Option<&Path>,
) -> Gateway {
    start_launch(name, upstream, models, key, Launch { control, limits, budget_file, ..Launch::default() })
}

#[derive(Default)]
struct Launch<'a> {
    control: Option<(&'a Path, &'a str)>,
    limits: Limits,
    budget_file: Option<&'a Path>,
    grants: Option<&'a Path>,
}

fn start_launch(name: &str, upstream: &str, models: &[&str], key: Option<&str>, launch: Launch<'_>) -> Gateway {
    let Launch { control, limits, budget_file, grants } = launch;
    let dir = scratch(name);
    let values: Vec<(&str, &str)> = key.map(|key| ("DECK_GATEWAY_TEST_KEY", key)).into_iter().collect();
    let deckd = Deckd::start(&dir, &values, control.map(|(socket, _)| socket));
    let mut command = Command::new(env!("CARGO_BIN_EXE_deck-gateway"));
    command
        .arg("--model-socket")
        .arg(dir.join("model.sock"))
        .arg("--extension-socket")
        .arg(dir.join("extension.sock"))
        .arg("--upstream")
        .arg(upstream)
        .arg("--secret")
        .arg("DECK_GATEWAY_TEST_KEY")
        .arg("--used-requests")
        .arg(limits.used_requests.to_string())
        .arg("--used-tokens")
        .arg(limits.used_tokens.to_string())
        .arg("--exit-on-stdin-eof")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for model in models {
        command.args(["--model", model]);
    }
    if let Some(limit) = limits.requests {
        command.arg("--request-budget").arg(limit.to_string());
    }
    if let Some(limit) = limits.tokens {
        command.arg("--token-budget").arg(limit.to_string());
    }
    if let Some(path) = budget_file {
        command.arg("--budget-file").arg(path);
    }
    if let Some(path) = grants {
        command.arg("--grants").arg(path).args(["--project", "/fixture"]);
    }
    deckd.args(&mut command, control.map_or("task-1", |(_, task)| task));
    command.env("DECK_GATEWAY_TEST_KEY", DECOY);
    let mut child = command.spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let ready: Ready = serde_json::from_str(line.trim()).unwrap_or_else(|error| panic!("{error}: {line:?}"));
    // The gateway runs with `--exit-on-stdin-eof`, so the handle is leaked on
    // purpose: dropping it here would stop the process mid-test.
    std::mem::forget(stdin);
    Gateway { child, ready, deckd }
}

struct Gateway {
    child: Child,
    ready: Ready,
    deckd: Deckd,
}

/// One connection to the fake provider. `assert` runs against the request the
/// provider saw; the reply is a fixed JSON body.
fn provider(assert: impl Fn(&Request) + Send + 'static) -> (u16, std::thread::JoinHandle<()>) {
    provider_with("text/event-stream", r#"{"id":"chatcmpl-1","choices":[{"delta":{"content":"ok"}}]}"#, assert)
}

fn provider_with(
    content_type: &'static str,
    body: &'static str,
    assert: impl Fn(&Request) + Send + 'static,
) -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut stream = stream;
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let request = http::read(&mut BufReader::new(&stream)).unwrap().unwrap();
        assert(&request);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        stream.flush().unwrap();
    });
    (port, handle)
}

fn send(socket: &PathBuf, request: &str) -> String {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn the_host_key_replaces_guest_credentials_and_responses_stream() {
    let (port, provider) = provider(|request| {
        assert_eq!(request.target, "/v1/chat/completions");
        assert_eq!(request.header("authorization"), Some("Bearer host-key"));
        assert_eq!(request.header("x-api-key"), None);
        assert_eq!(request.header("cookie"), None);
        assert_eq!(request.header("http-referer"), Some("https://deck.example"));
        assert!(request.header("x-invented").is_none());
        assert_eq!(request.body, br#"{"model":"allowed","messages":[]}"#);
    });
    let gateway = start("inject", &format!("http://127.0.0.1:{port}/v1"), &["allowed"], Some("host-key"));

    let body = r#"{"model":"allowed","messages":[]}"#;
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: relay\r\ncontent-type: application/json\r\n\
         authorization: Bearer deck-placeholder\r\nx-api-key: guest\r\ncookie: a=b\r\n\
         http-referer: https://deck.example\r\nx-invented: nope\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let response = send(&gateway.ready.model, &request);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("transfer-encoding: chunked"), "{response}");
    assert!(response.contains("chatcmpl-1"), "{response}");
    provider.join().unwrap();
}

#[test]
fn a_disallowed_model_is_denied_before_the_provider() {
    let gateway = start("denied", "http://127.0.0.1:9/v1", &["allowed"], Some("host-key"));
    let body = r#"{"model":"other","messages":[]}"#;
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let response = send(&gateway.ready.model, &request);
    assert!(response.starts_with("HTTP/1.1 403 Forbidden"), "{response}");
    assert!(response.contains("deck_boundary_denied"), "{response}");
    assert!(response.contains("other"), "{response}");
}

#[test]
fn paths_outside_the_model_surface_are_denied() {
    let gateway = start("paths", "http://127.0.0.1:9/v1", &["allowed"], Some("host-key"));
    let response = send(&gateway.ready.model, "POST /v1/embeddings HTTP/1.1\r\ncontent-length: 0\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 403 Forbidden"), "{response}");
}

#[test]
fn a_missing_host_key_denies_instead_of_forwarding_the_placeholder() {
    let gateway = start("no-key", "http://127.0.0.1:9/v1", &["allowed"], None);
    let body = r#"{"model":"allowed","messages":[]}"#;
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let response = send(&gateway.ready.model, &request);
    assert!(response.starts_with("HTTP/1.1 503 Service Unavailable"), "{response}");
    assert!(response.contains("deck_credential_missing"), "{response}");
}

#[test]
fn the_extension_port_answers_every_kind_as_unsupported() {
    let gateway = start("extension", "http://127.0.0.1:9/v1", &["allowed"], Some("host-key"));
    let mut stream = UnixStream::connect(&gateway.ready.extension).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.write_all(&7u32.to_be_bytes()).unwrap();
    stream.write_all(b"ssh-key").unwrap();
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).unwrap();
    let mut body = vec![0; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut body).unwrap();
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("unsupported kind"), "{body}");
    assert!(body.contains("ssh-key"), "{body}");
}

/// One connection to the extension port: the framed kind, then the request.
fn send_kind(socket: &PathBuf, kind: &str, request: &str) -> String {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.write_all(&u32::try_from(kind.len()).unwrap().to_be_bytes()).unwrap();
    stream.write_all(kind.as_bytes()).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

/// A fake daemon that answers any number of connections from one closure.
/// The closure sees each request and returns its `ok` value.
fn interactive_queue(
    name: &str,
    handler: impl Fn(&Value) -> Value + Send + Sync + 'static,
) -> (PathBuf, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
    let dir = scratch(name);
    let socket = dir.join("deckd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    let handler = std::sync::Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handler = handler.clone();
            let log = log.clone();
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                log.lock().unwrap().push(request.clone());
                let reply = serde_json::to_string(&serde_json::json!({ "ok": handler(&request) })).unwrap();
                stream.write_all(reply.as_bytes()).unwrap();
                stream.write_all(b"\n").unwrap();
                stream.flush().unwrap();
            });
        }
    });
    (socket, seen)
}

#[test]
fn the_boundary_kind_serves_events_and_long_polls_a_decision() {
    let (socket, seen) = interactive_queue("boundary-status", |request| match request["op"].as_str() {
        Some("approval") => serde_json::json!({ "id": "a1", "destination": "other", "state": "pending" }),
        Some("approval_status") => {
            serde_json::json!({ "id": "a1", "destination": "other", "state": "pending" })
        }
        // The daemon owns the wait; its reply is the decision.
        Some("approval_wait") => serde_json::json!({
            "id": "a1", "destination": "other", "state": { "allowed": "task" }
        }),
        _ => serde_json::Value::Null,
    });
    let gateway =
        start_with("boundary-kind", "http://127.0.0.1:9/v1", &["allowed"], Some("host-key"), Some((&socket, "t")));

    let response = send(&gateway.ready.model, &chat("other"));
    assert!(response.contains(r#""approval_id":"a1""#), "{response}");

    let events = send_kind(&gateway.ready.extension, "boundary", "GET /events?after=0 HTTP/1.1\r\nhost: relay\r\n\r\n");
    assert!(events.starts_with("HTTP/1.1 200 OK"), "{events}");
    assert!(events.contains(r#""approval_id":"a1""#), "{events}");
    assert!(events.contains(r#""state":"pending""#), "{events}");

    // The long poll is one bounded daemon call; its decidable reply is the
    // response, and the daemon marks the wait on the item for the card.
    let waited =
        send_kind(&gateway.ready.extension, "boundary", "GET /approvals/a1?wait=5 HTTP/1.1\r\nhost: relay\r\n\r\n");
    assert!(waited.starts_with("HTTP/1.1 200 OK"), "{waited}");
    assert!(waited.contains(r#""state":"allowed""#), "{waited}");
    assert!(waited.contains(r#""scope":"task""#), "{waited}");
    let seen = seen.lock().unwrap();
    assert!(seen.iter().any(|r| r["op"] == "approval_wait" && r["wait_ms"] == 5000), "{seen:?}");
}

#[test]
fn the_boundary_kind_withdraws_and_refuses_unknown_items() {
    let (socket, seen) = interactive_queue("boundary-withdraw", |request| match request["op"].as_str() {
        Some("withdraw_approval") => serde_json::Value::Null,
        Some("approval_status") => serde_json::Value::Null,
        _ => serde_json::Value::Null,
    });
    let gateway =
        start_with("withdraw-kind", "http://127.0.0.1:9/v1", &["allowed"], Some("host-key"), Some((&socket, "t")));
    let withdrawn = send_kind(
        &gateway.ready.extension,
        "boundary",
        "POST /approvals/a1/withdraw HTTP/1.1\r\nhost: relay\r\ncontent-length: 0\r\n\r\n",
    );
    assert!(withdrawn.starts_with("HTTP/1.1 200 OK"), "{withdrawn}");
    let missing =
        send_kind(&gateway.ready.extension, "boundary", "GET /approvals/absent HTTP/1.1\r\nhost: relay\r\n\r\n");
    assert!(missing.starts_with("HTTP/1.1 404 Not Found"), "{missing}");
    let seen = seen.lock().unwrap();
    assert!(seen.iter().any(|r| r["op"] == "withdraw_approval" && r["id"] == "a1"), "{seen:?}");
}

#[test]
fn an_unreachable_queue_still_answers_the_boundary_kind_with_an_error() {
    let dir = scratch("boundary-missing");
    let missing = dir.join("absent.sock");
    let gateway =
        start_with("boundary-down", "http://127.0.0.1:9/v1", &["allowed"], Some("host-key"), Some((&missing, "t")));
    let events = send_kind(&gateway.ready.extension, "boundary", "GET /events?after=0 HTTP/1.1\r\nhost: relay\r\n\r\n");
    assert!(events.starts_with("HTTP/1.1 200 OK"), "{events}");
    assert!(events.contains("[]"), "{events}");
    let polled =
        send_kind(&gateway.ready.extension, "boundary", "GET /approvals/a1?wait=0 HTTP/1.1\r\nhost: relay\r\n\r\n");
    assert!(polled.starts_with("HTTP/1.1 503"), "{polled}");
    assert!(polled.contains("unreachable"), "{polled}");
}

/// A fake daemon: one approval connection, one line in, one JSON reply out.
fn queue(name: &str, reply: Value, delay: Duration, assert: impl Fn(&Value) + Send + 'static) -> PathBuf {
    let dir = scratch(name);
    let socket = dir.join("deckd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert(&request);
        std::thread::sleep(delay);
        let reply = serde_json::to_string(&serde_json::json!({ "ok": reply })).unwrap();
        stream.write_all(reply.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();
    });
    socket
}

/// A fake daemon that answers every control connection and records it, so a
/// test can see a report that the gateway sends after the response.
fn recorder(name: &str) -> (PathBuf, Arc<Mutex<Vec<Value>>>) {
    let dir = scratch(name);
    let socket = dir.join("deckd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let mut line = String::new();
            if BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let Ok(request) = serde_json::from_str::<Value>(&line) else { continue };
            recorded.lock().unwrap().push(request);
            let _ = stream.write_all(b"{\"ok\":null}\n");
            let _ = stream.flush();
        }
    });
    (socket, seen)
}

fn eventually(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < deadline, "condition was not observed before timeout");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn chat(model: &str) -> String {
    let body = format!(r#"{{"model":"{model}","messages":[]}}"#);
    format!(
        "POST /v1/chat/completions HTTP/1.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
}

#[test]
fn a_pending_model_asks_deckd_and_reports_the_approval_id() {
    let body_len = chat("other").len() - chat("other").find("\r\n\r\n").unwrap() - 4;
    let socket = queue(
        "queue-pending",
        serde_json::json!({ "id": "a1f3", "state": "pending" }),
        Duration::ZERO,
        move |request| {
            assert_eq!(request["op"], "approval");
            assert_eq!(request["task"], "task-1");
            assert_eq!(request["gateway"], "model");
            assert_eq!(request["operation"], "chat_completions");
            assert_eq!(request["destination"], "other");
            assert_eq!(request["credential"], "DECK_GATEWAY_TEST_KEY");
            assert_eq!(request["details"]["method"], "POST");
            assert_eq!(request["details"]["path"], "/v1/chat/completions");
            assert_eq!(request["details"]["size"], body_len as u64);
        },
    );
    let gateway =
        start_with("pending", "http://127.0.0.1:9/v1", &["allowed"], Some("host-key"), Some((&socket, "task-1")));
    let response = send(&gateway.ready.model, &chat("other"));
    assert!(response.starts_with("HTTP/1.1 403 Forbidden"), "{response}");
    assert!(response.contains("x-deck-approval: a1f3"), "{response}");
    assert!(response.contains(r#""approval_id":"a1f3""#), "{response}");
    assert!(response.contains(r#""retryable":true"#), "{response}");
}

#[test]
fn an_allowed_approval_lets_the_same_request_reach_the_provider() {
    let socket = queue(
        "queue-allowed",
        serde_json::json!({ "id": "a1", "state": { "allowed": "once" } }),
        Duration::ZERO,
        |_| {},
    );
    let (port, provider) = provider(|request| {
        assert_eq!(request.target, "/v1/chat/completions");
        assert_eq!(request.body, br#"{"model":"other","messages":[]}"#);
    });
    let gateway = start_with(
        "allowed-approval",
        &format!("http://127.0.0.1:{port}/v1"),
        &["allowed"],
        Some("host-key"),
        Some((&socket, "task-1")),
    );
    let response = send(&gateway.ready.model, &chat("other"));
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    provider.join().unwrap();
}

#[test]
fn a_denied_approval_is_not_reported_as_retryable() {
    let socket = queue("queue-denied", serde_json::json!({ "id": "a2", "state": "denied" }), Duration::ZERO, |_| {});
    let gateway =
        start_with("denied-approval", "http://127.0.0.1:9/v1", &["allowed"], Some("host-key"), Some((&socket, "t")));
    let response = send(&gateway.ready.model, &chat("other"));
    assert!(response.starts_with("HTTP/1.1 403 Forbidden"), "{response}");
    assert!(response.contains(r#""retryable":false"#), "{response}");
    assert!(response.contains(r#""approval_id":"a2""#), "{response}");
}

#[test]
fn the_gateway_waits_for_the_queue_decision() {
    let grants = scratch("hold-grants").join("grants.json");
    let persisted = grants.clone();
    let socket = queue(
        "queue-hold",
        serde_json::json!({ "id": "a3", "state": { "allowed": "task" } }),
        Duration::from_millis(250),
        move |_| {
            std::fs::write(
                &persisted,
                serde_json::json!([{
                    "id": "held", "gateway": "model", "operation": "chat_completions", "credential": "DECK_GATEWAY_TEST_KEY",
                    "destination": "other", "scope": "task", "task": "t"
                }])
                .to_string(),
            )
            .unwrap();
        },
    );
    let (port, provider) = provider(|_| {});
    let gateway = start_launch(
        "hold",
        &format!("http://127.0.0.1:{port}/v1"),
        &["allowed"],
        Some("host-key"),
        Launch { control: Some((&socket, "t")), grants: Some(&grants), ..Launch::default() },
    );
    let started = std::time::Instant::now();
    let response = send(&gateway.ready.model, &chat("other"));
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(started.elapsed() >= Duration::from_millis(250), "the request waited for the queue");
    provider.join().unwrap();
}

#[test]
fn an_unreachable_queue_fails_closed() {
    let dir = scratch("queue-missing");
    let missing = dir.join("absent.sock");
    let gateway =
        start_with("queue-down", "http://127.0.0.1:9/v1", &["allowed"], Some("host-key"), Some((&missing, "t")));
    let response = send(&gateway.ready.model, &chat("other"));
    assert!(response.starts_with("HTTP/1.1 403 Forbidden"), "{response}");
    assert!(response.contains("unreachable"), "{response}");
    assert!(!response.contains("approval_id"), "{response}");
}

#[test]
fn a_spent_request_budget_denies_before_the_provider() {
    let (port, provider) = provider(|_| {});
    let gateway = start_full(
        "request-budget",
        &format!("http://127.0.0.1:{port}/v1"),
        &["allowed"],
        Some("host-key"),
        None,
        Limits { requests: Some(1), ..Limits::default() },
        None,
    );
    let first = send(&gateway.ready.model, &chat("allowed"));
    assert!(first.starts_with("HTTP/1.1 200 OK"), "{first}");
    provider.join().unwrap();

    // The provider is gone; a second upstream request would be a 502. Budget
    // admission must refuse it first.
    let second = send(&gateway.ready.model, &chat("allowed"));
    assert!(second.starts_with("HTTP/1.1 429 Too Many Requests"), "{second}");
    assert!(second.contains(r#""code":"deck_budget_exhausted""#), "{second}");
    assert!(second.contains("no provider request was made"), "{second}");
}

#[test]
fn token_usage_from_the_final_sse_chunk_spends_the_budget() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
        "data: {\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4,\"total_tokens\":7}}\n\n",
        "data: [DONE]\n\n",
    );
    let (port, provider) = provider_with("text/event-stream", body, |_| {});
    let gateway = start_full(
        "token-budget",
        &format!("http://127.0.0.1:{port}/v1"),
        &["allowed"],
        Some("host-key"),
        None,
        Limits { tokens: Some(7), ..Limits::default() },
        None,
    );
    let first = send(&gateway.ready.model, &chat("allowed"));
    assert!(first.starts_with("HTTP/1.1 200 OK"), "{first}");
    assert!(first.contains("data: [DONE]"), "the response still streamed: {first}");
    provider.join().unwrap();

    let second = send(&gateway.ready.model, &chat("allowed"));
    assert!(second.starts_with("HTTP/1.1 429 Too Many Requests"), "{second}");
    assert!(second.contains("token budget is spent (7 of 7)"), "{second}");
}

#[test]
fn the_gateway_records_usage_on_the_task() {
    let (socket, seen) = recorder("usage-record");
    let (port, provider) = provider_with("application/json", r#"{"id":"x","usage":{"total_tokens":5}}"#, |_| {});
    let gateway = start_with(
        "usage",
        &format!("http://127.0.0.1:{port}/v1"),
        &["allowed"],
        Some("host-key"),
        Some((&socket, "task-1")),
    );
    let response = send(&gateway.ready.model, &chat("allowed"));
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    provider.join().unwrap();

    eventually(|| seen.lock().unwrap().iter().any(|request| request["op"] == "model_usage"));
    let seen = seen.lock().unwrap();
    let usage = seen.iter().find(|request| request["op"] == "model_usage").unwrap();
    assert_eq!(usage["task"], "task-1");
    assert_eq!(usage["requests"], 1);
    assert_eq!(usage["tokens"], 5);
}

#[test]
fn an_admitted_request_is_recorded_even_when_the_provider_fails() {
    let (socket, seen) = recorder("usage-failed-provider");
    let gateway =
        start_with("usage-failure", "http://127.0.0.1:9/v1", &["allowed"], Some("host-key"), Some((&socket, "task-1")));
    let response = send(&gateway.ready.model, &chat("allowed"));
    assert!(response.starts_with("HTTP/1.1 502 Bad Gateway"), "{response}");

    eventually(|| seen.lock().unwrap().iter().any(|request| request["op"] == "model_usage"));
    let seen = seen.lock().unwrap();
    let usage = seen.iter().find(|request| request["op"] == "model_usage").unwrap();
    assert_eq!((usage["requests"].as_u64(), usage["tokens"].as_u64()), (Some(1), Some(0)));
}

#[test]
fn a_live_budget_file_applies_to_the_next_request() {
    let dir = scratch("live-budget");
    let budget = dir.join("budget.json");
    std::fs::write(&budget, r#"{"requests":2,"tokens":null}"#).unwrap();
    let (port, provider) = provider(|_| {});
    let gateway = start_full(
        "budget-file",
        &format!("http://127.0.0.1:{port}/v1"),
        &["allowed"],
        Some("host-key"),
        None,
        Limits::default(),
        Some(&budget),
    );
    let first = send(&gateway.ready.model, &chat("allowed"));
    assert!(first.starts_with("HTTP/1.1 200 OK"), "{first}");
    provider.join().unwrap();

    std::fs::write(&budget, r#"{"requests":1,"tokens":null}"#).unwrap();
    let second = send(&gateway.ready.model, &chat("allowed"));
    assert!(second.starts_with("HTTP/1.1 429 Too Many Requests"), "{second}");
    assert!(second.contains("request budget is spent (1 of 1)"), "{second}");
}

// ---- The web and package gateway (ADR 0004 §3) ----

/// A TCP listener that counts accepted connections and echoes every byte. It
/// sends the `hello` line as soon as a connection arrives, which proves the
/// gateway shared an established tunnel: a peer that is still the gateway's
/// own request reader never answers.
fn probe(name: &str, hello: &'static str) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
    let _ = name;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = hits.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::thread::spawn(move || {
                let _ = stream.write_all(hello.as_bytes());
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    (port, hits)
}

/// Append one `/etc/hosts`-shaped entry. A case that needs a *public*
/// destination pins it to a public address the gateway resolves from the file;
/// no test contacts a real public host.
fn pin(hosts: &Path, address: &str, name: &str) {
    let mut text = std::fs::read_to_string(hosts).unwrap_or_default();
    text.push_str(&format!("{address} {name}\n"));
    std::fs::write(hosts, text).unwrap();
}

fn hosts_file(name: &str) -> PathBuf {
    let dir = scratch(name);
    let hosts = dir.join("hosts");
    std::fs::write(&hosts, "").unwrap();
    hosts
}

/// A gateway with a web socket and whatever web policy the case needs.
fn with_web(
    name: &str,
    grants: &[&Path],
    profiles: &[&str],
    hosts: Option<&Path>,
    control: Option<(&Path, &str)>,
) -> Gateway {
    let dir = scratch(name);
    let mut command = Command::new(env!("CARGO_BIN_EXE_deck-gateway"));
    command
        .arg("--model-socket")
        .arg(dir.join("model.sock"))
        .arg("--extension-socket")
        .arg(dir.join("extension.sock"))
        .arg("--web-socket")
        .arg(dir.join("web.sock"))
        .args(["--project", "/fixture"])
        .stderr(Stdio::null())
        .arg("--upstream")
        .arg("http://127.0.0.1:9/v1")
        .arg("--model")
        .arg("allowed")
        .arg("--secret")
        .arg("DECK_GATEWAY_TEST_KEY")
        .arg("--exit-on-stdin-eof")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    for grant in grants {
        command.arg("--grants").arg(grant);
    }
    for profile in profiles {
        command.arg("--web-profile").arg(profile);
    }
    if let Some(hosts) = hosts {
        command.arg("--web-hosts").arg(hosts);
    }
    command.arg("--web-test-connect").arg("127.0.0.1");
    let deckd = Deckd::start(&dir, &[("DECK_GATEWAY_TEST_KEY", "host-key")], control.map(|(socket, _)| socket));
    deckd.args(&mut command, control.map_or("t", |(_, task)| task));
    command.env("DECK_GATEWAY_TEST_KEY", DECOY);
    let mut child = command.spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let ready: Ready = serde_json::from_str(line.trim()).unwrap_or_else(|error| panic!("{error}: {line:?}"));
    assert_eq!(ready.web.as_deref(), Some(dir.join("web.sock").as_path()), "the readiness line names the web socket");
    // The gateway runs with `--exit-on-stdin-eof`, so the handle is leaked on
    // purpose: dropping it here would stop the process mid-test.
    std::mem::forget(stdin);
    Gateway { child, ready, deckd }
}

/// A tunnel to a name the host file pins, with a grant file for the port.
fn grant_file(name: &str, destination: &str, operation: &str) -> PathBuf {
    let dir = scratch(name);
    let path = dir.join("grants.json");
    std::fs::write(
        &path,
        serde_json::json!([{
            "id": "grant-1", "task": "t", "gateway": "web", "operation": operation, "destination": destination, "scope": "task"
        }])
        .to_string(),
    )
    .unwrap();
    path
}

#[test]
fn connect_to_loopback_or_a_private_name_is_refused_without_asking() {
    let (port, hits) = probe("web-refused", "");
    let hosts = hosts_file("web-refused-hosts");
    pin(&hosts, "127.0.0.1", "pinned.example");
    pin(&hosts, "10.0.0.1", "private.example");
    let gateway = with_web("web-refused-gw", &[], &[], Some(&hosts), None);
    let web = gateway.ready.web.clone().unwrap();
    for target in ["127.0.0.1", "10.0.0.1", "169.254.169.254", "[::1]", "pinned.example", "private.example"] {
        let response = send(&web, &format!("CONNECT {target}:{port} HTTP/1.1\r\n\r\n"));
        assert!(response.starts_with("HTTP/1.1 403 Forbidden"), "{target}: {response}");
        assert!(!response.contains("approval_id"), "{target} must not create an item: {response}");
    }
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0, "no refused name is dialled");
}

#[test]
fn a_granted_port_opens_a_tunnel_and_another_port_is_a_different_decision() {
    // The name resolves from the fixture to a public address, so the policy
    // admits it; `--web-test-connect` sends the dial to the probe instead, so
    // the case proves the tunnel path without a route to a real host.
    let (port, hits) = probe("web-granted", "PROBE READY\n");
    let hosts = hosts_file("web-granted-hosts");
    pin(&hosts, "140.82.112.3", "pinned.example");
    let grants = grant_file("web-granted-file", &format!("pinned.example:{port}"), "connect");
    let gateway = with_web("web-granted-gw", &[&grants], &[], Some(&hosts), None);
    let web = gateway.ready.web.clone().unwrap();
    let mut tunnel = UnixStream::connect(&web).unwrap();
    tunnel.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let requested = format!(
        "CONNECT pinned.example:{port} HTTP/1.1\r\nProxy-Authorization: Basic synthetic\r\nX-Guest: proxy-only\r\n\r\n"
    );
    tunnel.write_all(requested.as_bytes()).unwrap();
    // The guest reads what the gateway wrote. The probe greets on accept and
    // echoes, so the tunnel is proven by the probe's own bytes arriving: the
    // gateway never writes them.
    // A UNIX socket has no message boundary, so the status head and the
    // destination's bytes may arrive in one read. The head must come first and
    // the destination's greeting with it; anything after them is the order
    // the gateway wrote, which this case does not constrain.
    const STATUS: &str = "HTTP/1.1 200 Connection Established\r\n\r\n";
    let mut seen = Vec::new();
    let mut buf = [0u8; 64];
    while !seen.windows(STATUS.len()).any(|window| window == STATUS.as_bytes())
        || !seen.windows(12).any(|window| window == b"PROBE READY\n")
    {
        let read = tunnel.read(&mut buf).unwrap();
        assert_ne!(read, 0, "closed after {:?}", String::from_utf8_lossy(&seen));
        seen.extend_from_slice(&buf[..read]);
    }
    assert_eq!(
        seen,
        [STATUS.as_bytes(), b"PROBE READY\n"].concat(),
        "only destination bytes follow the CONNECT response"
    );
    let payload = b"\x16\x03\x01 not http";
    tunnel.write_all(payload).unwrap();
    let mut back = vec![0u8; payload.len()];
    tunnel.read_exact(&mut back).unwrap();
    assert_eq!(&back, payload, "the tunnel is opaque in both directions");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1, "the granted port is dialled once");
    // The grant names one port; another port on the same name has neither the
    // grant nor a profile, and with no queue it is refused rather than held.
    let other = send(&web, &format!("CONNECT pinned.example:{} HTTP/1.1\r\n\r\n", port + 1));
    assert!(other.starts_with("HTTP/1.1 403 Forbidden"), "{other}");
    assert!(other.contains(&format!("pinned.example:{}", port + 1)), "{other}");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1, "an ungranted port is not dialled");
}

/// Read the proxy handshake and the probe's greeting before sending payload.
fn open_web_tunnel(web: &Path, destination: &str) -> UnixStream {
    let mut stream = UnixStream::connect(web).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    stream.write_all(format!("CONNECT {destination} HTTP/1.1\r\n\r\n").as_bytes()).unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"PROBE READY\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    assert!(head.starts_with(b"HTTP/1.1 200 Connection Established\r\n\r\n"));
    stream
}

fn echo_payload(stream: &mut UnixStream) {
    stream.write_all(b"still open").unwrap();
    let mut back = [0; 10];
    stream.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"still open");
}

#[test]
fn revoking_a_grant_closes_its_open_tunnel_and_preserves_other_tunnels() {
    let (port, hits) = probe("web-revoke", "PROBE READY\n");
    let hosts = hosts_file("web-revoke-hosts");
    pin(&hosts, "140.82.112.3", "revoked.example");
    pin(&hosts, "140.82.112.4", "kept.example");
    let destination = format!("revoked.example:{port}");
    let revoked = grant_file("web-revoke-file", &destination, "connect");
    let kept = grant_file("web-kept-file", &format!("kept.example:{port}"), "connect");
    let gateway = with_web("web-revoke-gw", &[&revoked, &kept], &[], Some(&hosts), None);
    let web = gateway.ready.web.as_ref().unwrap();
    let mut tunnel = open_web_tunnel(web, &destination);
    let mut unrelated = open_web_tunnel(web, &format!("kept.example:{port}"));
    echo_payload(&mut tunnel);
    echo_payload(&mut unrelated);

    std::fs::remove_file(revoked).unwrap();
    let mut byte = [0];
    assert_eq!(tunnel.read(&mut byte).unwrap(), 0, "revocation closes an already open tunnel");
    echo_payload(&mut unrelated);
    let refused = send(web, &format!("CONNECT {destination} HTTP/1.1\r\n\r\n"));
    assert!(refused.starts_with("HTTP/1.1 403 Forbidden"), "{refused}");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2, "a revoked destination is not dialled again");
}

#[test]
fn removing_a_project_rule_closes_its_tunnel_even_with_deckd_down() {
    let (port, _) = probe("web-project-revoke", "PROBE READY\n");
    let hosts = hosts_file("web-project-revoke-hosts");
    pin(&hosts, "140.82.112.3", "project.example");
    let destination = format!("project.example:{port}");
    let path = scratch("web-project-revoke-file").join("projects.json");
    let rule = serde_json::json!({
        "id": "project-1", "project": "/fixture", "gateway": "web", "operation": "connect",
        "destination": destination, "scope": "project"
    });
    std::fs::write(&path, serde_json::json!({"version": 1, "rules": [rule]}).to_string()).unwrap();
    let gateway = with_web("web-pr-revoke", &[&path], &[], Some(&hosts), None);
    let web = gateway.ready.web.as_ref().unwrap();
    let mut tunnel = open_web_tunnel(web, &destination);
    gateway.deckd.stop();
    echo_payload(&mut tunnel);
    let next = path.with_extension("tmp");
    std::fs::write(&next, r#"{"version":2,"rules":[]}"#).unwrap();
    std::fs::rename(next, &path).unwrap();
    assert_eq!(tunnel.read(&mut [0]).unwrap(), 0);
    let refused = send(web, &format!("CONNECT {destination} HTTP/1.1\r\n\r\n"));
    assert!(refused.starts_with("HTTP/1.1 403 Forbidden"), "{refused}");
}

#[test]
fn revoking_a_mirror_grant_closes_the_stream_and_its_upstream() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (closed, upstream_closed) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1000000\r\n\r\nfirst").unwrap();
        closed.send(stream.read(&mut [0]).unwrap()).unwrap();
    });
    let hosts = hosts_file("web-mirror-revoke-hosts");
    pin(&hosts, "140.82.112.3", "mirror.example");
    let destination = format!("mirror.example:{port}");
    let grants = grant_file("web-mirror-revoke-file", &destination, "fetch");
    let gateway = with_web("web-mirror-revoke-gw", &[&grants], &[], Some(&hosts), None);
    let mut stream = UnixStream::connect(gateway.ready.web.as_ref().unwrap()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    stream
        .write_all(format!("GET http://{destination}/file HTTP/1.1\r\nhost: {destination}\r\n\r\n").as_bytes())
        .unwrap();
    let mut response = Vec::new();
    while !response.ends_with(b"first") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        response.push(byte[0]);
    }
    std::fs::remove_file(grants).unwrap();
    assert_eq!(stream.read(&mut [0]).unwrap(), 0, "the guest's response stream closes");
    assert_eq!(upstream_closed.recv_timeout(Duration::from_secs(2)).unwrap(), 0, "the upstream closes too");
    server.join().unwrap();
}

#[test]
fn an_allow_once_tunnel_survives_consumption_and_the_next_request_asks_again() {
    let (port, hits) = probe("web-once", "PROBE READY\n");
    let hosts = hosts_file("web-once-hosts");
    pin(&hosts, "140.82.112.3", "once.example");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    let (queue, _) = interactive_queue("web-once-queue", move |_| {
        let state = if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            serde_json::json!({"allowed": "once"})
        } else {
            serde_json::json!("pending")
        };
        serde_json::json!({"id": "once-1", "state": state})
    });
    let gateway = with_web("web-once-gw", &[], &[], Some(&hosts), Some((&queue, "t")));
    let web = gateway.ready.web.as_ref().unwrap();
    let destination = format!("once.example:{port}");
    let mut tunnel = open_web_tunnel(web, &destination);
    let refused = send(web, &format!("CONNECT {destination} HTTP/1.1\r\n\r\n"));
    assert!(refused.starts_with("HTTP/1.1 403 Forbidden"), "{refused}");
    // The refused second request also gives the monitor time to observe an
    // empty grant file. That must not revoke the already admitted operation.
    echo_payload(&mut tunnel);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn an_expiring_task_grant_closes_an_idle_tunnel_without_a_policy_file_change() {
    let (port, hits) = probe("web-expiry", "PROBE READY\n");
    let hosts = hosts_file("web-expiry-hosts");
    pin(&hosts, "140.82.112.3", "expires.example");
    let destination = format!("expires.example:{port}");
    let grants = grant_file("web-expiry-file", &destination, "connect");
    let gateway = with_web("web-expiry-gw", &[&grants], &[], Some(&hosts), None);
    let mut policy: Value = serde_json::from_slice(&std::fs::read(&grants).unwrap()).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    policy[0]["expires_ms"] = serde_json::json!(now + 3000);
    std::fs::write(&grants, policy.to_string()).unwrap();
    let web = gateway.ready.web.as_ref().unwrap();
    let mut tunnel = open_web_tunnel(web, &destination);
    tunnel.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    echo_payload(&mut tunnel);
    assert_eq!(tunnel.read(&mut [0]).unwrap(), 0, "an idle connection ends when its grant expires");
    let refused = send(web, &format!("CONNECT {destination} HTTP/1.1\r\n\r\n"));
    assert!(refused.starts_with("HTTP/1.1 403 Forbidden"), "{refused}");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn a_plain_http_mirror_read_is_forwarded_without_the_guest_proxy_credential() {
    let (port, hits) = probe("web-mirror", "");
    let hosts = hosts_file("web-mirror-hosts");
    pin(&hosts, "140.82.112.3", "mirror.example");
    let grants = grant_file("web-mirror-file", &format!("mirror.example:{port}"), "fetch");
    let gateway = with_web("web-mirror-gw", &[&grants], &[], Some(&hosts), None);
    let web = gateway.ready.web.clone().unwrap();
    // The probe echoes, so the request the mirror actually saw comes back
    // through the gateway as the "response". The probe keeps its connection
    // open until the guest closes, so the case reads the first line only.
    let mut stream = UnixStream::connect(&web).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream
        .write_all(
            format!(
                "GET http://mirror.example:{port}/pool/a.deb HTTP/1.1\r\nhost: mirror.example:{port}\r\n\
                 proxy-authorization: Basic Z3Vlc3Q=\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    while !response.ends_with(b"\r\n") {
        stream.read_exact(&mut byte).unwrap();
        response.push(byte[0]);
    }
    let response = String::from_utf8(response).unwrap();
    assert_eq!(response, "GET /pool/a.deb HTTP/1.1\r\n", "an origin-form request reaches the mirror");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    // A mirror the task holds no grant for is refused, and not dialled.
    let refused = send(&web, "GET http://mirror.example:1/ HTTP/1.1\r\nhost: mirror.example:1\r\n\r\n");
    assert!(refused.starts_with("HTTP/1.1 403 Forbidden"), "{refused}");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn a_redirect_from_a_mirror_to_another_name_is_a_destination_of_its_own() {
    // A mirror that redirects once, then answers with the file.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap_or(0) == 0 {
                    return;
                }
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
            log.lock().unwrap().push(head.clone());
            if head.starts_with("get /moved") {
                let body = "moved";
                let _ = write!(
                    stream,
                    "HTTP/1.1 301 Moved Permanently\r\nlocation: http://elsewhere.example:{port}/final\r\n\
                     content-length: {}\r\n\r\n{body}",
                    body.len()
                );
            } else {
                let body = "payload";
                let _ = write!(stream, "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}", body.len());
            }
            let _ = stream.flush();
        }
    });
    let hosts = hosts_file("web-redirect-hosts");
    pin(&hosts, "140.82.112.3", "mirror.example");
    pin(&hosts, "140.82.112.4", "elsewhere.example");
    let grants = grant_file("web-redirect-file", &format!("mirror.example:{port}"), "fetch");
    let gateway = with_web("web-redirect-gw", &[&grants], &[], Some(&hosts), None);
    let web = gateway.ready.web.clone().unwrap();
    let response =
        send(&web, &format!("GET http://mirror.example:{port}/moved HTTP/1.1\r\nhost: mirror.example:{port}\r\n\r\n"));
    // The redirect's destination has no grant, so it is refused: the answer is
    // the gateway's denial, never the redirect's own body.
    assert!(response.starts_with("HTTP/1.1 403 Forbidden"), "{response}");
    assert!(!response.contains("moved"), "the client never sees the redirect: {response}");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "the second name is never contacted: {seen:?}");
    assert!(seen[0].contains(&format!("host: mirror.example:{port}")), "{seen:?}");
}

#[test]
fn a_web_denial_reaches_the_agent_through_the_boundary_status_events() {
    let (opened, queue_seen) = interactive_queue("web-events", |request| match request["op"].as_str() {
        Some("approval") => serde_json::json!({ "id": "w1", "destination": "mirror.example:1", "state": "pending" }),
        _ => serde_json::Value::Null,
    });
    let hosts = hosts_file("web-events-hosts");
    pin(&hosts, "140.82.112.3", "mirror.example");
    let gateway = with_web("web-events-gw", &[], &[], Some(&hosts), Some((&opened, "t")));
    let web = gateway.ready.web.clone().unwrap();
    let response = send(&web, "GET http://mirror.example:1/ HTTP/1.1\r\nhost: mirror.example:1\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 403 Forbidden"), "{response}");
    assert!(response.contains(r#""approval_id":"w1""#), "{response}");
    assert!(
        queue_seen.lock().unwrap().iter().any(|ask| ask["gateway"] == "web" && ask["operation"] == "fetch"),
        "{:?}",
        queue_seen.lock().unwrap()
    );
    let events = send_kind(&gateway.ready.extension, "boundary", "GET /events?after=0 HTTP/1.1\r\nhost: relay\r\n\r\n");
    assert!(events.starts_with("HTTP/1.1 200 OK"), "{events}");
    assert!(events.contains(r#""approval_id":"w1""#), "the agent learns why it was blocked: {events}");
    assert!(events.contains(r#""gateway":"web""#), "{events}");
    assert!(events.contains(r#""state":"pending""#), "{events}");
}

// ---- The GitHub operation gateway (ADR 0007) --------------------------------

/// A fake `api.github.com`: the connection carries one request, which it
/// records and answers with `reply`. Every known API call is counted.
fn api(
    assert: impl Fn(&Request) -> String + Send + Sync + 'static,
) -> (String, Arc<Mutex<Vec<Request>>>, Arc<std::sync::atomic::AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    let assert = Arc::new(assert);
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let log = log.clone();
            let count = count.clone();
            let assert = assert.clone();
            std::thread::spawn(move || {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let Ok(Some(request)) = http::read(&mut reader) else { return };
                let body = (assert)(&request);
                log.lock().unwrap().push(request);
                // `!<status> <body>` answers with that status instead of 200.
                let (status, body) = match body.strip_prefix('!').and_then(|rest| rest.split_once(' ')) {
                    Some((status, rest)) => (status.to_string(), rest.to_string()),
                    None => ("200".to_string(), body),
                };
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            });
        }
    });
    (base, seen, calls)
}

/// A gateway with a GitHub socket and whatever policy the case needs.
fn with_github(name: &str, api: &str, remote: &str, control: Option<(&Path, &str)>, token: Option<&str>) -> Gateway {
    let dir = scratch(name);
    let mut command = Command::new(env!("CARGO_BIN_EXE_deck-gateway"));
    command
        .arg("--model-socket")
        .arg(dir.join("model.sock"))
        .arg("--extension-socket")
        .arg(dir.join("extension.sock"))
        .arg("--github-socket")
        .arg(dir.join("github.sock"))
        .arg("--github-repository")
        .arg("owner/repo")
        .arg("--github-api")
        .arg(api)
        .arg("--github-remote")
        .arg(remote)
        .arg("--github-scratch")
        .arg(dir.join("scratch"))
        .arg("--github-token")
        .arg("DECK_GATEWAY_TEST_GITHUB")
        .arg("--task")
        .arg("t-1")
        .arg("--upstream")
        .arg("http://127.0.0.1:9/v1")
        .arg("--model")
        .arg("allowed")
        .arg("--secret")
        .arg("DECK_GATEWAY_TEST_KEY")
        .arg("--exit-on-stdin-eof")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut values = vec![("DECK_GATEWAY_TEST_KEY", "host-key")];
    if let Some(token) = token {
        values.push(("DECK_GATEWAY_TEST_GITHUB", token));
    }
    let deckd = Deckd::start(&dir, &values, control.map(|(socket, _)| socket));
    deckd.args(&mut command, control.map_or("t-1", |(_, task)| task));
    command.env("DECK_GATEWAY_TEST_GITHUB", DECOY);
    let mut child = command.spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let ready: Ready = serde_json::from_str(line.trim()).unwrap_or_else(|error| panic!("{error}: {line:?}"));
    assert_eq!(ready.github.as_deref(), Some(dir.join("github.sock").as_path()));
    // The gateway runs with `--exit-on-stdin-eof`, so the handle is leaked on
    // purpose: dropping it here would stop the process mid-test.
    std::mem::forget(stdin);
    Gateway { child, ready, deckd }
}

/// A bare repository on host loopback stands in for GitHub: `git push` and
/// `git ls-remote` work against it with no credential.
fn bare(name: &str) -> (PathBuf, PathBuf) {
    let dir = scratch(name);
    let repo = dir.join("owner").join("repo.git");
    std::fs::create_dir_all(&repo).unwrap();
    let status = Command::new("git").args(["init", "--bare", "--quiet"]).arg(&repo).status().unwrap();
    assert!(status.success());
    (dir, repo)
}

/// The clone URL for a bare repository a case just made.
fn url(repo: &Path) -> String {
    format!("file://{}", repo.display())
}

/// A commit made by "the guest": a normal repository whose one commit is
/// turned into a bundle for `refs/heads/<branch>`.
fn bundle(dir: &Path, branch: &str, name: &str, history: &Path) -> Vec<u8> {
    bundle_with(dir, branch, name, history, name.as_bytes(), None)
}

/// A bundle whose commit writes `contents`, built from `since` when one is
/// named (`since..branch`, the incremental form with a prerequisite line).
fn bundle_with(dir: &Path, branch: &str, name: &str, history: &Path, contents: &[u8], since: Option<&str>) -> Vec<u8> {
    let work = dir.join(name);
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(["-c", "user.name=deck", "-c", "user.email=d@e"])
            .args(["-c", "commit.gpgsign=false"])
            .arg("-C")
            .arg(&work)
            .args(args)
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    if !work.join(".git").exists() {
        std::fs::create_dir_all(&work).unwrap();
        let status = Command::new("git").arg("init").arg("--quiet").arg(&work).status().unwrap();
        assert!(status.success());
        if history.is_dir() {
            // Start from the published history, so a second publish extends it.
            // A bare repository keeps the branch under its own ref; a normal
            // checkout of it (the second publish's history) keeps it under
            // `refs/remotes/origin/`.
            let bare = Command::new("git")
                .arg("-C")
                .arg(history)
                .args(["rev-parse", "--is-bare-repository"])
                .output()
                .unwrap();
            let source = if String::from_utf8_lossy(&bare.stdout).trim() == "true" {
                format!("refs/heads/{branch}")
            } else {
                format!("refs/remotes/origin/{branch}")
            };
            let status = Command::new("git")
                .arg("-C")
                .arg(&work)
                .arg("fetch")
                .arg("--quiet")
                .arg(history)
                .arg(format!("{source}:refs/remotes/published/{branch}"))
                .status()
                .unwrap();
            assert!(status.success(), "fetch {source} from {}", history.display());
            git(&["reset", "--hard", &format!("refs/remotes/published/{branch}")]);
        }
    }
    std::fs::write(work.join("file.txt"), contents).unwrap();
    git(&["add", "file.txt"]);
    git(&["commit", "--quiet", "-m", name]);
    // The fixture names its branch the way the policy names it; the gateway
    // refuses a bundle whose ref is any other. A case that builds bundles for
    // two branches only asks for the branch's own name here.
    git(&["checkout", "--quiet", "-B", branch]);
    let path = work.join(format!("{name}.bundle"));
    let create = Command::new("git")
        .arg("-C")
        .arg(&work)
        .args(["bundle", "create"])
        .arg(&path)
        .arg(match since {
            Some(since) => format!("{since}..refs/heads/{branch}"),
            None => format!("refs/heads/{branch}"),
        })
        .output()
        .unwrap();
    assert!(create.status.success(), "git bundle create: {}", String::from_utf8_lossy(&create.stderr));
    std::fs::read(&path).unwrap()
}

/// One publish: JSON head, a length-framed bundle, one reply.
fn publish(socket: &Path, branch: &str, bytes: &[u8]) -> String {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    // One body: the JSON operation, then the length-framed bundle. The
    // content-length covers both, the way a client sends a publish.
    let body = serde_json::json!({ "operation": "publish", "branch": branch }).to_string().into_bytes();
    let mut payload = body;
    payload.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_be_bytes());
    payload.extend_from_slice(bytes);
    let head = format!(
        "POST /operations/publish HTTP/1.1\r\nhost: gateway\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        payload.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(&payload).unwrap();
    stream.flush().unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn agreement(name: &str) -> PathBuf {
    let dir = scratch(name);
    let socket = dir.join("deckd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut line = String::new();
            if BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["gateway"], "github", "{request}");
            let reply = serde_json::json!({
                "ok": { "id": "g1", "gateway": "github", "operation": request["operation"],
                        "destination": request["destination"], "state": { "allowed": "project" } }
            });
            let _ = stream.write_all(format!("{reply}\n").as_bytes());
        }
    });
    socket
}

#[test]
fn a_publish_pushes_only_the_task_branch_and_a_secret_is_never_echoed() {
    let (dir, remote) = bare("gh-publish");
    let (api, seen, calls) = api(|_| "{}".into());
    let gateway = with_github(
        "gh-publish-gw",
        &api,
        &url(&remote),
        Some((&agreement("gh-publish-queue"), "t-1")),
        Some("ghp-hunter2-SECRET"),
    );
    let github = gateway.ready.github.clone().unwrap();
    let bytes = bundle(&dir, "deck/t-1", "first", &dir.join("none"));
    let response = publish(&github, "deck/t-1", &bytes);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("\"branch\":\"deck/t-1\""), "{response}");
    assert!(!response.contains("hunter2"), "the token never reaches the guest: {response}");
    // The remote now has the branch, and only it.
    let out = Command::new("git").arg("--git-dir").arg(&remote).arg("branch").arg("--list").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "deck/t-1");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a push uses git; its one API read is the visibility check: {seen:?}"
    );
    assert_eq!(seen.lock().unwrap()[0].target, "/repos/owner/repo");
}

#[test]
fn a_second_publish_extends_the_branch_and_a_rewrite_is_refused() {
    let (dir, remote) = bare("gh-extend");
    let (api, _, _) = api(|_| "{}".into());
    let gateway =
        with_github("gh-extend-gw", &api, &url(&remote), Some((&agreement("gh-extend-queue"), "t-1")), Some("t"));
    let github = gateway.ready.github.clone().unwrap();
    let first = bundle(&dir, "deck/t-1", "one", &dir.join("none"));
    assert!(publish(&github, "deck/t-1", &first).starts_with("HTTP/1.1 200 OK"));
    // A clone of what landed is the published history for the second bundle.
    let cloned = dir.join("cloned");
    assert!(
        Command::new("git").args(["clone", "--quiet"]).arg(url(&remote)).arg(&cloned).status().unwrap().success(),
        "clone the remote"
    );
    let second = bundle(&dir, "deck/t-1", "two", &cloned);
    let response = publish(&github, "deck/t-1", &second);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("\"created\":false"), "{response}");
    // A bundle built on a different root is a rewrite: refused, not forced.
    let unrelated = scratch("gh-extend-unrelated");
    let rewrite = bundle(&unrelated, "deck/t-1", "fork", &unrelated.join("none"));
    let response = publish(&github, "deck/t-1", &rewrite);
    // A rewrite is the policy's no, and the reply names the commit to build on.
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("deck_github_refused") && response.contains("never rewrites it"), "{response}");
    assert!(response.contains(r#""retryable":false"#), "{response}");
    let tip = Command::new("git").arg("--git-dir").arg(&remote).args(["rev-parse", "deck/t-1"]).output().unwrap();
    let tip = String::from_utf8_lossy(&tip.stdout).trim().to_string();
    assert!(response.contains(&format!(r#""current":"{tip}""#)), "{response}");
    let count =
        Command::new("git").arg("--git-dir").arg(&remote).args(["rev-list", "--count", "deck/t-1"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "2", "the rewrite did not land");
}

#[test]
fn a_bundle_for_another_branch_or_two_refs_never_publishes() {
    let (dir, remote) = bare("gh-branch");
    let (api, _, calls) = api(|_| "{}".into());
    let gateway =
        with_github("gh-branch-gw", &api, &url(&remote), Some((&agreement("gh-branch-queue"), "t-1")), Some("t"));
    let github = gateway.ready.github.clone().unwrap();
    let other = bundle(&dir, "main", "main", &dir.join("none"));
    let response = publish(&github, "deck/t-1", &other);
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert!(response.contains("deck_bad_request") && response.contains("not refs/heads/deck/t-1"), "{response}");
    let outs = Command::new("git").arg("--git-dir").arg(&remote).arg("branch").arg("--list").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&outs.stdout).trim(), "", "nothing landed");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    // A tag is not a branch either, and a second ref is refused by the header.
    let tag = bundle(&dir, "deck/t-1", "tagged", &dir.join("none"));
    assert!(publish(&github, "deck/t-1", &tag).starts_with("HTTP/1.1 200 OK"));
    let mut two = b"# v2 git bundle\n".to_vec();
    let sha = "0".repeat(40);
    two.extend_from_slice(format!("{sha} refs/heads/deck/t-1\n{sha} refs/heads/main\n\nPACK").as_bytes());
    let response = publish(&github, "deck/t-1", &two);
    assert!(response.contains("2 refs"), "{response}");
}

#[test]
fn merge_is_refused_by_name_and_a_pull_request_asks_the_queue() {
    let (dir, remote) = bare("gh-merge");
    let (api, seen, _) = api(|request| {
        match request.target.as_str() {
        t if t.ends_with("/pulls") => {
            r#"{"number":7,"title":"t","state":"open","draft":true,"head":{"ref":"deck/t-1","sha":"abc"},"base":{"ref":"main"},"mergeable":null,"html_url":"https://github.com/owner/repo/pull/7","user":{"login":"x"}}"#.into()
        }
        _ => "{}".into(),
    }
    });
    let socket = agreement("gh-merge-queue");
    let gateway = with_github("gh-merge-gw", &api, &url(&remote), Some((&socket, "t-1")), Some("t"));
    let github = gateway.ready.github.clone().unwrap();
    // A merge is a named refusal, answered without asking anyone.
    let merge = send(
        &github,
        &format!(
            "POST /operations/pull_request/update HTTP/1.1\r\ncontent-length: {}\r\n\r\n{}",
            br#"{"operation":"merge","number":7}"#.len(),
            std::str::from_utf8(br#"{"operation":"merge","number":7}"#).unwrap()
        ),
    );
    assert!(merge.starts_with("HTTP/1.1 400"), "{merge}");
    assert!(merge.contains("not a Deck GitHub operation"), "{merge}");
    assert!(merge.contains("may publish"), "{merge}");
    // A pull request asks the queue first, then creates it with the host token.
    let ask = br#"{"operation":"create_pr","head":"deck/t-1","base":"main","title":"Add the thing","body":"It works","draft":true}"#;
    let created = send(
        &github,
        &format!(
            "POST /operations/pull_request HTTP/1.1\r\ncontent-length: {}\r\n\r\n{}",
            ask.len(),
            std::str::from_utf8(ask).unwrap()
        ),
    );
    assert!(created.starts_with("HTTP/1.1 200 OK"), "{created}");
    assert!(created.contains("\"number\":7"), "{created}");
    assert!(!created.contains("\"user\""), "only the review fields return: {created}");
    let seen = seen.lock().unwrap();
    let post = seen.iter().find(|request| request.method == "POST").expect("a POST reached the API");
    assert_eq!(post.target, "/repos/owner/repo/pulls");
    assert_eq!(post.header("authorization"), Some("Bearer t"));
    assert!(std::str::from_utf8(&post.body).unwrap().contains("\"head\":\"deck/t-1\""));
    let _ = dir;
}

#[test]
fn a_denied_publish_reports_the_approval_and_never_reaches_git() {
    let (dir, remote) = bare("gh-denied");
    let (api, _, calls) = api(|_| "{}".into());
    let queue = scratch("gh-denied-queue");
    let socket = queue.join("deckd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
        let reply = serde_json::json!({"ok": {"id": "g9", "state": "denied"}});
        let _ = stream.write_all(format!("{reply}\n").as_bytes());
    });
    let gateway = with_github("gh-denied-gw", &api, &url(&remote), Some((&socket, "t-1")), Some("t"));
    let github = gateway.ready.github.clone().unwrap();
    let bytes = bundle(&dir, "deck/t-1", "denied", &dir.join("none"));
    let response = publish(&github, "deck/t-1", &bytes);
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("\"approval_id\":\"g9\""), "{response}");
    assert!(response.contains("x-deck-approval: g9"), "{response}");
    assert!(response.contains("\"retryable\":false"), "{response}");
    let branches = Command::new("git").arg("--git-dir").arg(&remote).arg("branch").arg("--list").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&branches.stdout).trim(), "", "nothing was pushed");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1, "only the visibility read reached GitHub");
}

/// A fake daemon that answers like the approval queue's own-branch admission
/// and records every ask, so a gateway case can check both what the gateway
/// sent and whether it proceeded.
fn admission_queue(name: &str, mode: &'static str) -> (PathBuf, Arc<Mutex<Vec<Value>>>) {
    let dir = scratch(name);
    let socket = dir.join("deckd.sock");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut line = String::new();
            if BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            let coverage = &request["coverage"];
            let own = coverage["own_branch"].as_bool().unwrap_or(false);
            let public = coverage["public"].as_bool();
            let admitted = mode != "ask" && own && (mode == "own-branch-public" || public == Some(false));
            let state = match admitted {
                true => serde_json::json!({ "allowed": "default" }),
                false => serde_json::json!("pending"),
            };
            let reply = serde_json::json!({
                "ok": { "id": "g1", "gateway": "github", "operation": request["operation"],
                        "destination": request["destination"], "state": state }
            });
            let _ = stream.write_all(format!("{reply}\n").as_bytes());
            log.lock().unwrap().push(request);
        }
    });
    (socket, seen)
}

#[test]
fn a_covered_publish_is_admitted_only_for_a_private_repository() {
    for (case, repository, admitted, public) in [
        ("private", r#"{"private":true}"#, true, Some(false)),
        ("public", r#"{"private":false}"#, false, Some(true)),
        ("unreadable", r#"!404 {"message":"Not Found"}"#, false, None),
    ] {
        let (dir, remote) = bare(&format!("gh-visibility-{case}"));
        let (api, _, _) = api(move |request| match request.target.as_str() {
            "/repos/owner/repo" => repository.into(),
            _ => "{}".into(),
        });
        let (queue, asked) = admission_queue(&format!("gh-visibility-{case}-queue"), "own-branch");
        let gateway =
            with_github(&format!("gh-visibility-{case}-gw"), &api, &url(&remote), Some((&queue, "t-1")), Some("t"));
        let github = gateway.ready.github.clone().unwrap();
        let bytes = bundle(&dir, "deck/t-1", case, &dir.join("none"));
        let response = publish(&github, "deck/t-1", &bytes);
        if admitted {
            assert!(response.starts_with("HTTP/1.1 200 OK"), "{case}: {response}");
            let events =
                send_kind(&gateway.ready.extension, "boundary", "GET /events?after=0 HTTP/1.1\r\nhost: relay\r\n\r\n");
            assert!(events.contains(r#""scope":"default""#), "{case}: {events}");
            assert!(events.contains(r#""gateway":"github""#), "{case}: {events}");
        } else {
            assert!(response.starts_with("HTTP/1.1 403"), "{case}: {response}");
            assert!(response.contains("\"approval_id\":\"g1\""), "{case}: {response}");
        }
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 1, "{case}: one ask: {asked:?}");
        assert_eq!(asked[0]["coverage"]["own_branch"], true, "{case}");
        assert_eq!(asked[0]["coverage"]["public"], serde_json::to_value(public).unwrap(), "{case}");
    }
}

#[test]
fn own_branch_public_admits_a_public_repository_explicitly() {
    let (dir, remote) = bare("gh-visibility-open");
    let (api, _, _) = api(|request| match request.target.as_str() {
        "/repos/owner/repo" => r#"{"private":false}"#.into(),
        _ => "{}".into(),
    });
    let (queue, asked) = admission_queue("gh-visibility-open-queue", "own-branch-public");
    let gateway = with_github("gh-visibility-open-gw", &api, &url(&remote), Some((&queue, "t-1")), Some("t"));
    let github = gateway.ready.github.clone().unwrap();
    let bytes = bundle(&dir, "deck/t-1", "open", &dir.join("none"));
    assert!(publish(&github, "deck/t-1", &bytes).starts_with("HTTP/1.1 200 OK"));
    let asked = asked.lock().unwrap();
    assert_eq!(asked[0]["coverage"], serde_json::json!({ "own_branch": true, "public": true }));
}

#[test]
fn a_body_that_stops_before_its_content_length_is_refused_not_dropped() {
    // A peer that declares more than it sends must get an answer: on Linux a
    // close with unread bytes still queued becomes a RST, and the client sees
    // a connection reset instead of the refusal.
    let (_, remote) = bare("gh-short-body");
    let (api, _, _) = api(|_| "{}".into());
    let gateway = with_github("gh-short-body-gw", &api, &url(&remote), None, Some("t"));
    let github = gateway.ready.github.clone().unwrap();
    let mut stream = UnixStream::connect(&github).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    stream.write_all(b"POST /operations/refs HTTP/1.1\r\ncontent-length: 99\r\n\r\n{\"operation\":\"refs\"}").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert!(response.contains("stopped before its content-length"), "{response}");
}

#[test]
fn reads_need_no_approval_and_a_missing_token_refuses_every_operation() {
    let (_, remote) = bare("gh-reads");
    let (api, seen, _) = api(|request| match () {
        _ if request.target.ends_with("/git/matching-refs/heads/") => {
            r#"[{"ref":"refs/heads/deck/t-1","object":{"sha":"a"}},{"ref":"refs/heads/main","object":{"sha":"b"}}]"#
                .into()
        }
        _ => r#"{"number":3,"state":"open","draft":false}"#.into(),
    });
    let gateway = with_github("gh-reads-gw", &api, &url(&remote), None, Some("t"));
    let github = gateway.ready.github.clone().unwrap();
    // A complete, well-formed request for the wrong path: the body names an
    // operation that is not the one the path serves.
    let mismatched = send(
        &github,
        &format!(
            "POST /operations/pull_request/review HTTP/1.1\r\ncontent-length: {}\r\n\r\n{}",
            r#"{"operation":"refs"}"#.len(),
            r#"{"operation":"refs"}"#
        ),
    );
    assert!(mismatched.starts_with("HTTP/1.1 400"), "the review path serves only review: {mismatched}");
    let refs = send(
        &github,
        &format!(
            "POST /operations/refs HTTP/1.1\r\ncontent-length: {}\r\n\r\n{}",
            r#"{"operation":"refs"}"#.len(),
            r#"{"operation":"refs"}"#
        ),
    );
    assert!(refs.starts_with("HTTP/1.1 200 OK"), "{refs}");
    assert!(refs.contains("\"task_branch\":true"), "{refs}");
    assert!(refs.contains("\"branch\":\"main\""), "{refs}");
    let review = send(
        &github,
        &format!(
            "POST /operations/pull_request/review HTTP/1.1\r\ncontent-length: {}\r\n\r\n{}",
            r#"{"operation":"review","number":3}"#.len(),
            r#"{"operation":"review","number":3}"#
        ),
    );
    // A read reaches the API without a queue connection.
    assert!(review.starts_with("HTTP/1.1 200 OK"), "{review}");
    assert!(review.contains("\"number\":3"), "{review}");
    assert!(seen.lock().unwrap().iter().all(|request| request.header("authorization") == Some("Bearer t")));
    // Without a token nothing runs, and the guest is told why.
    let gateway = with_github("gh-reads-nokey", &api, &url(&remote), None, None);
    let github = gateway.ready.github.clone().unwrap();
    let review = send(
        &github,
        &format!(
            "POST /operations/pull_request/review HTTP/1.1\r\ncontent-length: {}\r\n\r\n{}",
            r#"{"operation":"review","number":3}"#.len(),
            r#"{"operation":"review","number":3}"#
        ),
    );
    assert!(review.starts_with("HTTP/1.1 503"), "{review}");
    assert!(review.contains("deck_credential_missing"), "{review}");
}

/// Bytes that a pack cannot compress, so the bundle's size is the file's.
fn noise(length: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

#[test]
fn a_bundle_larger_than_a_json_request_publishes() {
    let (dir, remote) = bare("gh-large");
    let (api, _, _) = api(|_| "{}".into());
    let gateway =
        with_github("gh-large-gw", &api, &url(&remote), Some((&agreement("gh-large-queue"), "t-1")), Some("t"));
    let github = gateway.ready.github.clone().unwrap();
    let bytes = bundle_with(&dir, "deck/t-1", "large", &dir.join("none"), &noise(400 * 1024), None);
    assert!(bytes.len() > 256 * 1024, "the fixture is past the JSON ceiling: {}", bytes.len());
    let response = publish(&github, "deck/t-1", &bytes);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
}

#[test]
fn an_incremental_bundle_publishes_from_a_fresh_scratch_repository() {
    let (dir, remote) = bare("gh-incremental");
    let (api, _, _) = api(|_| "{}".into());
    let first_gateway =
        with_github("gh-incr-gw1", &api, &url(&remote), Some((&agreement("gh-incr-queue1"), "t-1")), Some("t"));
    let first = bundle(&dir, "deck/t-1", "one", &dir.join("none"));
    assert!(publish(&first_gateway.ready.github.clone().unwrap(), "deck/t-1", &first).starts_with("HTTP/1.1 200 OK"));
    // The way a guest sends a follow-up: only the new commit, with the
    // published tip as its prerequisite. A second gateway has never seen the
    // first commit, so it must read it from the remote to accept this.
    let cloned = dir.join("cloned");
    assert!(Command::new("git").args(["clone", "--quiet"]).arg(url(&remote)).arg(&cloned).status().unwrap().success());
    let second = bundle_with(&dir, "deck/t-1", "two", &cloned, b"two", Some("refs/remotes/published/deck/t-1"));
    assert!(std::str::from_utf8(&second[..60]).unwrap_or_default().contains("\n-"), "a prerequisite line");
    let gateway =
        with_github("gh-incr-gw2", &api, &url(&remote), Some((&agreement("gh-incr-queue2"), "t-1")), Some("t"));
    let response = publish(&gateway.ready.github.clone().unwrap(), "deck/t-1", &second);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("\"created\":false"), "{response}");
    let count =
        Command::new("git").arg("--git-dir").arg(&remote).args(["rev-list", "--count", "deck/t-1"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "2");
}

#[test]
fn an_update_touches_only_the_tasks_own_pull_request_and_review_reports_checks() {
    let (_, remote) = bare("gh-update");
    let sha = "c".repeat(40);
    let pull = move |number: u64, head: &str, repository: &str| {
        format!(
            r#"{{"number":{number},"title":"t","state":"open","draft":true,"head":{{"ref":"{head}","sha":"{sha}","repo":{{"full_name":"{repository}"}}}},"base":{{"ref":"main"}}}}"#
        )
    };
    let (api, seen, _) = api(move |request| {
        match (request.method.as_str(), request.target.as_str()) {
        ("GET", t) if t.ends_with("/pulls/7") => pull(7, "deck/t-1", "owner/repo"),
        ("GET", t) if t.ends_with("/pulls/8") => pull(8, "someone-else", "owner/repo"),
        ("GET", t) if t.ends_with("/pulls/9") => pull(9, "deck/t-1", "fork/repo"),
        ("GET", t) if t.contains("/check-runs") => {
            r#"{"total_count":1,"check_runs":[{"name":"ci","status":"completed","conclusion":"success","output":{"text":"long"}}]}"#.into()
        }
        ("PATCH", _) => pull(7, "deck/t-1", "owner/repo"),
        _ => "{}".into(),
    }
    });
    let gateway =
        with_github("gh-update-gw", &api, &url(&remote), Some((&agreement("gh-update-queue"), "t-1")), Some("t"));
    let github = gateway.ready.github.clone().unwrap();
    let update = |number: u64| {
        let ask = format!(r#"{{"operation":"update_pr","number":{number},"body":"new"}}"#);
        send(
            &github,
            &format!("POST /operations/pull_request/update HTTP/1.1\r\ncontent-length: {}\r\n\r\n{ask}", ask.len()),
        )
    };
    let own = update(7);
    assert!(own.starts_with("HTTP/1.1 200 OK"), "{own}");
    for (number, why) in [(8, "not a branch this task may write"), (9, "not from a branch of owner/repo")] {
        let refused = update(number);
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
        assert!(refused.contains(why), "{refused}");
    }
    let patches: Vec<String> =
        seen.lock().unwrap().iter().filter(|request| request.method == "PATCH").map(|r| r.target.clone()).collect();
    assert_eq!(patches.len(), 1, "only the task's own pull request was changed: {patches:?}");
    assert!(patches[0].ends_with("/pulls/7"));
    let ask = r#"{"operation":"review","number":7}"#;
    let review = send(
        &github,
        &format!("POST /operations/pull_request/review HTTP/1.1\r\ncontent-length: {}\r\n\r\n{ask}", ask.len()),
    );
    assert!(review.starts_with("HTTP/1.1 200 OK"), "{review}");
    assert!(review.contains(r#""conclusion":"success""#), "{review}");
    assert!(!review.contains("long"), "only the run's verdict returns: {review}");
}

#[test]
fn githubs_own_refusals_reach_the_agent_by_kind() {
    let (_, remote) = bare("gh-kinds");
    let (api, _, _) = api(|request| {
        match (request.method.as_str(), request.target.as_str()) {
        ("POST", t) if t.ends_with("/pulls") => {
            r#"!422 {"message":"Validation Failed","errors":[{"message":"A pull request already exists for owner:deck/t-1."}]}"#.into()
        }
        ("GET", t) if t.ends_with("/pulls/404") => r#"!404 {"message":"Not Found"}"#.into(),
        ("GET", t) if t.ends_with("/pulls/401") => r#"!401 {"message":"Bad credentials"}"#.into(),
        ("GET", t) if t.ends_with("/pulls/503") => "!503 <html>unavailable</html>".into(),
        _ => "{}".into(),
    }
    });
    let gateway =
        with_github("gh-kinds-gw", &api, &url(&remote), Some((&agreement("gh-kinds-queue"), "t-1")), Some("t"));
    let github = gateway.ready.github.clone().unwrap();
    let post = |path: &str, ask: &str| {
        send(&github, &format!("POST {path} HTTP/1.1\r\ncontent-length: {}\r\n\r\n{ask}", ask.len()))
    };
    let exists =
        post("/operations/pull_request", r#"{"operation":"create_pr","head":"deck/t-1","base":"main","title":"t"}"#);
    assert!(exists.starts_with("HTTP/1.1 422"), "{exists}");
    assert!(exists.contains("deck_github_rejected") && exists.contains("already exists"), "{exists}");
    let missing = post("/operations/pull_request/update", r#"{"operation":"update_pr","number":404,"title":"x"}"#);
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
    assert!(missing.contains("deck_github_not_found"), "{missing}");
    let review = |number: u16| {
        post("/operations/pull_request/review", &format!(r#"{{"operation":"review","number":{number}}}"#))
    };
    let credential = review(401);
    assert!(credential.starts_with("HTTP/1.1 502"), "{credential}");
    assert!(credential.contains("deck_github_credential") && credential.contains(r#""retryable":false"#));
    let outage = review(503);
    assert!(outage.starts_with("HTTP/1.1 502"), "{outage}");
    assert!(outage.contains("deck_github_failed") && outage.contains(r#""retryable":true"#), "{outage}");
}

/// A provider that answers `count` connections in turn and records the
/// `authorization` each one carried.
fn provider_seq(count: usize) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    std::thread::spawn(move || {
        for _ in 0..count {
            let Ok((mut stream, _)) = listener.accept() else { return };
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let request = http::read(&mut BufReader::new(&stream)).unwrap().unwrap();
            recorded.lock().unwrap().push(request.header("authorization").unwrap_or_default().to_string());
            let body = r#"{"id":"chatcmpl-1","choices":[]}"#;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (port, seen)
}

#[test]
fn every_request_fetches_the_key_so_a_rotation_serves_the_next_one() {
    let (port, seen) = provider_seq(2);
    let gateway = start("rotate", &format!("http://127.0.0.1:{port}/v1"), &["allowed"], Some("key-one"));
    assert!(send(&gateway.ready.model, &chat("allowed")).starts_with("HTTP/1.1 200 OK"));
    gateway.deckd.set("DECK_GATEWAY_TEST_KEY", "key-two");
    assert!(send(&gateway.ready.model, &chat("allowed")).starts_with("HTTP/1.1 200 OK"));
    eventually(|| seen.lock().unwrap().len() == 2);
    assert_eq!(*seen.lock().unwrap(), ["Bearer key-one", "Bearer key-two"], "never the environment's decoy");
    assert_eq!(gateway.deckd.fetched(), ["DECK_GATEWAY_TEST_KEY", "DECK_GATEWAY_TEST_KEY"]);
}

#[test]
fn a_denied_or_budget_refused_request_fetches_nothing() {
    let limits = Limits { requests: Some(1), used_requests: 1, ..Limits::default() };
    let gateway = start_full("no-fetch", "http://127.0.0.1:9/v1", &["allowed"], Some("key"), None, limits, None);
    assert!(send(&gateway.ready.model, "POST /v1/embeddings HTTP/1.1\r\ncontent-length: 0\r\n\r\n").contains("403"));
    assert!(send(&gateway.ready.model, &chat("other")).contains("deck_boundary_denied"));
    assert!(send(&gateway.ready.model, &chat("allowed")).contains("deck_budget_exhausted"));
    assert!(gateway.deckd.fetched().is_empty(), "{:?}", gateway.deckd.fetched());
}

#[test]
fn with_deckd_down_a_request_is_unavailable_and_reaches_no_provider_until_it_returns() {
    let (port, seen) = provider_seq(1);
    let gateway = start("deckd-down", &format!("http://127.0.0.1:{port}/v1"), &["allowed"], Some("host-key"));
    gateway.deckd.stop();
    let refused = send(&gateway.ready.model, &chat("allowed"));
    assert!(refused.starts_with("HTTP/1.1 503"), "{refused}");
    assert!(refused.contains("deck_credential_unavailable") && refused.contains(r#""retryable":true"#), "{refused}");
    assert!(seen.lock().unwrap().is_empty(), "nothing reached the provider");
    gateway.deckd.resume();
    let served = send(&gateway.ready.model, &chat("allowed"));
    assert!(served.starts_with("HTTP/1.1 200 OK"), "no restart is needed: {served}");
    eventually(|| seen.lock().unwrap().len() == 1);
}

#[test]
fn revoking_mid_stream_shuts_the_response_and_refuses_the_next_request() {
    // A provider that sends its head and one chunk, then stalls until the
    // test is over, as a long streamed turn does.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (release, released) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = http::read(&mut BufReader::new(&stream)).unwrap();
        let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n";
        write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n")
            .unwrap();
        write!(stream, "{:x}\r\n{chunk}\r\n", chunk.len()).unwrap();
        stream.flush().unwrap();
        let _ = released.recv_timeout(Duration::from_secs(30));
    });
    let gateway = start("revoke-stream", &format!("http://127.0.0.1:{port}/v1"), &["allowed"], Some("leaked"));
    // The watch is opened at start; give it a moment to register.
    eventually(|| !gateway.deckd.keys.lock().unwrap().watchers.is_empty());

    let mut stream = UnixStream::connect(&gateway.ready.model).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.write_all(chat("allowed").as_bytes()).unwrap();
    let mut seen = Vec::new();
    let mut buffer = [0u8; 4096];
    while !String::from_utf8_lossy(&seen).contains("first") {
        let read = stream.read(&mut buffer).unwrap();
        assert!(read > 0, "the stream ended before its first chunk");
        seen.extend_from_slice(&buffer[..read]);
    }
    let started = Instant::now();
    gateway.deckd.revoke("DECK_GATEWAY_TEST_KEY");
    let mut rest = Vec::new();
    let _ = stream.read_to_end(&mut rest);
    assert!(started.elapsed() < Duration::from_secs(5), "the revocation shut the stream");

    let refused = send(&gateway.ready.model, &chat("allowed"));
    assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
    assert!(refused.contains("deck_credential_revoked"), "{refused}");
    let _ = release.send(());
}

#[test]
fn a_revoked_github_token_refuses_the_next_operation_before_github() {
    let (_, remote) = bare("gh-revoke-remote");
    let (api, seen, _) = api(|_| r#"[]"#.into());
    let gateway = with_github("gh-revoke-gw", &api, &url(&remote), None, Some("ghp-token"));
    eventually(|| !gateway.deckd.keys.lock().unwrap().watchers.is_empty());
    let github = gateway.ready.github.clone().unwrap();
    let refs = || {
        send(
            &github,
            &format!(
                "POST /operations/refs HTTP/1.1\r\ncontent-length: {}\r\n\r\n{}",
                r#"{"operation":"refs"}"#.len(),
                r#"{"operation":"refs"}"#
            ),
        )
    };
    assert!(refs().starts_with("HTTP/1.1 200 OK"));
    let before = seen.lock().unwrap().len();
    gateway.deckd.revoke("DECK_GATEWAY_TEST_GITHUB");
    let refused = refs();
    assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
    assert!(refused.contains("deck_credential_revoked") && refused.contains(r#""retryable":false"#), "{refused}");
    assert_eq!(seen.lock().unwrap().len(), before, "nothing reached GitHub");
    gateway.deckd.set("DECK_GATEWAY_TEST_GITHUB", "ghp-new");
    assert!(refs().starts_with("HTTP/1.1 200 OK"), "a new value restores service");
}

#[test]
fn incomplete_connect_headers_have_a_total_deadline_even_when_bytes_keep_arriving() {
    let gateway = with_web("web-header-deadline", &[], &[], None, None);
    let web = gateway.ready.web.as_ref().unwrap();
    let start = Instant::now();
    let mut silent = UnixStream::connect(web).unwrap();
    let mut stalled = UnixStream::connect(web).unwrap();
    let mut trickled = UnixStream::connect(web).unwrap();
    for stream in [&silent, &stalled, &trickled] {
        stream.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
    }
    stalled.write_all(b"CONNECT example.com:443 HTTP/1.1\r\nX: ").unwrap();
    trickled.write_all(b"CONNECT example.com:443 HTTP/1.1\r\nX: ").unwrap();
    let mut writer = trickled.try_clone().unwrap();
    let sender = std::thread::spawn(move || {
        for _ in 0..80 {
            if writer.write_all(b"x").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let mut buffer = [0u8; 1024];
    assert_eq!(silent.read(&mut buffer).unwrap(), 0, "no initial bytes must also time out");
    for stream in [&mut stalled, &mut trickled] {
        let mut status = [0; b"HTTP/1.1 403 Forbidden\r\n".len()];
        stream.read_exact(&mut status).unwrap();
        assert_eq!(&status, b"HTTP/1.1 403 Forbidden\r\n");
    }
    assert!(start.elapsed() < Duration::from_secs(8), "trickling must not renew the header budget");
    drop(trickled);
    sender.join().unwrap();
    // A timed-out header must not stop the listener from serving other clients.
    let response = send(web, "CONNECT 127.0.0.1:443 HTTP/1.1\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 403 Forbidden"), "{response}");
}

#[test]
fn incomplete_github_headers_have_a_total_deadline_even_when_bytes_keep_arriving() {
    let gateway = with_github("gh-head", "http://127.0.0.1:9", "https://github.com/owner/repo", None, None);
    let github = gateway.ready.github.as_ref().unwrap();
    let start = Instant::now();
    let mut stream = UnixStream::connect(github).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
    stream.write_all(b"POST /operations/publish HTTP/1.1\r\nX: ").unwrap();
    let mut writer = stream.try_clone().unwrap();
    let sender = std::thread::spawn(move || {
        for _ in 0..80 {
            if writer.write_all(b"x").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 408 Request Timeout"), "{response}");
    assert!(start.elapsed() < Duration::from_secs(8), "trickling must not renew the header budget");
    drop(stream);
    sender.join().unwrap();
    // A timed-out request releases its worker and leaves the listener usable.
    let response = send(github, "BROKEN\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 400 Bad Request"), "{response}");
}
