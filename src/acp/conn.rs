//! The client side of one ACP connection as a pure state machine: feed it
//! each line the agent writes, get back the lines to write and the
//! [`Inbound`] events the UI should see.
//!
//! The design choice: one agent process, one session. `initialize` is sent
//! at start, `session/new` as soon as it is answered, and prompts queue
//! nowhere: the UI only offers a prompt while no turn is running.

use super::VERSION;
use super::rpc::{self, Incoming};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;

/// What the UI learns from the agent.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// The handshake finished; prompts can be sent.
    Ready { agent: String, session_id: String },
    /// Streamed reply text.
    Message(String),
    /// Streamed reasoning text.
    Thought(String),
    /// A tool call started.
    Tool { id: String, title: String },
    /// A tool call changed status.
    ToolStatus { id: String, status: ToolStatus },
    /// The agent asks before running a tool. Answer with
    /// [`Conn::answer`] using the same `request`.
    Permission { request: Value, title: String, options: Vec<PermissionOption> },
    /// The prompt's turn finished, with ACP's stop reason.
    TurnEnded(String),
    /// Something failed; the text is for the user.
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Done,
    Failed,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Initialize,
    NewSession,
    Prompt,
}

/// Lines to write and events to surface after one step.
#[derive(Debug, Default, PartialEq)]
pub struct Step {
    pub lines: Vec<String>,
    pub events: Vec<Inbound>,
}

pub struct Conn {
    cwd: PathBuf,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    agent: String,
    session: Option<String>,
}

impl Conn {
    /// A connection and the `initialize` line that opens it.
    pub fn start(cwd: PathBuf) -> (Self, String) {
        let mut c = Conn { cwd, next_id: 0, pending: HashMap::new(), agent: String::new(), session: None };
        let caps = json!({ "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false });
        let line = c.request(
            Pending::Initialize,
            "initialize",
            json!({
                "protocolVersion": VERSION,
                "clientCapabilities": caps,
                "clientInfo": { "name": "deck", "version": env!("CARGO_PKG_VERSION") },
            }),
        );
        (c, line)
    }

    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    fn request(&mut self, kind: Pending, method: &str, params: Value) -> String {
        self.next_id += 1;
        self.pending.insert(self.next_id, kind);
        rpc::request(self.next_id, method, params)
    }

    /// The `session/prompt` line for `text`, or `None` before the session exists.
    pub fn prompt(&mut self, text: &str) -> Option<String> {
        let sid = self.session.clone()?;
        let params = json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": text }] });
        Some(self.request(Pending::Prompt, "session/prompt", params))
    }

    /// The `session/cancel` notification, or `None` before the session exists.
    pub fn cancel(&self) -> Option<String> {
        Some(rpc::notification("session/cancel", json!({ "sessionId": self.session.as_deref()? })))
    }

    /// The reply to a permission request: the chosen option, or cancelled.
    pub fn answer(request: &Value, option: Option<&str>) -> String {
        let outcome = match option {
            Some(id) => json!({ "outcome": "selected", "optionId": id }),
            None => json!({ "outcome": "cancelled" }),
        };
        rpc::ok(request, json!({ "outcome": outcome }))
    }

    /// One line from the agent.
    pub fn read(&mut self, line: &str) -> Step {
        match rpc::parse(line) {
            Some(Incoming::Response { id, result }) => self.response(&id, result),
            Some(Incoming::Notification { method, params }) if method == "session/update" => {
                Step { lines: vec![], events: update(&params["update"]).into_iter().collect() }
            }
            Some(Incoming::Request { id, method, params }) if method == "session/request_permission" => {
                Step { lines: vec![], events: vec![permission(id, &params)] }
            }
            // fs/* and terminal/* are not advertised, so an agent shouldn't ask.
            Some(Incoming::Request { id, method, .. }) => Step {
                lines: vec![rpc::err(&id, rpc::METHOD_NOT_FOUND, &format!("deck does not offer {method}"))],
                events: vec![],
            },
            _ => Step::default(),
        }
    }

    fn response(&mut self, id: &Value, result: Result<Value, String>) -> Step {
        let Some(kind) = id.as_u64().and_then(|n| self.pending.remove(&n)) else { return Step::default() };
        let value = match result {
            Ok(v) => v,
            Err(e) => return Step { lines: vec![], events: vec![Inbound::Failed(format!("{}: {e}", name(kind)))] },
        };
        match kind {
            Pending::Initialize => {
                let info = &value["agentInfo"];
                self.agent = info["title"].as_str().or(info["name"].as_str()).unwrap_or("agent").to_string();
                let params = json!({ "cwd": self.cwd.display().to_string(), "mcpServers": [] });
                Step { lines: vec![self.request(Pending::NewSession, "session/new", params)], events: vec![] }
            }
            Pending::NewSession => match value["sessionId"].as_str() {
                Some(sid) => {
                    self.session = Some(sid.to_string());
                    let ready = Inbound::Ready { agent: self.agent.clone(), session_id: sid.to_string() };
                    Step { lines: vec![], events: vec![ready] }
                }
                None => Step { lines: vec![], events: vec![Inbound::Failed("session/new named no session".into())] },
            },
            Pending::Prompt => {
                let stop = value["stopReason"].as_str().unwrap_or("end_turn").to_string();
                Step { lines: vec![], events: vec![Inbound::TurnEnded(stop)] }
            }
        }
    }
}

fn name(kind: Pending) -> &'static str {
    match kind {
        Pending::Initialize => "initialize",
        Pending::NewSession => "session/new",
        Pending::Prompt => "session/prompt",
    }
}

/// A `session/update` payload as an event, if deck shows it.
fn update(u: &Value) -> Option<Inbound> {
    let text = || u["content"]["text"].as_str().map(str::to_string);
    let id = || u["toolCallId"].as_str().unwrap_or_default().to_string();
    match u["sessionUpdate"].as_str()? {
        "agent_message_chunk" => Some(Inbound::Message(text()?)),
        "agent_thought_chunk" => Some(Inbound::Thought(text()?)),
        "tool_call" => Some(Inbound::Tool { id: id(), title: u["title"].as_str().unwrap_or("tool").to_string() }),
        "tool_call_update" => {
            let status = match u["status"].as_str()? {
                "completed" => ToolStatus::Done,
                "failed" => ToolStatus::Failed,
                _ => ToolStatus::Running,
            };
            Some(Inbound::ToolStatus { id: id(), status })
        }
        _ => None,
    }
}

fn permission(request: Value, params: &Value) -> Inbound {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn handshake() -> Conn {
        let (mut c, init) = Conn::start(PathBuf::from("/w"));
        assert!(init.contains(r#""method":"initialize""#));
        let s = c.read(r#"{"jsonrpc":"2.0","id":1,"result":{"agentInfo":{"name":"construct"}}}"#);
        assert_eq!(s.lines.len(), 1);
        assert!(s.lines[0].contains(r#""method":"session/new""#) && s.lines[0].contains(r#""cwd":"/w""#));
        let s = c.read(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#);
        assert_eq!(s.events, vec![Inbound::Ready { agent: "construct".into(), session_id: "s1".into() }]);
        c
    }

    #[test]
    fn no_prompt_before_the_session_exists() {
        let (mut c, _) = Conn::start(PathBuf::from("/w"));
        assert_eq!(c.prompt("hi"), None);
        assert_eq!(c.cancel(), None);
    }

    #[test]
    fn a_turn_streams_then_ends() {
        let mut c = handshake();
        let p = c.prompt("hi").unwrap_or_default();
        assert!(p.contains(r#""sessionId":"s1""#) && p.contains(r#""text":"hi""#));
        let chunk = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"yo"}}}}"#;
        assert_eq!(c.read(chunk).events, vec![Inbound::Message("yo".into())]);
        let tool = r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call","toolCallId":"t","title":"read x","status":"in_progress"}}}"#;
        assert_eq!(c.read(tool).events, vec![Inbound::Tool { id: "t".into(), title: "read x".into() }]);
        let end = r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#;
        assert_eq!(c.read(end).events, vec![Inbound::TurnEnded("end_turn".into())]);
    }

    #[test]
    fn permission_requests_round_trip_their_id() {
        let mut c = handshake();
        let req = r#"{"jsonrpc":"2.0","id":"p9","method":"session/request_permission","params":{"toolCall":{"title":"shell rm"},"options":[{"optionId":"y","name":"Allow","kind":"allow_once"},{"optionId":"n","name":"Reject","kind":"reject_once"}]}}"#;
        let events = c.read(req).events;
        let [Inbound::Permission { request, title, options }] = events.as_slice() else {
            panic!("expected one permission event");
        };
        assert_eq!(title, "shell rm");
        assert!(options[0].allows() && !options[1].allows());
        let reply = Conn::answer(request, Some("y"));
        assert_eq!(
            rpc::parse(&reply),
            Some(Incoming::Response {
                id: json!("p9"),
                result: Ok(json!({"outcome":{"outcome":"selected","optionId":"y"}}))
            })
        );
    }

    #[test]
    fn unoffered_client_methods_are_refused() {
        let mut c = handshake();
        let s = c.read(r#"{"jsonrpc":"2.0","id":5,"method":"fs/read_text_file","params":{}}"#);
        assert!(s.lines[0].contains("-32601"));
    }

    #[test]
    fn a_failed_handshake_is_shown() {
        let (mut c, _) = Conn::start(PathBuf::from("/w"));
        let s = c.read(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"boom"}}"#);
        assert_eq!(s.events, vec![Inbound::Failed("initialize: boom (-32603)".into())]);
    }
}
