//! The holder's view of one ACP stream (ADR 0011 §3): fed every line in both
//! directions, it knows whether a session is set up, which one, the current
//! turn and open permission questions, and it can start a prompt of its own.
//! Pure: no I/O, so every rule here is a unit test.
//!
//! The design choice: the holder is a reader first. It sees the whole stream
//! from the agent's start, so a window's `initialize` and `session/load` set
//! the session up for it too, and it writes only when `deckd` asks it to
//! prompt. Responses to its own requests carry a reserved id prefix and are
//! kept from the window; every other line is forwarded untouched. It never
//! answers an agent request: permission questions stay for a window.

use crate::lifecycle::{Phase, Turn};
use deck_acp::TurnState;
use deck_acp::names::{CONSTRUCT_META, LEGACY_CONSTRUCT_META, LEGACY_TURN_METHOD, TURN_METHOD};
use deck_acp::rpc::{self, Incoming};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

/// The request id prefix the holder writes with; a window's ids start `deck-`.
pub const PREFIX: &str = "holder";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Request {
    Initialize,
    New,
    Load(String),
    Prompt,
}

/// What one agent line means for the holder.
#[derive(Debug, Default, PartialEq)]
pub struct Step {
    /// Whether the window should see the line. False only for responses to
    /// the holder's own requests.
    pub forward: bool,
    /// Lines the holder must write to the agent next.
    pub write: Vec<String>,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The turn changed; the holder records it.
    Turn(Turn),
    /// The holder set up this session itself (`session/new` or `session/load`).
    Session(String),
    /// The holder's prompt started this turn.
    Prompted(u64),
    /// The holder's prompt cannot start.
    Failed(String),
}

#[derive(Debug, PartialEq)]
pub enum Refused {
    /// A turn is running, a question is open, or a prompt is already starting.
    Busy,
}

/// How to set the session up if nobody has: the agent's working directory
/// and the session to load, as the window would choose them.
#[derive(Debug, Clone)]
pub struct Setup {
    pub cwd: String,
    pub session: Option<String>,
}

pub struct Observer {
    tag: String,
    next: u64,
    client: HashMap<Value, Request>,
    own: HashMap<Value, Request>,
    initialized: bool,
    /// A session is set up on the stream's current attachment.
    ready: bool,
    /// The agent sends turn notifications (its live-attach contract), so a
    /// prompt response is not the end of a turn.
    live: bool,
    session: Option<String>,
    turn: Option<Turn>,
    questions: HashSet<Value>,
    /// A prompt waiting for the session the holder is setting up.
    queued: Option<String>,
    /// The holder's prompt, written, until its turn is seen to start.
    starting: Option<(Value, u64)>,
    setup: Option<Setup>,
}

impl Observer {
    /// `tag` keeps this holder's ids apart from an earlier holder's on a
    /// reused stream; the process id is enough.
    pub fn new(tag: &str) -> Self {
        Observer {
            tag: tag.into(),
            next: 0,
            client: HashMap::new(),
            own: HashMap::new(),
            initialized: false,
            ready: false,
            live: false,
            session: None,
            turn: None,
            questions: HashSet::new(),
            queued: None,
            starting: None,
            setup: None,
        }
    }

    pub fn turn(&self) -> Option<&Turn> {
        self.turn.as_ref()
    }

    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// One line a window wrote to the agent.
    pub fn client(&mut self, line: &[u8]) -> Vec<Event> {
        let Some(message) = std::str::from_utf8(line).ok().and_then(rpc::parse) else { return vec![] };
        match message {
            Incoming::Request { id, method, params } => {
                let request = match method.as_str() {
                    "initialize" => {
                        // A new attachment: the session must be set up again
                        // on it, and the agent retires its open questions.
                        self.ready = false;
                        self.questions.clear();
                        Request::Initialize
                    }
                    "session/new" => Request::New,
                    "session/load" => Request::Load(params["sessionId"].as_str().unwrap_or_default().to_owned()),
                    "session/prompt" => Request::Prompt,
                    _ => return vec![],
                };
                let started = request == Request::Prompt && !self.live;
                self.client.insert(id, request);
                if started {
                    // An agent without turn notifications: the prompt is the
                    // start and its response the end (as `conn.rs` reads it).
                    let number = self.turn.as_ref().map_or(0, |t| t.number) + 1;
                    return self.set(Turn { number, state: Phase::Working, stop_reason: None });
                }
                vec![]
            }
            Incoming::Response { id, .. } => {
                // The window answered a permission question.
                if self.questions.remove(&id) && self.questions.is_empty() && !self.live {
                    return self.phase(Phase::Working);
                }
                vec![]
            }
            Incoming::Notification { .. } => vec![],
        }
    }

    /// One line the agent wrote.
    pub fn agent(&mut self, line: &[u8]) -> Step {
        let Some(message) = std::str::from_utf8(line).ok().and_then(rpc::parse) else {
            return Step { forward: true, ..Step::default() };
        };
        match message {
            Incoming::Response { id, result } => {
                if let Some(request) = self.own.remove(&id) {
                    let mut step = self.own_response(&id, request, result);
                    step.forward = false;
                    return step;
                }
                let events = match self.client.remove(&id) {
                    Some(request) => self.client_response(request, result),
                    None => vec![],
                };
                Step { forward: true, write: vec![], events }
            }
            Incoming::Notification { method, params } if method == TURN_METHOD || method == LEGACY_TURN_METHOD => {
                let mine = self.session.is_some() && self.session.as_deref() == params["sessionId"].as_str();
                let events = if mine { self.observe(&params) } else { vec![] };
                Step { forward: true, write: vec![], events }
            }
            Incoming::Request { id, method, .. } if method == "session/request_permission" => {
                self.questions.insert(id);
                let events = if self.live { vec![] } else { self.phase(Phase::Waiting) };
                Step { forward: true, write: vec![], events }
            }
            _ => Step { forward: true, ..Step::default() },
        }
    }

    /// Start a prompt, setting the session up first if nobody has.
    pub fn prompt(&mut self, text: &str, setup: Setup) -> Result<Step, Refused> {
        let busy = self.turn.as_ref().is_some_and(|t| !t.idle())
            || !self.questions.is_empty()
            || self.starting.is_some()
            || self.queued.is_some();
        if busy {
            return Err(Refused::Busy);
        }
        if self.ready {
            return Ok(self.send_prompt(text));
        }
        self.queued = Some(text.to_owned());
        self.setup = Some(setup);
        let line = if self.initialized { self.open_session() } else { self.initialize() };
        Ok(Step { forward: false, write: vec![line], events: vec![] })
    }

    /// Give up on a prompt that did not start in time, so the next one is
    /// not refused as busy. A late turn still updates the turn as usual.
    pub fn abandon(&mut self) {
        self.queued = None;
        self.starting = None;
    }

    fn id(&mut self, request: Request) -> Value {
        self.next += 1;
        let id = json!(format!("{PREFIX}-{}-{}", self.tag, self.next));
        self.own.insert(id.clone(), request);
        id
    }

    fn initialize(&mut self) -> String {
        let mut caps =
            json!({ "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false, "_meta": {} });
        caps["_meta"][CONSTRUCT_META] = json!({ "turn": true });
        let id = self.id(Request::Initialize);
        rpc::request(
            id,
            "initialize",
            json!({
                "protocolVersion": deck_acp::VERSION,
                "clientCapabilities": caps,
                "clientInfo": { "name": "deck-holder", "version": env!("CARGO_PKG_VERSION") },
            }),
        )
    }

    fn open_session(&mut self) -> String {
        let setup = self.setup.clone().unwrap_or(Setup { cwd: "/".into(), session: None });
        let mut params = json!({ "cwd": setup.cwd, "mcpServers": [] });
        match setup.session.or_else(|| self.session.clone()) {
            Some(session) => {
                params["sessionId"] = json!(&session);
                let id = self.id(Request::Load(session));
                rpc::request(id, "session/load", params)
            }
            None => {
                let id = self.id(Request::New);
                rpc::request(id, "session/new", params)
            }
        }
    }

    fn send_prompt(&mut self, text: &str) -> Step {
        let Some(session) = self.session.clone() else {
            return Step { forward: false, write: vec![], events: vec![Event::Failed("no session to prompt".into())] };
        };
        let id = self.id(Request::Prompt);
        let params = json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] });
        let line = rpc::request(id.clone(), "session/prompt", params);
        let before = self.turn.as_ref().map_or(0, |t| t.number);
        if self.live {
            self.starting = Some((id, before));
            return Step { forward: false, write: vec![line], events: vec![] };
        }
        let mut events = self.set(Turn { number: before + 1, state: Phase::Working, stop_reason: None });
        events.push(Event::Prompted(before + 1));
        Step { forward: false, write: vec![line], events }
    }

    fn own_response(&mut self, id: &Value, request: Request, result: Result<Value, String>) -> Step {
        let fail = |this: &mut Self, why: String| {
            this.queued = None;
            Step { forward: false, write: vec![], events: vec![Event::Failed(why)] }
        };
        match (request, result) {
            (Request::Initialize, Ok(value)) => {
                self.initialized(&value);
                let line = self.open_session();
                Step { forward: false, write: vec![line], events: vec![] }
            }
            (Request::New, Ok(value)) => match value["sessionId"].as_str() {
                Some(session) => {
                    self.ready = true;
                    self.session = Some(session.to_owned());
                    let mut step = self.release();
                    step.events.insert(0, Event::Session(session.to_owned()));
                    step
                }
                None => fail(self, "session/new named no session".into()),
            },
            (Request::Load(session), Ok(value)) => {
                self.ready = true;
                self.session = Some(session.clone());
                let mut events = vec![Event::Session(session)];
                events.extend(self.snapshot(&value));
                let mut step = self.release();
                events.append(&mut step.events);
                step.events = events;
                step
            }
            (Request::Prompt, Ok(value)) => {
                if self.live {
                    return Step::default();
                }
                let stop = value["stopReason"].as_str().unwrap_or("end_turn").to_owned();
                let number = self.turn.as_ref().map_or(0, |t| t.number);
                let events = self.set(Turn { number, state: Phase::Idle, stop_reason: Some(stop) });
                Step { forward: false, write: vec![], events }
            }
            (Request::Prompt, Err(error)) => {
                if self.starting.as_ref().is_some_and(|(pending, _)| pending == id) {
                    self.starting = None;
                }
                let mut step = fail(self, format!("session/prompt: {error}"));
                if !self.live {
                    let number = self.turn.as_ref().map_or(0, |t| t.number);
                    step.events.extend(self.set(Turn {
                        number,
                        state: Phase::Idle,
                        stop_reason: Some("refused".into()),
                    }));
                }
                step
            }
            (request, Err(error)) => fail(self, format!("{}: {error}", name(&request))),
        }
    }

    /// The queued prompt, once the session is ready.
    fn release(&mut self) -> Step {
        match self.queued.take() {
            Some(text) => self.send_prompt(&text),
            None => Step::default(),
        }
    }

    fn client_response(&mut self, request: Request, result: Result<Value, String>) -> Vec<Event> {
        let Ok(value) = result else { return vec![] };
        match request {
            Request::Initialize => {
                self.initialized(&value);
                vec![]
            }
            Request::New => {
                if let Some(session) = value["sessionId"].as_str() {
                    self.ready = true;
                    self.session = Some(session.to_owned());
                }
                vec![]
            }
            Request::Load(session) => {
                self.ready = true;
                self.session = Some(session);
                self.snapshot(&value)
            }
            Request::Prompt if !self.live => {
                let stop = value["stopReason"].as_str().unwrap_or("end_turn").to_owned();
                let number = self.turn.as_ref().map_or(0, |t| t.number);
                self.set(Turn { number, state: Phase::Idle, stop_reason: Some(stop) })
            }
            Request::Prompt => vec![],
        }
    }

    fn initialized(&mut self, value: &Value) {
        self.initialized = true;
        self.ready = false;
        let meta = &value["agentCapabilities"]["_meta"];
        self.live = meta[CONSTRUCT_META]["liveAttach"] == true || meta[LEGACY_CONSTRUCT_META]["liveAttach"] == true;
    }

    /// The turn a `session/load` reply reports in its metadata.
    fn snapshot(&mut self, value: &Value) -> Vec<Event> {
        let meta = &value["_meta"];
        let turn = if meta[CONSTRUCT_META]["turn"].is_null() {
            &meta[LEGACY_CONSTRUCT_META]["turn"]
        } else {
            &meta[CONSTRUCT_META]["turn"]
        };
        self.observe(turn)
    }

    fn observe(&mut self, value: &Value) -> Vec<Event> {
        let Some((number, state)) = deck_acp::inbound::parse_turn(value) else { return vec![] };
        let older = self.turn.as_ref().is_some_and(|t| t.number > number);
        if older {
            return vec![];
        }
        let turn = match state {
            TurnState::Working => Turn { number, state: Phase::Working, stop_reason: None },
            TurnState::Waiting => Turn { number, state: Phase::Waiting, stop_reason: None },
            TurnState::Idle { stop_reason } => Turn { number, state: Phase::Idle, stop_reason },
        };
        if turn.state != Phase::Waiting {
            self.questions.clear();
        }
        let mut events = self.set(turn);
        if let Some((_, before)) = &self.starting
            && number > *before
        {
            self.starting = None;
            events.push(Event::Prompted(number));
        }
        events
    }

    fn phase(&mut self, state: Phase) -> Vec<Event> {
        let Some(turn) = self.turn.clone() else { return vec![] };
        self.set(Turn { state, stop_reason: None, ..turn })
    }

    fn set(&mut self, turn: Turn) -> Vec<Event> {
        if self.turn.as_ref() == Some(&turn) {
            return vec![];
        }
        self.turn = Some(turn.clone());
        vec![Event::Turn(turn)]
    }
}

fn name(request: &Request) -> &'static str {
    match request {
        Request::Initialize => "initialize",
        Request::New => "session/new",
        Request::Load(_) => "session/load",
        Request::Prompt => "session/prompt",
    }
}

#[cfg(test)]
#[path = "observer_tests.rs"]
mod tests;
