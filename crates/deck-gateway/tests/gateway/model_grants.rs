//! Model grant cancellation through real guest and idle provider sockets.

use super::*;
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver};

struct Provider {
    port: u16,
    requests: Receiver<(String, TcpStream)>,
    closed: Receiver<String>,
}

impl Provider {
    fn idle(headers: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (requests, received) = mpsc::channel();
        let (closed, observed) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let requests = requests.clone();
                let closed = closed.clone();
                std::thread::spawn(move || {
                    let mut stream = stream;
                    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                    let request = http::read(&mut BufReader::new(&stream)).unwrap().unwrap();
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    let model = body["model"].as_str().unwrap().to_owned();
                    if headers {
                        stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n").unwrap();
                        stream.write_all(b"d\r\ndata: first\n\n\r\n").unwrap();
                        stream.flush().unwrap();
                    }
                    requests.send((model.clone(), stream.try_clone().unwrap())).unwrap();
                    let mut byte = [0];
                    if matches!(stream.read(&mut byte), Ok(0)) {
                        let _ = closed.send(model);
                    }
                });
            }
        });
        Self { port, requests: received, closed: observed }
    }

    fn request(&self, model: &str) -> TcpStream {
        let (seen, stream) = self.requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(seen, model);
        stream
    }
}

fn rule(model: &str, scope: &str) -> Value {
    serde_json::json!({
        "id": format!("grant-{model}"), "gateway": "model", "operation": "chat_completions",
        "destination": model, "scope": scope, "task": "t", "project": "/fixture"
    })
}

fn policy(path: &Path, rules: &[Value]) {
    let replacement = path.with_extension("tmp");
    std::fs::write(&replacement, serde_json::json!({"rules": rules}).to_string()).unwrap();
    std::fs::rename(replacement, path).unwrap();
}

fn launch(name: &str, provider: &Provider, path: &Path, scope: &'static str) -> Gateway {
    let file = path.to_path_buf();
    let once = std::sync::atomic::AtomicBool::new(false);
    let (socket, _) = interactive_queue(&format!("{name}-queue"), move |request| {
        if request["op"] != "approval" {
            return Value::Null;
        }
        let model = request["destination"].as_str().unwrap();
        let rules: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        let allowed = if model == "once" {
            !once.swap(true, std::sync::atomic::Ordering::SeqCst)
        } else {
            rules["rules"].as_array().unwrap().iter().any(|r| r["destination"] == model)
        };
        serde_json::json!({
            "id": "approval-model", "destination": model,
            "state": if allowed { serde_json::json!({"allowed": if model == "once" { "once" } else { scope }}) }
                     else { serde_json::json!("pending") }
        })
    });
    let gateway = start_launch(
        name,
        &format!("http://127.0.0.1:{}/v1", provider.port),
        &["configured"],
        Some("host-key"),
        Launch { control: Some((&socket, "t")), grants: Some(path), ..Launch::default() },
    );
    assert!(gateway.ready.web.is_none(), "model grants work with web disabled");
    gateway
}

fn guest(gateway: &Gateway, model: &str, streaming: bool) -> UnixStream {
    let mut stream = UnixStream::connect(&gateway.ready.model).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    stream.write_all(chat(model).as_bytes()).unwrap();
    if streaming {
        let mut seen = String::new();
        let mut byte = [0];
        while !seen.contains("first") {
            assert_eq!(stream.read(&mut byte).unwrap(), 1, "{seen}");
            seen.push(char::from(byte[0]));
        }
    }
    stream
}

#[test]
fn revoking_a_timed_model_grant_closes_idle_sockets_and_preserves_unrelated_and_once_streams() {
    let provider = Provider::idle(true);
    let path = scratch("model-stream-grants").join("grants.json");
    policy(&path, &[rule("extra", "timed"), rule("other", "timed")]);
    let gateway = launch("mg-s", &provider, &path, "timed");
    let mut extra = guest(&gateway, "extra", true);
    let _extra_provider = provider.request("extra");
    let mut survivors = Vec::new();
    for model in ["other", "configured", "once"] {
        survivors.push((guest(&gateway, model, true), provider.request(model)));
    }
    policy(&path, &[rule("other", "timed")]);
    let mut rest = String::new();
    extra.read_to_string(&mut rest).expect("revocation must close the guest even while the provider is idle");
    assert!(!rest.contains("0\r\n\r\n"), "an interrupted stream cannot claim completion");
    assert_eq!(provider.closed.recv_timeout(Duration::from_secs(2)).unwrap(), "extra");
    for (mut guest, mut upstream) in survivors {
        upstream.write_all(b"c\r\ndata: last\n\n\r\n0\r\n\r\n").unwrap();
        let mut rest = String::new();
        guest.read_to_string(&mut rest).unwrap();
        assert!(rest.contains("last") && rest.ends_with("0\r\n\r\n"), "{rest}");
    }
    assert!(send(&gateway.ready.model, &chat("once")).starts_with("HTTP/1.1 403"));
    let refused = send(&gateway.ready.model, &chat("extra"));
    assert!(refused.starts_with("HTTP/1.1 403") && refused.contains("approval-model"), "{refused}");
    assert!(provider.requests.recv_timeout(Duration::from_millis(150)).is_err());
}

#[test]
fn revoking_a_project_model_grant_before_headers_answers_once_and_closes_the_provider() {
    let provider = Provider::idle(false);
    let path = scratch("model-waiting-grants").join("projects.json");
    policy(&path, &[rule("extra", "project")]);
    let gateway = launch("mg-w", &provider, &path, "project");
    let mut guest = guest(&gateway, "extra", false);
    let _upstream = provider.request("extra");
    policy(&path, &[]);
    let mut response = String::new();
    guest.read_to_string(&mut response).expect("revocation must interrupt the provider's idle header read");
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("deck_boundary_denied"), "{response}");
    assert_eq!(response.matches("HTTP/1.1").count(), 1, "{response}");
    assert_eq!(provider.closed.recv_timeout(Duration::from_secs(2)).unwrap(), "extra");
}

#[test]
fn an_expiring_model_grant_closes_idle_sockets_while_the_daemon_is_down() {
    let provider = Provider::idle(true);
    let path = scratch("model-expiry").join("grants.json");
    let mut grant = rule("extra", "timed");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
    grant["expires_ms"] = serde_json::json!(now + 1500);
    policy(&path, &[grant]);
    let gateway = launch("mg-e", &provider, &path, "timed");
    let mut guest = guest(&gateway, "extra", true);
    let _upstream = provider.request("extra");
    gateway.deckd.stop();
    let mut rest = Vec::new();
    guest.read_to_end(&mut rest).unwrap();
    assert_eq!(provider.closed.recv_timeout(Duration::from_secs(2)).unwrap(), "extra");
}

#[test]
fn credential_revocation_also_interrupts_an_idle_provider() {
    let provider = Provider::idle(false);
    let gateway = start("mg-key", &format!("http://127.0.0.1:{}/v1", provider.port), &["configured"], Some("host-key"));
    eventually(|| !gateway.deckd.keys.lock().unwrap().watchers.is_empty());
    let mut guest = guest(&gateway, "configured", false);
    let _upstream = provider.request("configured");
    gateway.deckd.revoke("DECK_GATEWAY_TEST_KEY");
    let mut response = String::new();
    guest.read_to_string(&mut response).unwrap();
    assert!(response.contains("deck_credential_revoked"), "{response}");
    assert_eq!(response.matches("HTTP/1.1").count(), 1, "{response}");
    assert_eq!(provider.closed.recv_timeout(Duration::from_secs(2)).unwrap(), "configured");
}

#[test]
fn a_task_approval_without_a_persisted_rule_refuses_before_fetching_credentials() {
    let (socket, _) =
        interactive_queue("mg-missing-q", |_| serde_json::json!({"id": "a", "state": {"allowed": "task"}}));
    let gateway = start_with("mg-missing", "http://127.0.0.1:9/v1", &["configured"], Some("key"), Some((&socket, "t")));
    let response = send(&gateway.ready.model, &chat("extra"));
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("authorizing model grant is no longer present"), "{response}");
    assert!(gateway.deckd.fetched().is_empty());
}

#[test]
fn a_new_task_or_project_approval_keeps_its_stream_revocable() {
    for (label, scope) in [("t", "task"), ("p", "project")] {
        let provider = Provider::idle(true);
        let path = scratch(&format!("mh-file-{label}")).join("grants.json");
        policy(&path, &[]);
        let persisted = path.clone();
        let (socket, _) = interactive_queue(&format!("mhq-{label}"), move |request| {
            if request["op"] != "approval" {
                return Value::Null;
            }
            // The daemon's ordering: persist the decision before returning it.
            policy(&persisted, &[rule("extra", scope)]);
            serde_json::json!({"id": "held", "state": {"allowed": scope}})
        });
        let gateway = start_launch(
            &format!("mh-{label}"),
            &format!("http://127.0.0.1:{}/v1", provider.port),
            &["configured"],
            Some("key"),
            Launch { control: Some((&socket, "t")), grants: Some(&path), ..Launch::default() },
        );
        let mut guest = guest(&gateway, "extra", true);
        let _upstream = provider.request("extra");
        policy(&path, &[]);
        guest.read_to_end(&mut Vec::new()).unwrap();
        assert_eq!(provider.closed.recv_timeout(Duration::from_secs(2)).unwrap(), "extra");
    }
}
