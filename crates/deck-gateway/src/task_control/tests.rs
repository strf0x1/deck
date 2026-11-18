//! The surface against every exchange in the vendored contract's
//! `control.exchanges`, with a scripted `deckd`: the gateway's own refusals
//! (token, route, body, the set) must match the matrix byte for byte and
//! never reach the daemon, and the daemon's answers must leave with the
//! table's status and body.

use super::*;
use serde_json::{Value, json};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

fn vendored() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let lock = std::fs::read_to_string(root.join("CONTRACT.lock")).unwrap();
    let line = lock.lines().find(|line| line.starts_with("vendored")).unwrap();
    root.join(line.split_once('=').unwrap().1.trim().trim_matches('"'))
}

fn control() -> Value {
    let matrix: Value =
        serde_json::from_str(&std::fs::read_to_string(vendored().join("matrix.json")).unwrap()).unwrap();
    matrix["control"].clone()
}

/// A request in, a reply out.
struct Duplex {
    input: Cursor<Vec<u8>>,
    output: Vec<u8>,
}

impl Read for Duplex {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.input.read(buf)
    }
}

impl Write for Duplex {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.output.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Answers with what it was given, and remembers what it heard.
struct Scripted {
    answer: Mutex<Option<Outcome>>,
    heard: Mutex<Vec<(String, Value)>>,
}

impl Daemon for Scripted {
    fn call(&self, coordinator: &str, operation: &wire::Operation) -> Outcome {
        self.heard.lock().unwrap().push((coordinator.into(), serde_json::to_value(operation).unwrap()));
        self.answer.lock().unwrap().clone().expect("the daemon was not expected to hear this")
    }
}

fn exchange(surface: &Surface, raw: Vec<u8>) -> (u16, Vec<(String, String)>, Value) {
    let mut duplex = Duplex { input: Cursor::new(raw), output: Vec::new() };
    surface.handle(&mut duplex).unwrap();
    let text = String::from_utf8(duplex.output).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(": ").unwrap();
            (name.to_string(), value.to_string())
        })
        .collect();
    (status, headers, serde_json::from_str(body).unwrap())
}

fn request(method: &str, path: &str, headers: &[(String, String)], body: &[u8]) -> Vec<u8> {
    let mut raw = format!("{method} {path} HTTP/1.1\r\nhost: deck\r\n");
    for (name, value) in headers {
        raw.push_str(&format!("{name}: {value}\r\n"));
    }
    raw.push_str(&format!("content-length: {}\r\n\r\n", body.len()));
    let mut raw = raw.into_bytes();
    raw.extend_from_slice(body);
    raw
}

fn surface(token: Option<&str>, daemon: Arc<Scripted>) -> Surface {
    Surface::new("01JD3C00RD1NAT0R0000000001".into(), token.map(|t| Token::new(t).unwrap()), daemon)
}

#[test]
fn every_contract_exchange_gets_the_matrixs_reply() {
    let control = control();
    let exchanges = control["exchanges"].as_array().unwrap();
    assert!(exchanges.len() >= 45);
    for case in exchanges {
        let name = case["case"].as_str().unwrap();
        let expect = &case["expect"];
        let status = expect["status"].as_u64().unwrap() as u16;
        let code = expect["code"].as_str().and_then(wire::Code::named);
        // The gateway's own refusals never reach deckd; everything else is
        // deckd's answer, scripted here as the matrix states it.
        let gateways = matches!(code, Some(wire::Code::BadRequest | wire::Code::Unauthorized))
            || name.starts_with("operation-outside-set");
        let answer = match (gateways, code) {
            (true, _) => None,
            (false, None) => Some(Outcome::Reply(expect["body"].clone())),
            (false, Some(code)) => {
                Some(Outcome::Refused(wire::Refusal::new(code, expect["body"]["error"]["message"].as_str().unwrap())))
            }
        };
        let daemon = Arc::new(Scripted { answer: Mutex::new(answer), heard: Mutex::new(vec![]) });
        let surface = surface(case["token"].as_str(), daemon.clone());
        let headers: Vec<(String, String)> = case["headers"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, value)| (name.clone(), value.as_str().unwrap().to_string()))
            .collect();
        let raw = request(
            case["method"].as_str().unwrap_or("POST"),
            case["path"].as_str().unwrap_or("/control"),
            &headers,
            case["body"].to_string().as_bytes(),
        );
        let (got, reply_headers, body) = exchange(&surface, raw);
        assert_eq!(got, status, "{name}: {body}");
        assert_eq!(body, expect["body"], "{name}");
        let challenge = reply_headers.iter().any(|(n, v)| n == "www-authenticate" && v == CHALLENGE);
        assert_eq!(challenge, status == 401, "{name}: the challenge rides only a 401");
        assert!(!reply_headers.iter().any(|(n, _)| n.starts_with("access-control-")), "{name}: no CORS");
        let heard = daemon.heard.lock().unwrap();
        assert_eq!(heard.len(), usize::from(!gateways), "{name}");
        if let Some((coordinator, operation)) = heard.first() {
            assert_eq!(coordinator, "01JD3C00RD1NAT0R0000000001", "{name}: the task is the gateway's, not the guest's");
            assert_eq!(operation, &case["body"], "{name}: the typed operation is the guest's request");
        }
    }
}

#[test]
fn a_large_or_lengthless_body_is_refused_before_it_is_read() {
    let daemon = Arc::new(Scripted { answer: Mutex::new(None), heard: Mutex::new(vec![]) });
    let surface = surface(None, daemon);
    // The head promises more than the limit and sends none of it: the reply
    // must not wait for a body.
    let raw = format!("POST /control HTTP/1.1\r\ncontent-length: {}\r\n\r\n", wire::MAX_BODY + 1).into_bytes();
    let (status, _, body) = exchange(&surface, raw);
    assert_eq!((status, body["error"]["code"].as_str()), (413, Some("deck_control_bad_request")));
    let (status, _, _) = exchange(&surface, b"POST /control HTTP/1.1\r\n\r\n".to_vec());
    assert_eq!(status, 411);
    let (status, _, _) = exchange(&surface, b"POST /control HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n".to_vec());
    assert_eq!(status, 400, "chunked bodies are refused");
}

#[test]
fn a_loopback_surface_needs_a_token_and_a_loopback_address() {
    let daemon = Arc::new(Scripted { answer: Mutex::new(None), heard: Mutex::new(vec![]) });
    let open = Surface::new("c".into(), None, daemon.clone());
    assert!(serve_loopback(open, "127.0.0.1:0".parse().unwrap()).unwrap_err().to_string().contains(TOKEN_ENV));
    let token = Some(Token::new(&"a".repeat(32)).unwrap());
    let wide = Surface::new("c".into(), token, daemon);
    assert!(serve_loopback(wide, "0.0.0.0:0".parse().unwrap()).is_err());
}

#[test]
fn a_success_is_the_daemons_body() {
    let reply = json!({ "turn": 2 });
    let daemon =
        Arc::new(Scripted { answer: Mutex::new(Some(Outcome::Reply(reply.clone()))), heard: Mutex::new(vec![]) });
    let surface = surface(None, daemon);
    let body = br#"{"operation":"prompt","id":"t1","text":"go"}"#;
    let (status, headers, got) = exchange(&surface, request("POST", "/control", &[], body));
    assert_eq!((status, got), (200, reply));
    assert!(headers.iter().any(|(n, v)| n == "content-type" && v == "application/json"));
}

/// Over real TCP: a token-less request with a body gets its 401 even when
/// the client reads only after the gateway has closed (deck#152's Linux CI
/// saw a reset here instead).
#[test]
fn a_tcp_client_reads_its_refusal_after_the_close() {
    let daemon = Arc::new(Scripted { answer: Mutex::new(None), heard: Mutex::new(vec![]) });
    let surface = surface(Some(&"a".repeat(32)), daemon);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        surface.handle(stream).unwrap();
    });
    let mut client = std::net::TcpStream::connect(address).unwrap();
    let body = format!(r#"{{"operation":"tasks","pad":"{}"}}"#, "x".repeat(16 * 1024));
    client.write_all(&request("POST", "/control", &[], body.as_bytes())).unwrap();
    server.join().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let mut reply = String::new();
    client.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 401"), "{reply}");
}

#[test]
fn the_contracts_request_payloads_parse_exactly_as_it_says() {
    let payloads: Value =
        serde_json::from_str(&std::fs::read_to_string(vendored().join("payloads.json")).unwrap()).unwrap();
    let mut seen = 0;
    for case in payloads["cases"].as_array().unwrap() {
        let schema = case["schema"].as_str().unwrap();
        if !(schema.starts_with("control-") && schema.ends_with("-request.schema.json")) {
            continue;
        }
        seen += 1;
        // A schema is one operation's; another operation is invalid for it.
        let operation = schema.trim_start_matches("control-").trim_end_matches("-request.schema.json");
        let parsed = wire::parse(case["payload"].to_string().as_bytes());
        let accepted = parsed.as_ref().is_ok_and(|parsed| parsed.name() == operation);
        assert_eq!(accepted, case["valid"] == true, "{}: {parsed:?}", case["case"]);
    }
    assert!(seen > 20, "the contract has control request fixtures");
}
