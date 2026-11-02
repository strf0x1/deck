//! A scripted ACP agent for tests and for driving deck without a model.
//! Each prompt: a thought, a streamed reply, one tool call that asks
//! permission, then a final line saying what it was told.
//!
//! It speaks over stdio by default. `deck-fake-agent --listen <socket>` binds
//! a Unix socket and serves the first connection instead, so a test or a
//! runtime can reach an agent deck did not launch. Either way the loop ends
//! at end of input, which is what closing the stream looks like from here.
//!
//! `DECK_FAKE_DELAY_MS` paces the stream (default 35).

use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::time::Duration;

const SESSION: &str = "fake-1";

struct Agent<W: Write> {
    out: W,
    turn: u32,
}

impl<W: Write> Agent<W> {
    fn send(&mut self, v: Value) {
        let _ = writeln!(self.out, "{v}");
        let _ = self.out.flush();
    }

    fn update(&mut self, u: Value) {
        let params = json!({ "sessionId": SESSION, "update": u });
        self.send(json!({ "jsonrpc": "2.0", "method": "session/update", "params": params }));
    }

    fn chunk(&mut self, kind: &str, text: &str) {
        self.update(json!({ "sessionUpdate": kind, "content": { "type": "text", "text": text } }));
    }

    fn serve<R: BufRead>(mut self, input: R, delay: Duration) {
        let mut lines = input.lines();
        while let Some(Ok(line)) = lines.next() {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            let id = msg["id"].clone();
            match msg["method"].as_str() {
                Some("initialize") => self.send(json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": 1,
                    "agentInfo": { "name": "fake", "title": "fake agent", "version": "0" },
                    "agentCapabilities": { "loadSession": false },
                    "authMethods": [],
                }})),
                Some("session/new") => {
                    self.send(json!({ "jsonrpc": "2.0", "id": id, "result": { "sessionId": SESSION } }))
                }
                Some("session/prompt") => {
                    let asked = msg["params"]["prompt"][0]["text"].as_str().unwrap_or_default().to_string();
                    if self.prompt(id, asked, delay, &mut lines).is_none() {
                        return;
                    }
                }
                _ if !id.is_null() => {
                    self.send(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "unknown" } }))
                }
                _ => {}
            }
        }
    }

    /// One turn. `None` means the input ended while waiting for the answer.
    fn prompt(
        &mut self,
        id: Value,
        asked: String,
        delay: Duration,
        lines: &mut impl Iterator<Item = std::io::Result<String>>,
    ) -> Option<()> {
        self.turn += 1;
        let tool = format!("t{}", self.turn);
        self.chunk("agent_thought_chunk", "planning a small change");
        for word in format!("You said \"{asked}\". Checking the workspace first.").split_inclusive(' ') {
            std::thread::sleep(delay);
            self.chunk("agent_message_chunk", word);
        }
        self.update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": tool,
            "title": "shell git push origin deck/spike",
            "kind": "execute",
            "status": "pending",
        }));
        self.send(json!({ "jsonrpc": "2.0", "id": 900 + self.turn, "method": "session/request_permission", "params": {
            "sessionId": SESSION,
            "toolCall": { "toolCallId": tool, "title": "shell git push origin deck/spike" },
            "options": [
                { "optionId": "allow", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "always", "name": "Always allow", "kind": "allow_always" },
                { "optionId": "reject", "name": "Reject", "kind": "reject_once" },
            ],
        }}));
        // Wait for the answer; anything else in between is ignored.
        let outcome = loop {
            let Ok(line) = lines.next()? else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if v["id"] == json!(900 + self.turn) && v.get("method").is_none() {
                break v["result"]["outcome"]["optionId"].as_str().unwrap_or("cancelled").to_string();
            }
        };
        let ok = outcome == "allow" || outcome == "always";
        let status = if ok { "completed" } else { "failed" };
        self.update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": tool, "status": status }));
        self.chunk("agent_message_chunk", &format!("\npermission: {outcome}"));
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": { "stopReason": "end_turn" } }));
        Some(())
    }
}

fn main() {
    let delay =
        Duration::from_millis(std::env::var("DECK_FAKE_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(35));
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--listen") => listen(args.next(), delay),
        Some(other) => {
            eprintln!("unknown argument {other}; usage: deck-fake-agent [--listen <socket>]");
            std::process::exit(2);
        }
        None => {
            let agent = Agent { out: std::io::stdout().lock(), turn: 0 };
            agent.serve(std::io::stdin().lock(), delay);
        }
    }
}

#[cfg(unix)]
fn listen(path: Option<String>, delay: Duration) {
    use std::os::unix::net::UnixListener;
    let Some(path) = path else {
        eprintln!("--listen needs a socket path");
        std::process::exit(2);
    };
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bind {path}: {e}");
            std::process::exit(1);
        }
    };
    let (stream, _) = match listener.accept() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("accept {path}: {e}");
            std::process::exit(1);
        }
    };
    let input = match stream.try_clone() {
        Ok(s) => std::io::BufReader::new(s),
        Err(e) => {
            eprintln!("clone {path}: {e}");
            std::process::exit(1);
        }
    };
    Agent { out: stream, turn: 0 }.serve(input, delay);
}

#[cfg(not(unix))]
fn listen(_: Option<String>, _: Duration) {
    eprintln!("--listen needs Unix sockets");
    std::process::exit(2);
}
