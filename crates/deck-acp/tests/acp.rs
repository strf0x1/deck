//! The ACP client against the scripted fake agent: over a child's stdio, and
//! over a Unix socket to an agent it did not start. Both run the same
//! turn: handshake, a streamed prompt, a permission answer that reaches the
//! agent, and a tool that reports failed because the answer was reject.

use deck_acp::{Client, Inbound, ToolStatus};
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use futures::executor::block_on;
use std::time::Duration;

/// A hung agent fails the test instead of hanging the run.
fn hang_guard() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(20));
        eprintln!("timed out");
        std::process::exit(1);
    });
}

#[expect(clippy::expect_used, reason = "a test helper: an early close is the failure")]
fn next(rx: &mut UnboundedReceiver<Inbound>) -> Inbound {
    block_on(rx.next()).expect("the connection closed early")
}

#[expect(clippy::unwrap_used, reason = "a test helper: a rejected write is the failure")]
fn turn(client: &Client, rx: &mut UnboundedReceiver<Inbound>) {
    assert_eq!(next(rx), Inbound::Ready { agent: "fake agent".into(), session_id: "fake-1".into() });
    client.prompt("ship it").unwrap();

    let mut text = String::new();
    let request = loop {
        match next(rx) {
            Inbound::Message(t) => text.push_str(&t),
            Inbound::Permission { request, title, options } => {
                assert_eq!(title, "shell git push origin deck/spike");
                assert_eq!(options.iter().filter(|o| o.allows()).count(), 2);
                break request;
            }
            _ => {}
        }
    };
    assert!(text.contains("You said \"ship it\""), "{text}");

    client.answer(&request, Some("reject")).unwrap();
    let mut failed_tool = false;
    loop {
        match next(rx) {
            Inbound::Message(t) => text.push_str(&t),
            Inbound::ToolStatus { status: ToolStatus::Failed, .. } => failed_tool = true,
            Inbound::TurnEnded(stop) => {
                assert_eq!(stop, "end_turn");
                break;
            }
            Inbound::Failed(e) => panic!("{e}"),
            _ => {}
        }
    }
    assert!(failed_tool, "a rejected tool reports failed");
    assert!(text.ends_with("permission: reject"), "{text}");
}

#[test]
fn a_turn_with_a_permission_answer() {
    use std::process::{Command, Stdio};
    hang_guard();
    let mut child = Command::new(env!("CARGO_BIN_EXE_deck-fake-agent"))
        .env("DECK_FAKE_DELAY_MS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let (client, mut rx) = Client::over(std::env::temp_dir(), stdout, stdin).unwrap();
    turn(&client, &mut rx);
    // Closing the connection is the agent's end of file; it exits by itself.
    drop(client);
    assert!(child.wait().unwrap().success());
}

#[cfg(unix)]
#[test]
fn the_agent_outlives_a_client_it_did_not_launch() {
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;
    use std::process::{Child, Command, ExitStatus, Stdio};

    fn connect(path: &std::path::Path) -> UnixStream {
        for _ in 0..500 {
            if let Ok(s) = UnixStream::connect(path) {
                return s;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the fake agent never listened on {}", path.display());
    }

    fn wait_exit(child: &mut Child) -> Option<ExitStatus> {
        for _ in 0..500 {
            if let Ok(Some(status)) = child.try_wait() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    hang_guard();
    let socket = std::env::temp_dir().join(format!("deck-fake-agent-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket);
    let mut child = Command::new(env!("CARGO_BIN_EXE_deck-fake-agent"))
        .arg("--listen")
        .arg(&socket)
        .env("DECK_FAKE_DELAY_MS", "0")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();

    let stream = connect(&socket);
    let input = BufReader::new(stream.try_clone().unwrap());
    let (client, mut rx) = Client::over(std::env::temp_dir(), input, stream).unwrap();
    turn(&client, &mut rx);

    // Deck holds no process here: closing the connection only ends the
    // stream. The agent reads end of file and exits by itself, so the status
    // is a code, not the signal a kill would leave behind.
    drop(client);
    let status = wait_exit(&mut child).expect("the agent outlived its closed connection");
    assert_eq!(status.code(), Some(0), "the agent was killed, not closed out");
    let _ = std::fs::remove_file(&socket);
}

/// The fake agent's sessions outlive its process, as construct's do: a
/// second process loads the first one's session from `DECK_FAKE_STATE`,
/// replaying it, and refuses what construct refuses.
#[test]
fn a_session_outlives_the_fake_agents_process() {
    use serde_json::{Value, json};
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

    struct Raw {
        child: Child,
        input: ChildStdin,
        output: BufReader<ChildStdout>,
        next: u64,
    }
    impl Raw {
        fn start(state: &std::path::Path) -> Raw {
            let mut child = Command::new(env!("CARGO_BIN_EXE_deck-fake-agent"))
                .env("DECK_FAKE_DELAY_MS", "0")
                .env("DECK_FAKE_STATE", state)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let input = child.stdin.take().unwrap();
            let output = BufReader::new(child.stdout.take().unwrap());
            Raw { child, input, output, next: 0 }
        }
        fn send(&mut self, v: Value) {
            writeln!(self.input, "{v}").unwrap();
        }
        fn read(&mut self) -> Value {
            let mut line = String::new();
            self.output.read_line(&mut line).unwrap();
            serde_json::from_str(&line).unwrap()
        }
        /// Send a request; return the notifications before its reply, and the reply.
        fn call(&mut self, method: &str, params: Value) -> (Vec<Value>, Value) {
            self.next += 1;
            let id = self.next;
            self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
            let mut before = vec![];
            loop {
                let v = self.read();
                if v["id"] == json!(id) && v.get("method").is_none() {
                    return (before, v);
                }
                before.push(v);
            }
        }
    }

    hang_guard();
    let state = std::env::temp_dir().join(format!("deck-fake-state-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state).unwrap();

    let mut first = Raw::start(&state);
    let (_, init) = first.call("initialize", json!({}));
    assert_eq!(init["result"]["agentCapabilities"]["loadSession"], json!(true));
    assert!(init["result"]["agentCapabilities"]["sessionCapabilities"]["close"].is_object());
    let (_, new) = first.call("session/new", json!({ "cwd": "/" }));
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
    let prompt = json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "ship it" }] });
    first.next += 1;
    let prompt_id = first.next;
    first.send(json!({ "jsonrpc": "2.0", "id": prompt_id, "method": "session/prompt", "params": prompt }));
    loop {
        let v = first.read();
        if v["method"] == "session/request_permission" {
            first.send(json!({ "jsonrpc": "2.0", "id": v["id"], "result": { "outcome": { "optionId": "allow" } } }));
        }
        if v["id"] == json!(prompt_id) && v.get("method").is_none() {
            break;
        }
    }
    drop(first.input);
    assert!(first.child.wait().unwrap().success());

    let mut second = Raw::start(&state);
    second.call("initialize", json!({}));
    let (replayed, loaded) = second.call("session/load", json!({ "sessionId": sid, "cwd": "/", "mcpServers": [] }));
    assert_eq!(loaded["result"], json!({}));
    let said: Vec<(&str, &str)> = replayed
        .iter()
        .map(|v| {
            let u = &v["params"]["update"];
            (u["sessionUpdate"].as_str().unwrap(), u["content"]["text"].as_str().unwrap())
        })
        .collect();
    assert_eq!(said.len(), 2, "{said:?}");
    assert_eq!(said[0], ("user_message_chunk", "ship it"));
    assert_eq!(said[1].0, "agent_message_chunk");
    assert!(said[1].1.ends_with("permission: allow"), "{}", said[1].1);

    let (_, again) = second.call("session/load", json!({ "sessionId": sid, "cwd": "/", "mcpServers": [] }));
    assert_eq!(again["error"]["code"], json!(-32602), "already loaded on this connection");
    let (_, closed) = second.call("session/close", json!({ "sessionId": sid }));
    assert_eq!(closed["result"], json!({}));
    let (_, closed) = second.call("session/close", json!({ "sessionId": sid }));
    assert_eq!(closed["error"]["code"], json!(-32002), "a second close is not found, which clients treat as done");
    let (replayed, resumed) = second.call("session/resume", json!({ "sessionId": sid, "cwd": "/", "mcpServers": [] }));
    assert_eq!((replayed.len(), &resumed["result"]), (0, &json!({})), "resume does not replay");
    let (_, missing) = second.call("session/load", json!({ "sessionId": "nope", "cwd": "/", "mcpServers": [] }));
    assert_eq!(missing["error"]["code"], json!(-32002));

    drop(second.input);
    assert!(second.child.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(&state);
}
