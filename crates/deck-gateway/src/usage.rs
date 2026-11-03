//! Provider token usage from a response as it crosses the gateway.
//!
//! The design choice: the gateway never estimates tokens. It reads the
//! provider's own `usage` object, which for a streamed turn arrives in the
//! last data chunk before `[DONE]`, and treats the latest reported total as
//! authoritative. The bytes still stream unchanged; this only observes them.

use serde_json::Value;

/// A data line in an SSE response is bounded so a provider cannot grow the
/// gateway's buffer without limit.
const MAX_LINE: usize = 64 * 1024;
/// A non-streamed JSON body is bounded for the same reason. A larger body is
/// still forwarded; it just cannot contribute usage.
const MAX_BODY: usize = 8 * 1024 * 1024;

enum Mode {
    Sse,
    Json,
}

pub struct Meter {
    mode: Mode,
    line: Vec<u8>,
    body: Vec<u8>,
    truncated: bool,
    tokens: u64,
}

impl Meter {
    pub fn new(content_type: &str) -> Self {
        let mode = if content_type.trim_start().to_ascii_lowercase().starts_with("text/event-stream") {
            Mode::Sse
        } else {
            Mode::Json
        };
        Self { mode, line: Vec::new(), body: Vec::new(), truncated: false, tokens: 0 }
    }

    /// Observe bytes on their way to the guest.
    pub fn observe(&mut self, bytes: &[u8]) {
        match self.mode {
            Mode::Sse => self.observe_sse(bytes),
            Mode::Json => {
                if self.truncated {
                    return;
                }
                let room = MAX_BODY.saturating_sub(self.body.len());
                if bytes.len() > room {
                    self.body.extend_from_slice(&bytes[..room]);
                    self.truncated = true;
                } else {
                    self.body.extend_from_slice(bytes);
                }
            }
        }
    }

    /// The total tokens reported for this response. A partial line or body is
    /// parsed too, because a provider may close without a trailing newline.
    pub fn finish(&mut self) -> u64 {
        match self.mode {
            Mode::Sse => {
                if !self.line.is_empty() {
                    let line = std::mem::take(&mut self.line);
                    self.read_line(&line);
                }
            }
            Mode::Json if !self.truncated => {
                if let Ok(value) = serde_json::from_slice::<Value>(&self.body)
                    && let Some(tokens) = tokens(&value)
                {
                    self.tokens = tokens;
                }
            }
            Mode::Json => {}
        }
        self.tokens
    }

    fn observe_sse(&mut self, mut bytes: &[u8]) {
        while let Some(index) = bytes.iter().position(|byte| *byte == b'\n') {
            self.line.extend_from_slice(&bytes[..index]);
            let line = std::mem::take(&mut self.line);
            self.read_line(&line);
            bytes = &bytes[index + 1..];
        }
        if self.line.len() + bytes.len() > MAX_LINE {
            // Drop the oversized line; the next newline starts a fresh one.
            self.line.clear();
            return;
        }
        self.line.extend_from_slice(bytes);
    }

    fn read_line(&mut self, bytes: &[u8]) {
        let Ok(text) = std::str::from_utf8(bytes) else { return };
        let Some(data) = text.trim_end_matches('\r').strip_prefix("data:") else { return };
        let data = data.trim_start();
        if data == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else { return };
        if let Some(tokens) = tokens(&value) {
            self.tokens = tokens;
        }
    }
}

/// `total_tokens` when present, else the prompt/completion pair. OpenAI and
/// compatible providers use these names; a response with no usable count
/// contributes nothing.
fn tokens(value: &Value) -> Option<u64> {
    let usage = value.get("usage")?;
    if let Some(total) = usage.get("total_tokens").and_then(Value::as_u64) {
        return Some(total);
    }
    let prompt = usage.get("prompt_tokens").and_then(Value::as_u64);
    let completion = usage.get("completion_tokens").and_then(Value::as_u64);
    match (prompt, completion) {
        (Some(prompt), Some(completion)) => Some(prompt.saturating_add(completion)),
        (Some(prompt), None) => Some(prompt),
        (None, Some(completion)) => Some(completion),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_json_response_reports_total_tokens() {
        let mut meter = Meter::new("application/json");
        meter.observe(br#"{"id":"x","usage":{"#);
        meter.observe(br#""prompt_tokens":3,"completion_tokens":4,"total_tokens":7}}"#);
        assert_eq!(meter.finish(), 7);
    }

    #[test]
    fn prompt_and_completion_are_used_without_a_total() {
        let mut meter = Meter::new("application/json");
        meter.observe(br#"{"usage":{"prompt_tokens":5,"completion_tokens":8}}"#);
        assert_eq!(meter.finish(), 13);
    }

    #[test]
    fn the_final_sse_chunk_is_counted_even_after_done() {
        let mut meter = Meter::new("text/event-stream; charset=utf-8");
        meter.observe(b"data: {\"choices\":[{\"delta\":{\"content\":\"o\"}}]}\n\n");
        meter.observe(b"data: {\"usage\":{\"total_tokens\":9}}\n\n");
        meter.observe(b"data: [DONE]\n\n");
        assert_eq!(meter.finish(), 9);
    }

    #[test]
    fn sse_lines_split_across_reads_still_parse() {
        let mut meter = Meter::new("text/event-stream");
        meter.observe(b"data: {\"usa");
        meter.observe(b"ge\":{\"total_tokens\":11}}\n");
        assert_eq!(meter.finish(), 11);
    }

    #[test]
    fn the_latest_usage_wins_and_absent_usage_is_zero() {
        let mut meter = Meter::new("text/event-stream");
        meter.observe(b"data: {\"usage\":{\"total_tokens\":2}}\ndata: {\"usage\":{\"total_tokens\":6}}\n");
        assert_eq!(meter.finish(), 6);
        let mut none = Meter::new("application/json");
        none.observe(br#"{"id":"x"}"#);
        assert_eq!(none.finish(), 0);
    }

    #[test]
    fn an_oversized_json_body_is_forwarded_but_not_counted() {
        let mut meter = Meter::new("application/json");
        meter.observe(&vec![b'x'; MAX_BODY + 1]);
        assert_eq!(meter.finish(), 0);
    }
}
