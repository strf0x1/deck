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
