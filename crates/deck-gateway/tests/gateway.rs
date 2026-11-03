//! The real gateway binary against a fake provider on host loopback. These
//! exercise the boundary the guest sees: a Unix socket, a guest request with
//! its own credential headers, and a host-side key added only after they were
//! discarded.

#![expect(clippy::unwrap_used, reason = "test fixture setup must succeed")]

use deck_gateway::http::{self, Request};
use deck_gateway::server::Ready;
use serde_json::Value;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Gateway {
    child: Child,
    _stdin: ChildStdin,
    ready: Ready,
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
    let dir = scratch(name);
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
    if let Some((socket, task)) = control {
        command.arg("--control").arg(socket).arg("--task").arg(task);
    }
    if let Some(key) = key {
        command.env("DECK_GATEWAY_TEST_KEY", key);
    } else {
        command.env_remove("DECK_GATEWAY_TEST_KEY");
    }
    let mut child = command.spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let ready: Ready = serde_json::from_str(line.trim()).unwrap_or_else(|error| panic!("{error}: {line:?}"));
    Gateway { child, _stdin: stdin, ready }
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
    let socket = queue(
        "queue-hold",
        serde_json::json!({ "id": "a3", "state": { "allowed": "task" } }),
        Duration::from_millis(250),
        |_| {},
    );
    let (port, provider) = provider(|_| {});
    let gateway = start_with(
        "hold",
        &format!("http://127.0.0.1:{port}/v1"),
        &["allowed"],
        Some("host-key"),
        Some((&socket, "t")),
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
        command.arg("--web-grants").arg(grant);
    }
    for profile in profiles {
        command.arg("--web-profile").arg(profile);
    }
    if let Some(hosts) = hosts {
        command.arg("--web-hosts").arg(hosts);
    }
    command.arg("--web-test-connect").arg("127.0.0.1");
    if let Some((socket, task)) = control {
        command.arg("--control").arg(socket).arg("--task").arg(task);
    }
    command.env("DECK_GATEWAY_TEST_KEY", "host-key");
    let mut child = command.spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let ready: Ready = serde_json::from_str(line.trim()).unwrap_or_else(|error| panic!("{error}: {line:?}"));
    assert_eq!(ready.web.as_deref(), Some(dir.join("web.sock").as_path()), "the readiness line names the web socket");
    Gateway { child, _stdin: stdin, ready }
}

/// A tunnel to a name the host file pins, with a grant file for the port.
fn grant_file(name: &str, destination: &str, scope: &str) -> PathBuf {
    let dir = scratch(name);
    let path = dir.join("grants.json");
    std::fs::write(
        &path,
        serde_json::json!([{
            "gateway": "web", "operation": "connect", "destination": destination, "scope": scope
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
    let grants = grant_file("web-granted-file", &format!("pinned.example:{port}"), "task");
    let gateway = with_web("web-granted-gw", &[&grants], &[], Some(&hosts), None);
    let web = gateway.ready.web.clone().unwrap();
    let mut tunnel = UnixStream::connect(&web).unwrap();
    tunnel.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let requested = format!("CONNECT pinned.example:{port} HTTP/1.1\r\n\r\n");
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
    assert!(seen.starts_with(STATUS.as_bytes()), "the gateway's head is first: {}", String::from_utf8_lossy(&seen));
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
