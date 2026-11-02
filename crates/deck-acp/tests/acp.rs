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
