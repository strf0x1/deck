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
use crate::permission::{Questions, View};
use deck_acp::TurnState;
use deck_acp::names::{
    CONSTRUCT_META, IDEMPOTENCY_KEY, IDEMPOTENT_PROMPTS, LEGACY_CONSTRUCT_META, LEGACY_TURN_METHOD, TURN_METHOD,
};
use deck_acp::rpc::{self, Incoming};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A snapshot must be small enough to persist and display in full. A question
/// outside these bounds remains open but cannot be snapshotted for idle stop.
const MAX_QUESTIONS: usize = 64;
const MAX_OPTIONS: usize = 32;
const MAX_TITLE_BYTES: usize = 4096;
const MAX_OPTION_BYTES: usize = 1024;
const MAX_REQUEST_BYTES: usize = 1024;

/// One option offered by the agent for a permission question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionChoice {
    pub id: String,
    pub name: String,
    pub kind: String,
}

/// The question as displayed for its current ACP request. Its request ID is
/// only valid while the current agent and attachment remain alive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionQuestion {
    pub request: Value,
    pub title: String,
    pub options: Vec<PermissionChoice>,
}

struct Question {
    id: Value,
    details: Option<PermissionQuestion>,
}

struct QueuedPrompt {
    text: String,
    key: Option<String>,
}
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

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
    /// Permission observations changed, including within the same turn.
    Permissions,
    /// The holder set up this session itself (`session/new` or `session/load`).
    Session(String),
    /// The holder's prompt started this turn.
    Prompted(u64),
    /// A keyed prompt was answered without a new turn notification.
    Settled,
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
    idempotent: bool,
    session: Option<String>,
    turn: Option<Turn>,
    questions: Questions,
    setup_request: Option<(Value, Option<String>)>,
    staged: Option<Questions>,
    session_epoch: u64,
    seen_questions: HashSet<u64>,
    identities_exhausted: bool,
    recovery_questions: Vec<Question>,
    /// A prompt waiting for the session the holder is setting up.
    queued: Option<QueuedPrompt>,
    /// The holder's prompt, written, until its turn is seen to start.
    starting: Option<(Value, u64, bool)>,
    setup: Option<Setup>,
    message: crate::message::Reader,
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
            idempotent: false,
            session: None,
            turn: None,
            questions: Questions::new(format!("{tag}-0")),
            setup_request: None,
            staged: None,
            session_epoch: 0,
            seen_questions: HashSet::new(),
            identities_exhausted: false,
            recovery_questions: Vec::new(),
            queued: None,
            starting: None,
            setup: None,
            message: crate::message::Reader::default(),
        }
    }

    /// The agent's last assistant message on this session.
    pub fn message(&self) -> Option<&crate::message::LastMessage> {
        self.message.message()
    }

    pub fn turn(&self) -> Option<&Turn> {
        self.turn.as_ref()
    }

    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// Pending questions in arrival order. `None` means at least one cannot
    /// be preserved in full, so the caller must not stop and lose it.
    pub fn pending_questions(&self) -> Option<Vec<&PermissionQuestion>> {
        if self.recovery_questions.len() > MAX_QUESTIONS
            || self.recovery_questions.len() != self.questions.view().pending_count
        {
            return None;
        }
        self.recovery_questions.iter().map(|question| question.details.as_ref()).collect()
    }

    /// ACP evidence only. Host approvals and agent output not yet observed
    /// must be handled by the caller before any task can be stopped.
    pub(crate) fn fence_snapshot(&self) -> Option<(Turn, Vec<PermissionQuestion>)> {
        if !self.initialized
            || !self.ready
            || self.queued.is_some()
            || self.starting.is_some()
            || self.setup_request.is_some()
            || self.staged.is_some()
        {
            return None;
        }
        let turn = self.turn.as_ref()?;
        match turn.state {
            Phase::Idle => {
                if !self.questions.is_empty()
                    || self.client.values().any(|request| *request == Request::Prompt)
                    || self.own.values().any(|request| *request == Request::Prompt)
                {
                    return None;
                }
            }
            Phase::Waiting if !self.questions.is_empty() => {
                // The original turn may still have its prompt request open.
                // A second admitted prompt has an uncertain relationship to
                // the observed question and must not be snapshotted.
                let pending = self.client.values().filter(|request| **request == Request::Prompt).count()
                    + self.own.values().filter(|request| **request == Request::Prompt).count();
                if pending > 1 {
                    return None;
                }
            }
            _ => return None,
        }
        Some((turn.clone(), self.pending_questions()?.into_iter().cloned().collect()))
    }

    /// Whether work is in flight, so an idle stop must wait.
    pub fn busy(&self) -> bool {
        self.turn.as_ref().is_some_and(|turn| !turn.idle())
            || !self.questions.is_empty()
            || self.starting.is_some()
            || self.queued.is_some()
    }
    /// Work that makes an idle stop unsafe. An open question is handled by
    /// the waiting deadline, not by treating the task as busy forever.
    pub fn in_flight(&self) -> bool {
        self.turn.as_ref().is_some_and(|turn| turn.state == Phase::Working)
            || self.starting.is_some()
            || self.queued.is_some()
    }

    /// One line a window wrote to the agent.
    pub fn permissions(&self) -> View {
        self.questions.view()
    }

    pub fn client(&mut self, line: &[u8]) -> Vec<Event> {
        let before = self.permissions();
        let mut events = self.client_line(line);
        if before != self.permissions() {
            events.push(Event::Permissions);
        }
        events
    }

    fn client_line(&mut self, line: &[u8]) -> Vec<Event> {
        let Some(message) = std::str::from_utf8(line).ok().and_then(rpc::parse) else { return vec![] };
        match message {
            Incoming::Request { id, method, params } => {
                let request = match method.as_str() {
                    "initialize" => {
                        // A new attachment: the session must be set up again
                        // on it, and the agent retires its open questions.
                        self.ready = false;
                        self.idempotent = false;
                        self.retire_questions();
                        self.client.clear();
                        self.setup_request = None;
                        self.staged = None;
                        Request::Initialize
                    }
                    "session/new" => Request::New,
                    "session/load" => Request::Load(params["sessionId"].as_str().unwrap_or_default().to_owned()),
                    "session/prompt" => Request::Prompt,
                    _ => return vec![],
                };
                let started = request == Request::Prompt && !self.live;
                self.begin_setup(&id, &request);
                self.client.insert(id, request);
                if started {
                    // An agent without turn notifications: the prompt is the
                    // start and its response the end (as `conn.rs` reads it).
                    let number = self.turn.as_ref().map_or(0, |t| t.number) + 1;
                    return self.set(Turn { number, state: Phase::Working, stop_reason: None });
                }
                vec![]
            }
            Incoming::Response { id, result } => {
                // This is an observed response, not proof of delivery.
                self.recovery_questions.retain(|question| question.id != id);
                if let Some(staged) = self.staged.as_mut() {
                    staged.answer(&id, &result);
                }
                if self.questions.answer(&id, &result) && self.questions.is_empty() && !self.live {
                    return self.phase(Phase::Working);
                }
                vec![]
            }
            Incoming::Notification { .. } => vec![],
        }
    }

    /// One line the agent wrote.
    pub fn agent(&mut self, line: &[u8]) -> Step {
        let before = self.permissions();
        let mut step = self.agent_line(line);
        if before != self.permissions() {
            step.events.push(Event::Permissions);
        }
        step
    }

    fn agent_line(&mut self, line: &[u8]) -> Step {
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
                    Some(request) => self.client_response(&id, request, result),
                    None => vec![],
                };
                Step { forward: true, write: vec![], events }
            }
            Incoming::Notification { method, params } if method == TURN_METHOD || method == LEGACY_TURN_METHOD => {
                let mine = self.session.is_some() && self.session.as_deref() == params["sessionId"].as_str();
                let events = if mine { self.observe(&params) } else { vec![] };
                Step { forward: true, write: vec![], events }
            }
            Incoming::Notification { method, params } if method == "session/update" => {
                if self.session.is_none() || self.session.as_deref() == params["sessionId"].as_str() {
                    self.message.update(&params["update"]);
                }
                Step { forward: true, ..Step::default() }
            }
            Incoming::Request { id, method, params } if method == "session/request_permission" => {
                let accepted = self.question(id.clone(), &params);
                if accepted {
                    if let Some(question) = self.recovery_questions.iter_mut().find(|question| question.id == id) {
                        question.details = None;
                    } else {
                        let details = (self.recovery_questions.len() < MAX_QUESTIONS)
                            .then(|| permission_question(&id, &params))
                            .flatten();
                        self.recovery_questions.push(Question { id, details });
                    }
                }
                let events = if self.live || !accepted { vec![] } else { self.phase(Phase::Waiting) };
                Step { forward: true, write: vec![], events }
            }
            _ => Step { forward: true, ..Step::default() },
        }
    }

    /// Start a prompt, setting the session up first if nobody has.
    pub fn prompt(&mut self, text: &str, setup: Setup) -> Result<Step, Refused> {
        self.prompt_with_key(text, setup, None)
    }

    /// A supplied key is required: an agent without the capability is never
    /// sent the prompt, so a retry cannot silently become non-idempotent.
    pub fn prompt_with_key(&mut self, text: &str, setup: Setup, key: Option<&str>) -> Result<Step, Refused> {
        let busy = self.turn.as_ref().is_some_and(|t| !t.idle())
            || !self.questions.is_empty()
            || self.starting.is_some()
            || self.queued.is_some();
        if busy {
            return Err(Refused::Busy);
        }
        if key == Some("") {
            return Ok(Step {
                forward: false,
                write: vec![],
                events: vec![Event::Failed("empty idempotency key".into())],
            });
        }
        if self.ready {
            return Ok(self.send_prompt(text, key));
        }
        self.queued = Some(QueuedPrompt { text: text.to_owned(), key: key.map(str::to_owned) });
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
        self.begin_setup(&id, &request);
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

    fn send_prompt(&mut self, text: &str, key: Option<&str>) -> Step {
        if key.is_some() && !self.idempotent {
            return Step {
                forward: false,
                write: vec![],
                events: vec![Event::Failed("agent does not support idempotent prompts".into())],
            };
        }
        let Some(session) = self.session.clone() else {
            return Step { forward: false, write: vec![], events: vec![Event::Failed("no session to prompt".into())] };
        };
        let id = self.id(Request::Prompt);
        let mut params = json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] });
        if let Some(key) = key {
            params["_meta"] = json!({ CONSTRUCT_META: { IDEMPOTENCY_KEY: key } });
        }
        let line = rpc::request(id.clone(), "session/prompt", params);
        let before = self.turn.as_ref().map_or(0, |t| t.number);
        if self.live || key.is_some() {
            self.starting = Some((id, before, key.is_some()));
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
                    if !self.select_session(id, session) {
                        return Step::default();
                    }
                    let mut step = self.release();
                    step.events.insert(0, Event::Session(session.to_owned()));
                    step
                }
                None => fail(self, "session/new named no session".into()),
            },
            (Request::Load(session), Ok(value)) => {
                if !self.select_session(id, &session) {
                    return Step::default();
                }
                let mut events = vec![Event::Session(session)];
                events.extend(self.snapshot(&value));
                let mut step = self.release();
                events.append(&mut step.events);
                step.events = events;
                step
            }
            (Request::Prompt, Ok(value)) => {
                let keyed = self.starting.as_ref().is_some_and(|(pending, _, keyed)| pending == id && *keyed);
                if keyed {
                    self.starting = None;
                    return Step { forward: false, write: vec![], events: vec![Event::Settled] };
                }
                if self.live {
                    return Step::default();
                }
                let stop = value["stopReason"].as_str().unwrap_or("end_turn").to_owned();
                let number = self.turn.as_ref().map_or(0, |t| t.number);
                let events = self.set(Turn { number, state: Phase::Idle, stop_reason: Some(stop) });
                Step { forward: false, write: vec![], events }
            }
            (Request::Prompt, Err(error)) => {
                if self.starting.as_ref().is_some_and(|(pending, _, _)| pending == id) {
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
            (request, Err(error)) => {
                self.failed_setup(id);
                fail(self, format!("{}: {error}", name(&request)))
            }
        }
    }

    /// The queued prompt, once the session is ready.
    fn release(&mut self) -> Step {
        match self.queued.take() {
            Some(prompt) => self.send_prompt(&prompt.text, prompt.key.as_deref()),
            None => Step::default(),
        }
    }

    fn client_response(&mut self, id: &Value, request: Request, result: Result<Value, String>) -> Vec<Event> {
        let Ok(value) = result else {
            self.failed_setup(id);
            return vec![];
        };
        match request {
            Request::Initialize => {
                self.initialized(&value);
                vec![]
            }
            Request::New => {
                if let Some(session) = value["sessionId"].as_str() {
                    self.select_session(id, session);
                }
                vec![]
            }
            Request::Load(session) => {
                if !self.select_session(id, &session) {
                    return vec![];
                }
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

    fn retire_questions(&mut self) {
        self.recovery_questions.clear();
        self.questions.retire();
        if self.setup_request.as_ref().is_some_and(|(_, target)| *target == self.session)
            && let Some(staged) = self.staged.as_mut()
        {
            staged.retire();
        }
    }

    fn begin_setup(&mut self, id: &Value, request: &Request) {
        let target = match request {
            Request::Load(session) => Some(session.clone()),
            Request::New => None,
            _ => return,
        };
        self.session_epoch += 1;
        self.staged = Some(self.questions.stage(format!("{}-{}", self.tag, self.session_epoch)));
        self.setup_request = Some((id.clone(), target));
    }

    fn failed_setup(&mut self, id: &Value) {
        if self.setup_request.as_ref().is_some_and(|(request, _)| request == id) {
            self.setup_request = None;
            self.staged = None;
        }
    }

    fn select_session(&mut self, id: &Value, session: &str) -> bool {
        if !self.setup_request.as_ref().is_some_and(|(request, _)| request == id) {
            return false;
        }
        if let Some(mut staged) = self.staged.take() {
            if self.session.as_deref() == Some(session) {
                let previous = std::mem::replace(&mut self.questions, Questions::new(String::new()));
                staged.retain_history(previous);
            }
            self.questions = staged;
            // Replayed questions have separate wire identities. Until their
            // full payloads can be captured, never reuse an older snapshot.
            self.recovery_questions.clear();
        }
        if self.session.as_deref() != Some(session) {
            self.turn = None;
            self.message.reset();
        }
        self.setup_request = None;
        self.staged = None;
        self.session = Some(session.to_owned());
        self.ready = true;
        true
    }

    fn question(&mut self, id: Value, params: &Value) -> bool {
        let session = params["sessionId"].as_str();
        let current = (session.is_some() && session == self.session.as_deref()) || (session.is_none() && !self.live);
        let staged = self.setup_request.as_ref().is_some_and(|(_, target)| {
            (session.is_some() && session == target.as_deref()) || (session.is_none() && !self.live)
        }) && self.staged.is_some();
        if !current && !staged {
            return false;
        }
        if staged && self.questions.contains(&id) {
            self.questions.block(&id);
        }
        if !staged
            && let Some(replay) = self.staged.as_mut()
            && replay.contains(&id)
        {
            replay.block(&id);
        }
        let questions = match (staged, self.staged.as_mut()) {
            (true, Some(questions)) => questions,
            _ => &mut self.questions,
        };
        if questions.contains(&id) {
            questions.ask(id, params, None, false);
            return current && !staged;
        }
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut hash);
        let fingerprint = hash.finish();
        let reused = self.identities_exhausted || !self.seen_questions.insert(fingerprint);
        if self.seen_questions.len() >= 4096 {
            self.identities_exhausted = true;
        }
        questions.ask(id, params, if staged { None } else { self.turn.as_ref().map(|t| t.number) }, reused);
        current && !staged
    }

    fn initialized(&mut self, value: &Value) {
        self.initialized = true;
        self.ready = false;
        let meta = &value["agentCapabilities"]["_meta"];
        self.live = meta[CONSTRUCT_META]["liveAttach"] == true || meta[LEGACY_CONSTRUCT_META]["liveAttach"] == true;
        self.idempotent = meta[CONSTRUCT_META][IDEMPOTENT_PROMPTS] == true;
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
            self.retire_questions();
        }
        let mut events = self.set(turn);
        if let Some((_, before, _)) = &self.starting
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
        if turn.state != Phase::Waiting {
            self.retire_questions();
        }
        if turn.state == Phase::Working && self.turn.as_ref().is_none_or(|t| t.number < turn.number) {
            self.message.reset();
        }
        self.turn = Some(turn.clone());
        vec![Event::Turn(turn)]
    }
}

fn permission_question(request: &Value, params: &Value) -> Option<PermissionQuestion> {
    if serde_json::to_vec(request).ok()?.len() > MAX_REQUEST_BYTES {
        return None;
    }
    let title = params["toolCall"]["title"].as_str().unwrap_or("a tool");
    if title.len() > MAX_TITLE_BYTES {
        return None;
    }
    let mut options = Vec::new();
    if let Some(offered) = params["options"].as_array() {
        for option in offered {
            let Some(id) = option["optionId"].as_str() else { continue };
            let name = option["name"].as_str().unwrap_or_default();
            let kind = option["kind"].as_str().unwrap_or_default();
            if options.len() == MAX_OPTIONS || [id, name, kind].iter().any(|field| field.len() > MAX_OPTION_BYTES) {
                return None;
            }
            options.push(PermissionChoice { id: id.into(), name: name.into(), kind: kind.into() });
        }
    }
    Some(PermissionQuestion { request: request.clone(), title: title.into(), options })
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
