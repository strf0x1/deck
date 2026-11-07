//! A paced fake turn. The actor advances it between input messages, so a
//! permission wait or stream never prevents initialize, load, close or cancel.

use serde_json::{Value, json};
use std::collections::VecDeque;

pub struct Turn {
    pub prompt: Value,
    pub attachment: u64,
    pub words: VecDeque<String>,
    pub waiting: bool,
    pub question: Option<Value>,
}

impl Turn {
    pub fn new(prompt: Value, attachment: u64, asked: &str) -> Self {
        let said = format!("You said \"{asked}\". Checking the workspace first.");
        Self {
            prompt,
            attachment,
            words: said.split_inclusive(' ').map(str::to_owned).collect(),
            waiting: false,
            question: None,
        }
    }
}

pub fn question(id: Value, session: &str, number: u32) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
        "sessionId": session,
        "toolCall": { "toolCallId": format!("t{number}"), "title": "shell git push origin deck/spike" },
        "options": [
            { "optionId": "allow", "name": "Allow once", "kind": "allow_once" },
            { "optionId": "always", "name": "Always allow", "kind": "allow_always" },
            { "optionId": "reject", "name": "Reject", "kind": "reject_once" },
        ],
    }})
}

/// A read and an edit before the streamed reply, carrying ACP's `locations`
/// and `diff` content the way Construct's file tools do, so the transcript
/// has a file read and a change to draw.
pub fn look_and_edit(number: u32) -> [Value; 4] {
    let (read, edit) = (format!("r{number}"), format!("e{number}"));
    [
        json!({ "sessionUpdate": "tool_call", "toolCallId": read, "title": "read src/lib.rs",
            "kind": "read", "status": "in_progress", "rawInput": { "path": "src/lib.rs" },
            "locations": [{ "path": "src/lib.rs" }] }),
        json!({ "sessionUpdate": "tool_call_update", "toolCallId": read, "status": "completed",
            "content": [{ "type": "content", "content": { "type": "text", "text": "pub fn greet() -> &'static str {\n    \"hi\"\n}\n" } }] }),
        json!({ "sessionUpdate": "tool_call", "toolCallId": edit, "title": "edit src/lib.rs",
            "kind": "edit", "status": "in_progress",
            "rawInput": { "path": "src/lib.rs", "old": "\"hi\"", "new": "\"hello\"" },
            "locations": [{ "path": "src/lib.rs", "line": 2 }] }),
        json!({ "sessionUpdate": "tool_call_update", "toolCallId": edit, "status": "completed",
            "content": [{ "type": "diff", "path": "src/lib.rs",
                "oldText": "pub fn greet() -> &'static str {\n    \"hi\"\n}\n",
                "newText": "pub fn greet() -> &'static str {\n    \"hello\"\n}\n" }] }),
    ]
}
