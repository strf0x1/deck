//! The fake agent's durable session log. Record updates before sending them,
//! so killing a process leaves replayable history and an interrupted turn.

use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::PathBuf;

pub struct Journal {
    root: Option<PathBuf>,
}

impl Journal {
    pub fn new() -> Self {
        let root = std::env::var_os("DECK_FAKE_STATE").filter(|v| !v.is_empty()).map(PathBuf::from);
        Self { root }
    }

    pub fn persistent(&self) -> bool {
        self.root.is_some()
    }

    pub fn create(&self, sid: &str) {
        let Some(path) = self.path(sid) else { return };
        if let Some(root) = &self.root {
            let _ = fs::create_dir_all(root);
        }
        let _ = OpenOptions::new().create_new(true).write(true).open(path);
    }

    pub fn begin(&self, sid: &str, text: &str) {
        self.append(sid, json!({ "event": "begin" }));
        self.update(
            sid,
            &json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": text } }),
        );
    }

    pub fn update(&self, sid: &str, update: &Value) {
        self.append(sid, json!({ "event": "update", "update": update }));
    }

    pub fn end(&self, sid: &str) {
        self.append(sid, json!({ "event": "end" }));
    }

    /// Recover once, then return the full history and the number of turns.
    pub fn load(&self, sid: &str) -> io::Result<(Vec<Value>, u32)> {
        let path = self.path(sid).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let bytes = fs::read(&path)?;
        let complete = bytes.iter().rposition(|&byte| byte == b'\n').map_or(0, |i| i + 1);
        if complete < bytes.len() {
            OpenOptions::new().write(true).open(&path)?.set_len(complete as u64)?;
        }
        let text = String::from_utf8_lossy(&bytes[..complete]);
        let mut updates = Vec::new();
        let (mut pending, mut turns) = (false, 0);
        for line in text.lines() {
            let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
            match entry["event"].as_str() {
                Some("begin") => {
                    pending = true;
                    turns += 1;
                }
                Some("end") => pending = false,
                Some("update") => updates.push(entry["update"].clone()),
                _ => {}
            }
        }
        if pending {
            let mut tools = BTreeSet::new();
            for update in &updates {
                let Some(id) = update["toolCallId"].as_str() else { continue };
                match update["sessionUpdate"].as_str() {
                    Some("tool_call") => {
                        tools.insert(id.to_string());
                    }
                    Some("tool_call_update") if matches!(update["status"].as_str(), Some("completed" | "failed")) => {
                        tools.remove(id);
                    }
                    _ => {}
                }
            }
            for id in tools {
                let failed = json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": "failed" });
                self.update(sid, &failed);
                updates.push(failed);
            }
            let interrupted = json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "\nTurn interrupted when the agent stopped. Nothing was retried." },
            });
            self.update(sid, &interrupted);
            self.end(sid);
            updates.push(interrupted);
        }
        Ok((updates, turns))
    }

    fn path(&self, sid: &str) -> Option<PathBuf> {
        if sid.is_empty() || !sid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return None;
        }
        self.root.as_ref().map(|root| root.join(format!("{sid}.jsonl")))
    }

    fn append(&self, sid: &str, entry: Value) {
        let Some(path) = self.path(sid) else { return };
        if let Ok(mut file) = OpenOptions::new().append(true).open(path) {
            let _ = writeln!(file, "{entry}");
            let _ = file.sync_data();
        }
    }
}
