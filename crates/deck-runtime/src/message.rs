//! A task's last assistant message, as its holder reads it from the ACP
//! stream (ADR 0011 §4): the text a coordinator's task summary carries, at
//! most 2 KiB. Pure: the observer feeds it `session/update`s.
//!
//! The design choice: "last message" is the agent's text since its last
//! other output. A tool call, a plan or a user message closes the message in
//! progress, and the next text chunk starts a new one, so a summary carries
//! what the agent said at the end of its turn, not its narration before a
//! tool. Only the first 2048 bytes are kept, cut at a character boundary,
//! with `truncated` set when anything was dropped.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The most of a message a summary carries (contract 1.4.0 §13.4).
pub const LIMIT: usize = 2048;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastMessage {
    pub text: String,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Default)]
pub struct Reader {
    current: LastMessage,
    /// Other output followed the message; the next chunk starts a new one.
    closed: bool,
}

impl Reader {
    /// One `session/update`'s `update` object.
    pub fn update(&mut self, update: &Value) {
        match update["sessionUpdate"].as_str() {
            Some("agent_message_chunk") => {
                if let Some(text) = update["content"]["text"].as_str() {
                    self.push(text);
                }
            }
            Some("tool_call" | "tool_call_update" | "plan" | "user_message_chunk") => self.closed = true,
            _ => {}
        }
    }

    /// A new turn began: what the agent says next is a new message.
    pub fn reset(&mut self) {
        self.closed = true;
    }

    pub fn message(&self) -> Option<&LastMessage> {
        (!self.current.text.is_empty()).then_some(&self.current)
    }

    fn push(&mut self, text: &str) {
        if self.closed {
            self.current = LastMessage::default();
            self.closed = false;
        }
        let room = LIMIT - self.current.text.len();
        if text.len() <= room {
            self.current.text.push_str(text);
            return;
        }
        let mut end = room;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.current.text.push_str(text.get(..end).unwrap_or_default());
        self.current.truncated = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunk(text: &str) -> Value {
        json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } })
    }

    #[test]
    fn the_last_message_is_the_text_after_the_last_tool_call() {
        let mut reader = Reader::default();
        assert_eq!(reader.message(), None);
        reader.update(&chunk("Reading "));
        reader.update(&chunk("the file."));
        assert_eq!(reader.message().unwrap().text, "Reading the file.");
        reader.update(&json!({ "sessionUpdate": "tool_call", "toolCallId": "1" }));
        assert_eq!(reader.message().unwrap().text, "Reading the file.", "kept until new text arrives");
        reader.update(&json!({ "sessionUpdate": "agent_thought_chunk", "content": { "text": "hm" } }));
        reader.update(&chunk("Done."));
        assert_eq!(reader.message().unwrap().text, "Done.");
        reader.reset();
        reader.update(&chunk("Next turn."));
        assert_eq!(reader.message().unwrap().text, "Next turn.");
    }

    #[test]
    fn a_long_message_is_cut_at_a_character_boundary() {
        let mut reader = Reader::default();
        reader.update(&chunk(&"é".repeat(1500)));
        let message = reader.message().unwrap();
        assert!(message.truncated);
        assert_eq!(message.text.len(), LIMIT);
        reader.update(&chunk("more"));
        assert_eq!(reader.message().unwrap().text.len(), LIMIT);
        let mut odd = Reader::default();
        odd.update(&chunk("a"));
        odd.update(&chunk(&"é".repeat(1500)));
        assert_eq!(odd.message().unwrap().text.len(), LIMIT - 1, "never half a character");
    }
}
