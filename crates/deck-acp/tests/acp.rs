//! The ACP client against the scripted fake agent: over a child's stdio, and
//! over a Unix socket to an agent it did not start. Both run the same
//! turn: handshake, a streamed prompt, a permission answer that reaches the
//! agent, and a tool that reports failed because the answer was reject.

use deck_acp::{Client, Inbound, ToolStatus, TurnState};
use futures::StreamExt as _;
use futures::channel::mpsc::{TryRecvError, UnboundedReceiver};
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

/// The same, but a name the wrong namespace never sends fails the test instead
/// of hanging it.
fn next_before(rx: &mut UnboundedReceiver<Inbound>, what: &str) -> Inbound {
    for _ in 0..500 {
        match rx.try_recv() {
            Ok(event) => return event,
            Err(TryRecvError::Closed) => panic!("the agent closed before {what}"),
            Err(TryRecvError::Empty) => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
    panic!("no {what} from the agent");
}

#[expect(clippy::unwrap_used, reason = "a test helper: a rejected write is the failure")]
fn turn(client: &Client, rx: &mut UnboundedReceiver<Inbound>) {
    assert_eq!(next(rx), Inbound::Ready { agent: "fake agent".into(), session_id: "fake-1".into(), turn: None });
    client.prompt("ship it").unwrap();

    let mut text = String::new();
    let mut tool_seen = false;
    let request = loop {
        match next(rx) {
            Inbound::Message(t) => text.push_str(&t),
            Inbound::Tool { id, title, status } => {
                assert_eq!(id, "t1");
                assert_eq!(title, "shell git push origin deck/spike");
                assert_eq!(status, ToolStatus::Running);
                tool_seen = true;
            }
            Inbound::Permission { request, title, options } => {
                assert_eq!(title, "shell git push origin deck/spike");
                assert_eq!(options.iter().filter(|o| o.allows()).count(), 2);
                break request;
            }
            _ => {}
        }
    };
    assert!(tool_seen, "the live tool appears before its permission question");
    assert!(text.contains("You said \"ship it\""), "{text}");

    client.answer(&request, Some("reject")).unwrap();
    let mut failed_tool = false;
    loop {
        match next(rx) {
            Inbound::Message(t) => text.push_str(&t),
            Inbound::ToolStatus { status: ToolStatus::Failed, .. } => failed_tool = true,
            Inbound::Turn { state: TurnState::Idle { stop_reason: Some(stop) }, .. } | Inbound::TurnEnded(stop) => {
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
    assert_eq!(loaded["result"]["_meta"]["construct"]["turn"]["state"], "idle");
    let said: Vec<(&str, &str)> = replayed
        .iter()
        .filter_map(|v| {
            let u = &v["params"]["update"];
            Some((u["sessionUpdate"].as_str()?, u["content"]["text"].as_str()?))
        })
        .collect();
    assert_eq!(said[0], ("user_message_chunk", "ship it"));
    let reply: String = said.iter().filter(|(kind, _)| *kind == "agent_message_chunk").map(|(_, text)| *text).collect();
    assert_eq!(reply, "You said \"ship it\". Checking the workspace first.\npermission: allow");

    let (_, again) = second.call("session/load", json!({ "sessionId": sid, "cwd": "/", "mcpServers": [] }));
    assert_eq!(again["error"]["code"], json!(-32602), "already loaded on this connection");
    let (_, closed) = second.call("session/close", json!({ "sessionId": sid }));
    assert_eq!(closed["result"], json!({}));
    let (_, closed) = second.call("session/close", json!({ "sessionId": sid }));
    assert_eq!(closed["result"], json!({}), "close is idempotent");
    let (replayed, resumed) = second.call("session/resume", json!({ "sessionId": sid, "cwd": "/", "mcpServers": [] }));
    assert_eq!((replayed.len(), &resumed["result"]), (0, &json!({})), "resume does not replay");
    let (_, missing) = second.call("session/load", json!({ "sessionId": "nope", "cwd": "/", "mcpServers": [] }));
    assert_eq!(missing["error"]["code"], json!(-32002));

    drop(second.input);
    assert!(second.child.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn client_load_restores_a_cut_off_first_turn_without_repeating_it() {
    use std::io::BufReader;
    use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

    fn spawn(state: &std::path::Path) -> (Child, BufReader<ChildStdout>, ChildStdin) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_deck-fake-agent"))
            .env("DECK_FAKE_DELAY_MS", "0")
            .env("DECK_FAKE_STATE", state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = BufReader::new(child.stdout.take().unwrap());
        let output = child.stdin.take().unwrap();
        (child, input, output)
    }

    fn replay(rx: &mut UnboundedReceiver<Inbound>, sid: &str) -> Vec<Inbound> {
        let mut events = Vec::new();
        loop {
            match next(rx) {
                Inbound::Ready { agent, session_id, .. } => {
                    assert_eq!((agent.as_str(), session_id.as_str()), ("fake agent", sid));
                    return events;
                }
                Inbound::Failed(error) => panic!("{error}"),
                event => events.push(event),
            }
        }
    }

    hang_guard();
    let state = std::env::temp_dir().join(format!("deck-fake-interrupted-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    let cwd = std::env::temp_dir();
    let (mut first, input, output) = spawn(&state);
    let (client, mut rx) = Client::over(cwd.clone(), input, output).unwrap();
    let Inbound::Ready { session_id, .. } = next(&mut rx) else { panic!("expected Ready") };
    client.prompt("first unfinished turn").unwrap();
    loop {
        match next(&mut rx) {
            Inbound::Permission { .. } => break,
            Inbound::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    first.kill().unwrap();
    first.wait().unwrap();
    drop(client);

    let (mut second, input, output) = spawn(&state);
    let (client, mut rx) = Client::load(cwd.clone(), input, output, session_id.clone()).unwrap();
    let events = replay(&mut rx, &session_id);
    assert_eq!(events.first(), Some(&Inbound::UserMessage("first unfinished turn".into())));
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            Inbound::Message(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text,
        "You said \"first unfinished turn\". Checking the workspace first.\nTurn interrupted when the agent stopped. Nothing was retried."
    );
    assert_eq!(events.iter().filter(|event| matches!(event, Inbound::Tool { .. })).count(), 1);
    assert!(events.contains(&Inbound::ToolStatus { id: "t1".into(), status: ToolStatus::Failed }));
    assert!(!events.iter().any(|event| matches!(event, Inbound::Permission { .. } | Inbound::TurnEnded(_))));
    drop(client);
    assert!(second.wait().unwrap().success());

    let (mut third, input, output) = spawn(&state);
    let (client, mut rx) = Client::load(cwd, input, output, session_id.clone()).unwrap();
    assert_eq!(replay(&mut rx, &session_id), events, "loading again does not repeat recovery or the turn");
    drop(client);
    assert!(third.wait().unwrap().success());
    std::fs::remove_dir_all(state).unwrap();
}

#[test]
fn a_new_session_can_be_loaded_before_any_prompt() {
    use std::io::BufReader;
    use std::process::{Command, Stdio};

    hang_guard();
    let state = std::env::temp_dir().join(format!("deck-fake-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    let mut sid: Option<String> = None;
    for _ in 0..2 {
        let mut child = Command::new(env!("CARGO_BIN_EXE_deck-fake-agent"))
            .env("DECK_FAKE_STATE", &state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = BufReader::new(child.stdout.take().unwrap());
        let output = child.stdin.take().unwrap();
        let (client, mut rx) = match &sid {
            Some(id) => Client::load(std::env::temp_dir(), input, output, id.clone()),
            None => Client::over(std::env::temp_dir(), input, output),
        }
        .unwrap();
        let Inbound::Ready { session_id, .. } = next(&mut rx) else { panic!("expected Ready") };
        if let Some(id) = &sid {
            assert_eq!(id, &session_id);
        }
        sid = Some(session_id);
        drop(client);
        assert!(child.wait().unwrap().success());
    }
    std::fs::remove_dir_all(state).unwrap();
}

#[test]
fn a_boundary_notice_reaches_the_fake_agent_that_advertised_it() {
    use std::io::BufReader;
    use std::process::{Command, Stdio};

    hang_guard();
    let mut child = Command::new(env!("CARGO_BIN_EXE_deck-fake-agent"))
        .env("DECK_FAKE_DELAY_MS", "0")
        .env("DECK_BOUNDARY_URL", "http://127.0.0.1:9")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let input = BufReader::new(child.stdout.take().unwrap());
    let output = child.stdin.take().unwrap();
    let (client, mut rx) = Client::over(std::env::temp_dir(), input, output).unwrap();
    let Inbound::Ready { .. } = next(&mut rx) else { panic!("expected Ready") };
    client
        .boundary_notice(&serde_json::json!({
            "approval_id": "a1f3",
            "destination": "index.crates.io:443",
            "outcome": "allowed",
            "scope": "task",
        }))
        .unwrap();
    let note = loop {
        match next(&mut rx) {
            Inbound::Message(text) => break text,
            Inbound::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    assert!(note.contains("[deck boundary] index.crates.io:443"), "{note}");
    assert!(note.contains("approval a1f3"), "{note}");
    assert!(note.contains("is now allowed (scope: task)"), "{note}");
    drop(client);
    assert!(child.wait().unwrap().success());
}

#[test]
fn an_agent_without_the_capability_is_never_sent_a_boundary_notice() {
    use std::io::BufReader;
    use std::process::{Command, Stdio};

    hang_guard();
    let mut child = Command::new(env!("CARGO_BIN_EXE_deck-fake-agent"))
        .env("DECK_FAKE_DELAY_MS", "0")
        .env_remove("DECK_BOUNDARY_URL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let input = BufReader::new(child.stdout.take().unwrap());
    let output = child.stdin.take().unwrap();
    let (client, mut rx) = Client::over(std::env::temp_dir(), input, output).unwrap();
    let Inbound::Ready { .. } = next(&mut rx) else { panic!("expected Ready") };
    // The client drops it instead of writing a method the agent never offered.
    client.boundary_notice(&serde_json::json!({ "approval_id": "a1" })).unwrap();
    client.prompt("ship it").unwrap();
    let mut text = String::new();
    loop {
        match next(&mut rx) {
            Inbound::Message(chunk) => text.push_str(&chunk),
            Inbound::Permission { request, .. } => {
                client.answer(&request, Some("reject")).unwrap();
            }
            Inbound::Turn { state: TurnState::Idle { .. }, .. } | Inbound::TurnEnded(_) => break,
            Inbound::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(!text.contains("[deck boundary]"), "{text}");
    drop(client);
    assert!(child.wait().unwrap().success());
}

/// One namespace at a time, like a released Construct or the next release:
/// the turn method, the load snapshot and the boundary method must all follow
/// what that agent advertised (deck#36).
#[expect(clippy::unwrap_used, reason = "protocol setup and observations must succeed")]
fn one_namespace(names: &str) {
    use std::io::BufReader;
    use std::process::{Command, Stdio};

    hang_guard();
    let state = std::env::temp_dir().join(format!("deck-fake-names-{names}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_deck-fake-agent"))
            .env("DECK_FAKE_DELAY_MS", "0")
            .env("DECK_FAKE_NAMES", names)
            .env("DECK_FAKE_STATE", &state)
            .env("DECK_BOUNDARY_URL", "http://127.0.0.1:9")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
    };

    let mut first = spawn();
    let input = BufReader::new(first.stdout.take().unwrap());
    let output = first.stdin.take().unwrap();
    let (client, mut rx) = Client::over(std::env::temp_dir(), input, output).unwrap();
    let Inbound::Ready { session_id, .. } = next(&mut rx) else { panic!("{names}: expected Ready") };

    client.prompt("ship it").unwrap();
    loop {
        match next_before(&mut rx, &format!("{names} turn completion")) {
            Inbound::Permission { request, .. } => client.answer(&request, Some("allow")).unwrap(),
            Inbound::Turn { state: TurnState::Idle { .. }, .. } | Inbound::TurnEnded(_) => break,
            Inbound::Failed(error) => panic!("{names}: {error}"),
            _ => {}
        }
    }
    client
        .boundary_notice(&serde_json::json!({
            "approval_id": "a1f3",
            "destination": "index.crates.io:443",
            "outcome": "allowed",
            "scope": "task",
        }))
        .unwrap();
    loop {
        match next_before(&mut rx, &format!("{names} boundary note")) {
            Inbound::Message(text) if text.contains("[deck boundary]") => {
                assert!(text.contains("index.crates.io:443"), "{names}: {text}");
                break;
            }
            Inbound::Failed(error) => panic!("{names}: {error}"),
            _ => {}
        }
    }
    drop(client);
    assert!(first.wait().unwrap().success());

    let mut second = spawn();
    let input = BufReader::new(second.stdout.take().unwrap());
    let output = second.stdin.take().unwrap();
    let (client, mut rx) = Client::load(std::env::temp_dir(), input, output, session_id).unwrap();
    loop {
        match next_before(&mut rx, &format!("{names} load snapshot")) {
            Inbound::Ready { turn: Some(TurnState::Idle { .. }), .. } => break,
            Inbound::Ready { turn, .. } => panic!("{names}: load lost the idle snapshot: {turn:?}"),
            Inbound::Failed(error) => panic!("{names}: {error}"),
            _ => {}
        }
    }
    drop(client);
    assert!(second.wait().unwrap().success());
    std::fs::remove_dir_all(state).unwrap();
}

#[test]
fn a_released_construct_keeps_the_bare_names_working() {
    one_namespace("legacy");
}

#[test]
fn a_reverse_dns_only_construct_works_without_the_bare_names() {
    one_namespace("reverse");
}

/// The GitHub capability is advertised under the current key only, and only
/// when this process has a gateway to call (contract 1.2.0 §11.1).
#[test]
fn the_github_capability_needs_a_gateway_and_has_no_legacy_form() {
    use serde_json::{Value, json};

    fn meta(env: &[(&str, &str)]) -> Value {
        use std::io::{BufRead as _, BufReader, Write as _};
        use std::process::{Command, Stdio};

        hang_guard();
        let mut child = Command::new(env!("CARGO_BIN_EXE_deck-fake-agent"))
            .env("DECK_FAKE_DELAY_MS", "0")
            .env_remove("DECK_GITHUB_URL")
            .env_remove("DECK_BOUNDARY_URL")
            .env_remove("DECK_FAKE_NAMES")
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        writeln!(
            input,
            "{}",
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": 1,
                "clientCapabilities": { "_meta": { "com.sprawlcommons/construct": { "events": true } } },
            }})
        )
        .unwrap();
        let reply = loop {
            let mut line = String::new();
            output.read_line(&mut line).unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["id"] == json!(1) && value.get("method").is_none() {
                break value;
            }
        };
        drop(input);
        assert!(child.wait().unwrap().success());
        reply["result"]["agentCapabilities"]["_meta"].clone()
    }

    let without = meta(&[]);
    assert!(without["com.sprawlcommons/deck"]["github_operations"].is_null(), "{without}");
    let with = meta(&[("DECK_GITHUB_URL", "http://127.0.0.1:9")]);
    assert_eq!(with["com.sprawlcommons/deck"]["github_operations"], json!(1), "{with}");
    assert!(with["deck"]["github_operations"].is_null(), "the capability has no legacy form: {with}");
}
