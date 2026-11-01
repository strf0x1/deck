//! A scripted ACP agent for tests and for driving deck without a model.
//! Each prompt: a thought, a streamed reply, one tool call that asks
//! permission, then a final line saying what it was told.
//!
//! `DECK_FAKE_DELAY_MS` paces the stream (default 35).

use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::time::Duration;

fn send(v: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

fn update(sid: &str, u: Value) {
    send(json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": sid, "update": u } }));
}

fn chunk(sid: &str, kind: &str, text: &str) {
    update(sid, json!({ "sessionUpdate": kind, "content": { "type": "text", "text": text } }));
}

fn main() {
    let delay =
        Duration::from_millis(std::env::var("DECK_FAKE_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(35));
    let mut lines = std::io::stdin().lock().lines();
    let mut turn = 0u32;
    while let Some(Ok(line)) = lines.next() {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let id = msg["id"].clone();
        match msg["method"].as_str() {
            Some("initialize") => send(json!({ "jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": 1,
                "agentInfo": { "name": "fake", "title": "fake agent", "version": "0" },
                "agentCapabilities": { "loadSession": false },
                "authMethods": [],
            }})),
            Some("session/new") => send(json!({ "jsonrpc": "2.0", "id": id, "result": { "sessionId": "fake-1" } })),
            Some("session/prompt") => {
                turn += 1;
                let sid = "fake-1";
                let asked = msg["params"]["prompt"][0]["text"].as_str().unwrap_or_default().to_string();
                chunk(sid, "agent_thought_chunk", "planning a small change");
                for word in format!("You said \"{asked}\". Checking the workspace first.").split_inclusive(' ') {
                    std::thread::sleep(delay);
                    chunk(sid, "agent_message_chunk", word);
                }
                let tool = format!("t{turn}");
                update(
                    sid,
                    json!({ "sessionUpdate": "tool_call", "toolCallId": tool, "title": "shell git push origin deck/spike", "kind": "execute", "status": "pending" }),
                );
                send(json!({ "jsonrpc": "2.0", "id": 900 + turn, "method": "session/request_permission", "params": {
                    "sessionId": sid,
                    "toolCall": { "toolCallId": tool, "title": "shell git push origin deck/spike" },
                    "options": [
                        { "optionId": "allow", "name": "Allow once", "kind": "allow_once" },
                        { "optionId": "always", "name": "Always allow", "kind": "allow_always" },
                        { "optionId": "reject", "name": "Reject", "kind": "reject_once" },
                    ],
                }}));
                // Wait for the answer; anything else in between is ignored.
                let outcome = loop {
                    let Some(Ok(l)) = lines.next() else { return };
                    let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
                    if v["id"] == json!(900 + turn) && v.get("method").is_none() {
                        break v["result"]["outcome"]["optionId"].as_str().unwrap_or("cancelled").to_string();
                    }
                };
                let ok = outcome == "allow" || outcome == "always";
                update(
                    sid,
                    json!({ "sessionUpdate": "tool_call_update", "toolCallId": tool, "status": if ok { "completed" } else { "failed" } }),
                );
                chunk(sid, "agent_message_chunk", &format!("\npermission: {outcome}"));
                send(json!({ "jsonrpc": "2.0", "id": id, "result": { "stopReason": "end_turn" } }));
            }
            _ if !id.is_null() => {
                send(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "unknown" } }))
            }
            _ => {}
        }
    }
}
