//! The client side of one ACP connection as a pure state machine: feed it
//! each line the agent writes, get back the lines to write and the
//! [`Inbound`] events the UI should see.
//!
//! The design choice: one agent process, one session. `initialize` is sent
//! at start, then a session is created or loaded, and prompts queue
//! nowhere: the UI only offers a prompt while no turn is running.

use super::VERSION;
use super::inbound::{Inbound, TurnState, parse_turn, permission, update};
use super::rpc::{self, Incoming};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

pub(crate) enum Target {
    New,
    Load(String),
    Attach(String),
}

impl Target {
    fn session(&self) -> Option<&str> {
        match self {
            Self::New => None,
            Self::Load(id) | Self::Attach(id) => Some(id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    Initialize,
    NewSession,
    LoadSession(String),
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
    pending: HashMap<Value, Pending>,
    prefix: Option<String>,
    initialized: bool,
    live: bool,
    questions: HashSet<Value>,
    turn: Option<(u64, TurnState)>,
    agent: String,
    session: Option<String>,
    target: Target,
}

impl Conn {
    /// A connection and the `initialize` line that opens it.
    pub fn start(cwd: PathBuf) -> (Self, String) {
        Self::begin(cwd, Target::New, None)
    }

    /// Initialize, then rebuild the recorded session through `session/load`.
    pub fn load(cwd: PathBuf, session_id: String) -> (Self, String) {
        Self::begin(cwd, Target::Load(session_id), None)
    }

    /// Rebuild a session in a running agent, requiring its live-attach contract.
    pub fn attach(cwd: PathBuf, session_id: String) -> (Self, String) {
        Self::begin(cwd, Target::Attach(session_id), None)
    }

    pub(crate) fn begin(cwd: PathBuf, target: Target, prefix: Option<String>) -> (Self, String) {
        let mut c = Conn {
            cwd,
            next_id: 0,
            pending: HashMap::new(),
            agent: String::new(),
            session: None,
            target,
            prefix,
            initialized: false,
            live: false,
            questions: HashSet::new(),
            turn: None,
        };
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
        let id = match &self.prefix {
            Some(prefix) => json!(format!("{prefix}-{}", self.next_id)),
            None => json!(self.next_id),
        };
        self.pending.insert(id.clone(), kind);
        rpc::request(id, method, params)
    }

    /// The `session/prompt` line for `text`, or `None` before the session exists.
    pub fn prompt(&mut self, text: &str) -> Option<String> {
        let sid = self.session.clone()?;
        if self.turn.as_ref().is_some_and(|(_, state)| !matches!(state, TurnState::Idle { .. })) {
            return None;
        }
        let params = json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": text }] });
        Some(self.request(Pending::Prompt, "session/prompt", params))
    }

    /// The `session/cancel` notification, or `None` before the session exists.
    pub fn cancel(&self) -> Option<String> {
        Some(rpc::notification("session/cancel", json!({ "sessionId": self.session.as_deref()? })))
    }

    /// The reply to a permission request: the chosen option, or cancelled.
    pub fn answer(&mut self, request: &Value, option: Option<&str>) -> Option<String> {
        if !self.questions.remove(request) {
            return None;
        }
        let outcome = match option {
            Some(id) => json!({ "outcome": "selected", "optionId": id }),
            None => json!({ "outcome": "cancelled" }),
        };
        Some(rpc::ok(request, json!({ "outcome": outcome })))
    }

    /// One line from the agent.
    pub fn read(&mut self, line: &str) -> Step {
        match rpc::parse(line) {
            Some(Incoming::Response { id, result }) => self.response(&id, result),
            _ if !self.initialized => Step::default(),
            Some(Incoming::Notification { method, params }) if method == "_construct/turn" => {
                if self.session.is_none() || self.session.as_deref() != params["sessionId"].as_str() {
                    return Step::default();
                }
                Step { lines: vec![], events: self.observe_turn(&params).into_iter().collect() }
            }
            Some(Incoming::Notification { method, params }) if method == "session/update" && self.accepts(&params) => {
                Step { lines: vec![], events: update(&params["update"]).into_iter().collect() }
            }
            Some(Incoming::Request { id, method, params })
                if method == "session/request_permission" && self.accepts(&params) =>
            {
                self.questions.insert(id.clone());
                Step { lines: vec![], events: vec![permission(id, &params)] }
            }
            Some(Incoming::Request { method, .. }) if method == "session/request_permission" => Step::default(),
            // fs/* and terminal/* are not advertised, so an agent shouldn't ask.
            Some(Incoming::Request { id, method, .. }) => Step {
                lines: vec![rpc::err(&id, rpc::METHOD_NOT_FOUND, &format!("deck does not offer {method}"))],
                events: vec![],
            },
            _ => Step::default(),
        }
    }

    fn response(&mut self, id: &Value, result: Result<Value, String>) -> Step {
        let Some(kind) = self.pending.remove(id) else { return Step::default() };
        let value = match result {
            Ok(v) => v,
            Err(e) => return Step { lines: vec![], events: vec![Inbound::Failed(format!("{}: {e}", name(&kind)))] },
        };
        match kind {
            Pending::Initialize => {
                self.initialized = true;
                self.live = value["agentCapabilities"]["_meta"]["construct"]["liveAttach"] == true;
                if matches!(self.target, Target::Attach(_)) && !self.live {
                    return Step { lines: vec![], events: vec![Inbound::Detached] };
                }
                let info = &value["agentInfo"];
                self.agent = info["title"].as_str().or(info["name"].as_str()).unwrap_or("agent").to_string();
                let mut params = json!({ "cwd": self.cwd.display().to_string(), "mcpServers": [] });
                let (kind, method) = if let Some(sid) = self.target.session().map(str::to_owned) {
                    if value["agentCapabilities"]["loadSession"] != true {
                        return Step {
                            lines: vec![],
                            events: vec![Inbound::Failed("the agent does not support session/load".into())],
                        };
                    }
                    params["sessionId"] = json!(&sid);
                    (Pending::LoadSession(sid), "session/load")
                } else {
                    (Pending::NewSession, "session/new")
                };
                Step { lines: vec![self.request(kind, method, params)], events: vec![] }
            }
            Pending::NewSession => match value["sessionId"].as_str() {
                Some(sid) => {
                    self.session = Some(sid.to_string());
                    let ready = Inbound::Ready { agent: self.agent.clone(), session_id: sid.to_string(), turn: None };
                    Step { lines: vec![], events: vec![ready] }
                }
                None => Step { lines: vec![], events: vec![Inbound::Failed("session/new named no session".into())] },
            },
            Pending::LoadSession(sid) => {
                self.session = Some(sid.clone());
                let snapshot = &value["_meta"]["construct"]["turn"];
                if matches!(self.target, Target::Attach(_))
                    && (snapshot["sessionId"] != sid || parse_turn(snapshot).is_none())
                {
                    self.session = None;
                    return Step {
                        lines: vec![],
                        events: vec![Inbound::Failed("live attach returned no valid turn state".into())],
                    };
                }
                let turn = self.observe_turn(snapshot).and_then(|ev| match ev {
                    Inbound::Turn { state, .. } => Some(state),
                    _ => None,
                });
                Step {
                    lines: vec![],
                    events: vec![Inbound::Ready { agent: self.agent.clone(), session_id: sid, turn }],
                }
            }
            Pending::Prompt => {
                if self.live {
                    return Step::default();
                }
                let stop = value["stopReason"].as_str().unwrap_or("end_turn").to_string();
                Step { lines: vec![], events: vec![Inbound::TurnEnded(stop)] }
            }
        }
    }
    fn accepts(&self, params: &Value) -> bool {
        let selected = self.session.as_deref().or(self.target.session());
        // Older agents omit sessionId. A named foreign session never belongs here.
        selected.is_some()
            && match params["sessionId"].as_str() {
                Some(id) => Some(id) == selected,
                None => !self.live,
            }
    }

    fn observe_turn(&mut self, value: &Value) -> Option<Inbound> {
        let (number, state) = parse_turn(value)?;
        if self.turn.as_ref().is_some_and(|(old, old_state)| *old > number || (*old == number && *old_state == state)) {
            return None;
        }
        if !matches!(state, TurnState::Waiting) {
            self.questions.clear();
        }
        self.turn = Some((number, state.clone()));
        Some(Inbound::Turn { number, state })
    }
}

fn name(kind: &Pending) -> &'static str {
    match kind {
        Pending::Initialize => "initialize",
        Pending::NewSession => "session/new",
        Pending::LoadSession(_) => "session/load",
        Pending::Prompt => "session/prompt",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolStatus;

    fn handshake() -> Conn {
        let (mut c, init) = Conn::start(PathBuf::from("/w"));
        assert!(init.contains(r#""method":"initialize""#));
        let s = c.read(r#"{"jsonrpc":"2.0","id":1,"result":{"agentInfo":{"name":"construct"}}}"#);
        assert_eq!(s.lines.len(), 1);
        assert!(s.lines[0].contains(r#""method":"session/new""#) && s.lines[0].contains(r#""cwd":"/w""#));
        let s = c.read(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#);
        assert_eq!(s.events, vec![Inbound::Ready { agent: "construct".into(), session_id: "s1".into(), turn: None }]);
        c
    }

    #[test]
    fn load_replays_before_ready_and_keeps_the_requested_session_id() {
        let (mut c, init) = Conn::load(PathBuf::from("/w"), "saved".into());
        assert_eq!(
            rpc::parse(&init),
            Some(Incoming::Request {
                id: json!(1),
                method: "initialize".into(),
                params: json!({
                    "protocolVersion": VERSION,
                    "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false },
                    "clientInfo": { "name": "deck", "version": env!("CARGO_PKG_VERSION") },
                }),
            })
        );
        assert_eq!(c.prompt("too soon"), None);
        let step =
            c.read(r#"{"id":1,"result":{"agentInfo":{"name":"fake"},"agentCapabilities":{"loadSession":true}}}"#);
        assert_eq!(step.events, vec![]);
        assert_eq!(
            rpc::parse(&step.lines[0]),
            Some(Incoming::Request {
                id: json!(2),
                method: "session/load".into(),
                params: json!({ "cwd": "/w", "mcpServers": [], "sessionId": "saved" }),
            })
        );
        let replay = r#"{"method":"session/update","params":{"update":{"sessionUpdate":"user_message_chunk","content":{"text":"go"}}}}"#;
        assert_eq!(c.read(replay).events, vec![Inbound::UserMessage("go".into())]);
        assert_eq!(c.prompt("still too soon"), None);
        assert_eq!(
            c.read(r#"{"id":2,"result":{}}"#).events,
            vec![Inbound::Ready { agent: "fake".into(), session_id: "saved".into(), turn: None }]
        );
        let prompt = c.prompt("continue").unwrap();
        assert_eq!(
            rpc::parse(&prompt),
            Some(Incoming::Request {
                id: json!(3),
                method: "session/prompt".into(),
                params: json!({ "sessionId": "saved", "prompt": [{ "type": "text", "text": "continue" }] }),
            })
        );
    }

    #[test]
    fn loading_requires_agent_support_and_reports_load_errors() {
        let (mut unsupported, _) = Conn::load(PathBuf::from("/w"), "saved".into());
        let step = unsupported.read(r#"{"id":1,"result":{"agentCapabilities":{"loadSession":false}}}"#);
        assert_eq!(step.lines, Vec::<String>::new());
        assert_eq!(step.events, vec![Inbound::Failed("the agent does not support session/load".into())]);
        assert_eq!(unsupported.prompt("no"), None);

        let (mut missing, _) = Conn::load(PathBuf::from("/w"), "missing".into());
        missing.read(r#"{"id":1,"result":{"agentCapabilities":{"loadSession":true}}}"#);
        let step = missing.read(r#"{"id":2,"error":{"code":-32002,"message":"no session missing"}}"#);
        assert_eq!(step.events, vec![Inbound::Failed("session/load: no session missing (-32002)".into())]);
        assert_eq!(missing.prompt("no"), None);
    }

    fn live_init(c: &mut Conn) -> Step {
        c.read(&rpc::ok(
            &json!(1),
            json!({ "agentInfo": { "name": "construct" }, "agentCapabilities": {
                "loadSession": true, "_meta": { "construct": { "liveAttach": true } }
            }}),
        ))
    }

    fn turn_line(sid: &str, number: u64, state: &str) -> String {
        rpc::notification(
            "_construct/turn",
            json!({ "sessionId": sid, "turn": number, "state": state, "stopReason": "cancelled" }),
        )
    }

    #[test]
    fn live_attach_requires_advertisement_without_loading_or_starting_another_session() {
        let (mut c, _) = Conn::attach("/w".into(), "saved".into());
        let step = c.read(&rpc::ok(&json!(1), json!({ "agentCapabilities": { "loadSession": true } })));
        assert_eq!(step, Step { lines: vec![], events: vec![Inbound::Detached] });
        assert_eq!(c.prompt("no"), None);
    }

    #[test]
    fn live_replay_restores_each_phase_atomically_and_ignores_old_and_foreign_traffic() {
        for state in [TurnState::Working, TurnState::Waiting, TurnState::Idle { stop_reason: Some("cancelled".into()) }]
        {
            let (mut c, _) = Conn::attach("/w".into(), "saved".into());
            let update = rpc::notification(
                "session/update",
                json!({ "sessionId": "saved", "update": {
                    "sessionUpdate": "agent_message_chunk", "content": { "text": "partial" }
                }}),
            );
            assert_eq!(c.read(&update), Step::default());
            assert_eq!(c.read(&turn_line("saved", 1, "idle")), Step::default());
            assert_eq!(live_init(&mut c).lines.len(), 1);
            assert_eq!(c.read(&turn_line("saved", 1, "idle")), Step::default(), "initialize notices precede replay");
            assert_eq!(c.read(&update).events, vec![Inbound::Message("partial".into())]);
            assert_eq!(c.prompt("too soon"), None);
            let phase = match state {
                TurnState::Working => "working",
                TurnState::Waiting => "waiting",
                TurnState::Idle { .. } => "idle",
            };
            let snapshot = json!({ "sessionId": "saved", "turn": 1, "state": phase, "stopReason": "cancelled" });
            let ready = c.read(&rpc::ok(&json!(2), json!({ "_meta": { "construct": { "turn": snapshot } } })));
            assert_eq!(
                ready.events,
                vec![Inbound::Ready {
                    agent: "construct".into(),
                    session_id: "saved".into(),
                    turn: Some(state.clone())
                }]
            );
            assert_eq!(c.read(&turn_line("other", 3, "working")), Step::default());
            assert_eq!(c.read(&turn_line("saved", 1, phase)), Step::default(), "snapshot is not emitted twice");
            assert_eq!(
                c.read(&turn_line("saved", 0, "working")),
                Step::default(),
                "an older turn cannot change status"
            );
            assert_eq!(c.prompt("next").is_some(), matches!(state, TurnState::Idle { .. }));
        }
    }

    #[test]
    fn live_attach_rejects_missing_or_foreign_turn_snapshot() {
        for snapshot in [Value::Null, json!({ "sessionId": "other", "turn": 1, "state": "working" })] {
            let (mut c, _) = Conn::attach("/w".into(), "saved".into());
            live_init(&mut c);
            let result = c.read(&rpc::ok(&json!(2), json!({ "_meta": { "construct": { "turn": snapshot } } })));
            assert!(matches!(result.events.as_slice(), [Inbound::Failed(_)]));
            assert_eq!(c.prompt("no"), None);
        }
    }

    #[test]
    fn unique_request_namespace_ignores_queued_responses_with_reused_numeric_ids() {
        let (mut c, _) = Conn::begin("/w".into(), Target::Attach("saved".into()), Some("new-window".into()));
        assert_eq!(c.read(&rpc::ok(&json!(1), json!({ "stopReason": "end_turn" }))), Step::default());
        let step = c.read(&rpc::ok(
            &json!("new-window-1"),
            json!({ "agentCapabilities": { "loadSession": true, "_meta": { "construct": { "liveAttach": true } } } }),
        ));
        assert_eq!(step.lines.len(), 1);
        assert!(step.lines[0].contains("session/load"));
    }

    #[test]
    fn permission_ids_are_consumed_once_and_retired_when_the_turn_resumes() {
        let mut c = handshake();
        assert_eq!(c.answer(&json!("old"), Some("yes")), None);
        let request = rpc::request("fresh", "session/request_permission", json!({ "sessionId": "s1" }));
        c.read(&request);
        assert!(c.answer(&json!("fresh"), Some("yes")).is_some());
        assert_eq!(c.answer(&json!("fresh"), Some("no")), None);
        c.read(&request);
        c.read(&turn_line("s1", 1, "working"));
        assert_eq!(c.answer(&json!("fresh"), Some("yes")), None);
    }

    #[test]
    fn explicit_turn_completion_is_not_repeated_by_the_prompt_response() {
        let (mut c, _) = Conn::start("/w".into());
        live_init(&mut c);
        c.read(&rpc::ok(&json!(2), json!({ "sessionId": "s1" })));
        c.prompt("go");
        assert_eq!(
            c.read(&turn_line("s1", 1, "idle")).events,
            vec![Inbound::Turn { number: 1, state: TurnState::Idle { stop_reason: Some("cancelled".into()) } }]
        );
        assert!(c.prompt("next before the old response").is_some());
        assert_eq!(c.read(&rpc::ok(&json!(3), json!({ "stopReason": "cancelled" }))), Step::default());
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
        assert_eq!(
            c.read(tool).events,
            vec![Inbound::Tool { id: "t".into(), title: "read x".into(), status: ToolStatus::Running }]
        );
        let end = r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#;
        assert_eq!(c.read(end).events, vec![Inbound::TurnEnded("end_turn".into())]);
    }

    #[test]
    fn initial_tool_calls_preserve_status_including_finished_replay() {
        let mut c = handshake();
        for (status, expected) in [
            (Some("completed"), ToolStatus::Done),
            (Some("failed"), ToolStatus::Failed),
            (Some("pending"), ToolStatus::Running),
            (Some("in_progress"), ToolStatus::Running),
            (None, ToolStatus::Running),
        ] {
            let mut update = json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "read x" });
            if let Some(status) = status {
                update["status"] = json!(status);
            }
            let line = rpc::notification("session/update", json!({ "sessionId": "s1", "update": update }));
            assert_eq!(
                c.read(&line).events,
                vec![Inbound::Tool { id: "t".into(), title: "read x".into(), status: expected }]
            );
        }
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
        let reply = c.answer(request, Some("y")).unwrap();
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
