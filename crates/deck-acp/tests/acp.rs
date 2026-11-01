//! The ACP client against a real subprocess (the scripted fake agent):
//! handshake, a streamed turn, and a permission answer that reaches the agent.

use deck_acp::{Client, Inbound, ToolStatus};
use futures::StreamExt as _;
use futures::executor::block_on;
use std::time::Duration;

#[test]
fn a_turn_with_a_permission_answer() {
    // A hung agent fails the test instead of hanging the run.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(20));
        eprintln!("timed out");
        std::process::exit(1);
    });
    let argv = vec![env!("CARGO_BIN_EXE_deck-fake-agent").to_string()];
    let (client, mut rx) = Client::spawn(&argv, std::env::temp_dir()).unwrap();
    let mut next = || block_on(rx.next()).unwrap();

    assert_eq!(next(), Inbound::Ready { agent: "fake agent".into(), session_id: "fake-1".into() });
    client.prompt("ship it").unwrap();

    let mut text = String::new();
    let request = loop {
        match next() {
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
        match next() {
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
