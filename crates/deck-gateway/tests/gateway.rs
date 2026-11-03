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
use std::time::Duration;

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
        .arg("--exit-on-stdin-eof")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for model in models {
        command.args(["--model", model]);
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
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut stream = stream;
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let request = http::read(&mut BufReader::new(&stream)).unwrap().unwrap();
        assert(&request);
        let body = r#"{"id":"chatcmpl-1","choices":[{"delta":{"content":"ok"}}]}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
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
