//! Idempotent prompts as Construct implements them (contract 1.3.0 §12,
//! construct ADR 0045): a prompt resent with a key already on record starts no
//! turn. It waits for the running turn, gets the ended turn's stop reason, or
//! `cancelled` for a turn a kill cut off. The same key with other text is
//! refused. Keys are journaled with the prompt, so they survive a restart.

use deck_acp::names::{CONSTRUCT_META, IDEMPOTENCY_CONFLICT, IDEMPOTENCY_KEY};
use serde_json::{Value, json};
use std::collections::HashMap;

/// What to do with a prompt that may carry a recorded key.
pub enum Seen {
    New,
    Waits,
    Answer(&'static str),
    Stop(String),
    Refuse(Value),
}

#[derive(Default)]
pub struct Keys {
    /// Every journaled key: its prompt text and, once the turn ended, its stop reason.
    recorded: HashMap<String, (String, Option<String>)>,
    /// The running turn's key.
    running: Option<String>,
}

/// The key a prompt carries: absent or `null` is none; anything that is not a
/// string of 1 to 255 characters is refused, never treated as absent.
pub fn parse(params: &Value) -> Result<Option<String>, Value> {
    match &params["_meta"][CONSTRUCT_META][IDEMPOTENCY_KEY] {
        Value::Null => Ok(None),
        Value::String(k) if (1..=255).contains(&k.chars().count()) => Ok(Some(k.clone())),
        _ => Err(
            json!({ "code": -32602, "message": format!("{IDEMPOTENCY_KEY} must be a string of 1 to 255 characters") }),
        ),
    }
}

impl Keys {
    /// Keys read back from the journal; a turn without an end was cut off.
    pub fn from_journal(entries: Vec<(String, String, Option<String>)>) -> Self {
        let recorded = entries.into_iter().map(|(k, text, stop)| (k, (text, stop))).collect();
        Self { recorded, running: None }
    }

    pub fn begin(&mut self, key: Option<String>, text: &str) {
        if let Some(k) = &key {
            self.recorded.insert(k.clone(), (text.to_string(), None));
        }
        self.running = key;
    }

    pub fn end(&mut self, reason: &str) {
        if let Some(k) = self.running.take()
            && let Some((_, stop)) = self.recorded.get_mut(&k)
        {
            *stop = Some(reason.to_string());
        }
    }

    pub fn seen(&self, key: &str, text: &str) -> Seen {
        match self.recorded.get(key) {
            None => Seen::New,
            Some((t, _)) if t != text => Seen::Refuse(json!({
                "code": -32602,
                "message": format!("{IDEMPOTENCY_KEY} {key:?} already names a different prompt"),
                "data": { "reason": IDEMPOTENCY_CONFLICT },
            })),
            Some(_) if self.running.as_deref() == Some(key) => Seen::Waits,
            Some((_, Some(stop))) => Seen::Stop(stop.clone()),
            Some((_, None)) => Seen::Answer("cancelled"),
        }
    }
}
