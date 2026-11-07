//! Typed events and ACP payload decoding at the protocol boundary. Views
//! receive domain state rather than reading Construct's metadata themselves.

use crate::tool::{self, ToolCall, ToolUpdate};
use serde_json::Value;

/// What the UI learns from the agent.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// The handshake finished, with the loaded session's authoritative turn state.
    Ready { agent: String, session_id: String, turn: Option<TurnState> },
    /// A running agent cannot rebuild history on a live attachment.
    Detached,
    /// The agent's tools speak Deck's GitHub operation API (contract 1.2.0
    /// §11.1). Sent once after `initialize`, only when advertised, so its
    /// absence by `Ready` means the agent cannot publish from a sandbox.
    Publishes,
    /// Authoritative state, independent of a prompt request on this connection.
    Turn { number: u64, state: TurnState },
    /// A user prompt replayed while loading history.
    UserMessage(String),
    /// Streamed reply text.
    Message(String),
    /// Streamed reasoning text.
    Thought(String),
    /// A tool call and its current state, including a final state in replay.
    Tool(ToolCall),
    /// Some of a tool call's fields changed; apply with [`ToolCall::apply`].
    ToolUpdate(ToolUpdate),
    /// The agent asks before running a tool. Answer with
    /// [`crate::conn::Conn::answer`] using the same `request`.
    Permission { request: Value, title: String, options: Vec<PermissionOption> },
    /// The prompt's turn finished, with ACP's stop reason.
    TurnEnded(String),
    /// Something failed; the text is for the user.
    Failed(String),
}

/// Construct's turn state, restored from a load snapshot and live notices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnState {
    Working,
    Waiting,
    Idle { stop_reason: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    /// ACP's `kind`: `allow_once`, `allow_always`, `reject_once`, `reject_always`.
    pub kind: String,
}

impl PermissionOption {
    pub fn allows(&self) -> bool {
        self.kind.starts_with("allow")
    }
}

pub(crate) fn parse_turn(value: &Value) -> Option<(u64, TurnState)> {
    let number = value["turn"].as_u64()?;
    let state = match value["state"].as_str()? {
        "working" => TurnState::Working,
        "waiting" => TurnState::Waiting,
        "idle" => TurnState::Idle { stop_reason: value["stopReason"].as_str().map(str::to_owned) },
        _ => return None,
    };
    Some((number, state))
}
/// A `session/update` payload as an event, if deck shows it.
pub(crate) fn update(u: &Value) -> Option<Inbound> {
    let text = || u["content"]["text"].as_str().map(str::to_string);
    match u["sessionUpdate"].as_str()? {
        "user_message_chunk" => Some(Inbound::UserMessage(text()?)),
        "agent_message_chunk" => Some(Inbound::Message(text()?)),
        "agent_thought_chunk" => Some(Inbound::Thought(text()?)),
        "tool_call" => Some(Inbound::Tool(tool::call(u))),
        "tool_call_update" => Some(Inbound::ToolUpdate(tool::update(u))),
        _ => None,
    }
}

pub(crate) fn permission(request: Value, params: &Value) -> Inbound {
    let title = params["toolCall"]["title"].as_str().unwrap_or("a tool").to_string();
    let options = params["options"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|o| {
                    Some(PermissionOption {
                        id: o["optionId"].as_str()?.to_string(),
                        name: o["name"].as_str().unwrap_or_default().to_string(),
                        kind: o["kind"].as_str().unwrap_or_default().to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Inbound::Permission { request, title, options }
}
