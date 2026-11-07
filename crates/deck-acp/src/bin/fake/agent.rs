//! The fake ACP actor. One owner serializes replay and live delivery; every
//! initialize retires question IDs and suppresses old prompt responses.

use super::journal::Journal;
use super::turn::{self, Turn};
use deck_acp::names::{
    BOUNDARY_METHOD, CONSTRUCT_META, DECK_META, LEGACY_BOUNDARY_METHOD, LEGACY_CONSTRUCT_META, LEGACY_DECK_META,
    LEGACY_TURN_METHOD, TURN_METHOD,
};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub struct Agent<W> {
    out: W,
    journal: Journal,
    session: String,
    history: Vec<Value>,
    number: u32,
    attachment: u64,
    attached: u64,
    next_question: u64,
    turn: Option<Turn>,
    stop: Option<String>,
    /// Whether this process has a boundary endpoint, matching Construct: the
    /// capability is advertised only when the endpoint exists.
    boundary: bool,
    /// The GitHub operation gateway (`DECK_GITHUB_URL`). When set, this client
    /// advertises `github_operations` and a prompt runs the publication turn
    /// instead of the permission script.
    github: Option<String>,
    /// The turn method this client asked for: the reverse-DNS name when it
    /// named the namespace, otherwise the bare one (deck#36).
    turn_method: &'static str,
    /// The namespaces this process advertises and accepts, like a particular
    /// Construct release: a released one speaks only the bare names, the next
    /// release both, and the release after that only the reverse-DNS names.
    /// `DECK_FAKE_NAMES=legacy|reverse` pins one; unset (or `both`) is the
    /// transition.
    legacy_names: bool,
    reverse_names: bool,
}

impl<W: Write> Agent<W> {
    pub fn new(out: W) -> Self {
        let names = std::env::var("DECK_FAKE_NAMES").unwrap_or_default();
        Self {
            out,
            journal: Journal::new(),
            session: String::new(),
            history: vec![],
            number: 0,
            attachment: 0,
            attached: 0,
            next_question: 900,
            turn: None,
            stop: None,
            boundary: std::env::var_os("DECK_BOUNDARY_URL").is_some_and(|value| !value.is_empty()),
            github: std::env::var("DECK_GITHUB_URL").ok().filter(|value| !value.is_empty()),
            turn_method: LEGACY_TURN_METHOD,
            legacy_names: names != "reverse",
            reverse_names: names != "legacy",
        }
    }

    /// Whether this release understands the boundary method as sent. A
    /// released Construct ignores a method it never advertised.
    fn accepts_boundary(&self, method: &str) -> bool {
        (self.legacy_names && method == LEGACY_BOUNDARY_METHOD) || (self.reverse_names && method == BOUNDARY_METHOD)
    }

    pub fn serve(self, input: impl BufRead + Send + 'static, delay: Duration) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in input.lines() {
                let Ok(line) = line else { break };
                if let Ok(value) = serde_json::from_str(&line)
                    && tx.send(value).is_err()
                {
                    break;
                }
            }
        });
        let mut agent = self;
        let mut tick = Instant::now() + delay;
        loop {
            match rx.recv_timeout(tick.saturating_duration_since(Instant::now())) {
                Ok(msg) => agent.receive(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    agent.advance();
                    tick = Instant::now() + delay.max(Duration::from_millis(1));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn send(&mut self, value: Value) {
        let _ = writeln!(self.out, "{value}");
        let _ = self.out.flush();
    }

    fn reply(&mut self, id: Value, result: Result<Value, (i64, String)>) {
        self.send(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        });
    }

    fn snapshot(&self) -> Value {
        let phase = match &self.turn {
            Some(turn) if turn.waiting => "waiting",
            Some(_) => "working",
            None => "idle",
        };
        let mut value = json!({ "sessionId": self.session, "turn": self.number, "state": phase });
        if let Some(stop) = &self.stop {
            value["stopReason"] = json!(stop);
        }
        value
    }

    fn notice(&mut self) {
        if !self.session.is_empty() {
            self.send(json!({ "jsonrpc": "2.0", "method": self.turn_method, "params": self.snapshot() }));
        }
    }

    fn send_update(&mut self, update: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": self.session, "update": update,
        }}));
    }

    fn update(&mut self, update: Value) {
        self.journal.update(&self.session, &update);
        self.history.push(update.clone());
        if self.attached == self.attachment {
            self.send_update(update);
        }
    }

    fn chunk(&mut self, kind: &str, text: &str) {
        self.update(json!({ "sessionUpdate": kind, "content": { "type": "text", "text": text } }));
    }

    /// One done step of the publication turn: the tool call, then its result
    /// as the transcript content ACP gives a tool call.
    fn tool_step(&mut self, index: usize, title: &str, result: Result<String, String>) {
        let id = format!("p{}-{index}", self.number);
        self.update(json!({ "sessionUpdate": "tool_call", "toolCallId": id,
            "title": title, "kind": "execute", "status": "in_progress" }));
        let (status, text) = match result {
            Ok(text) => ("completed", text),
            Err(error) => ("failed", error),
        };
        self.update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": status,
            "content": [{ "type": "content", "content": { "type": "text", "text": text } }] }));
    }

    /// The scripted publication turn (ADR 0008 §9). A gateway refusal is the
    /// failing step's text, not a retry: the fake exists to show the card and
    /// the transcript, not to recover from either.
    fn publish(&mut self, url: &str, asked: &str) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let mut step = 0;
        super::github::run(url, &cwd, asked, |title, result| {
            step += 1;
            self.tool_step(step, title, result);
        });
    }

    fn ask(&mut self) {
        let Some(turn) = self.turn.as_mut().filter(|t| t.waiting) else { return };
        if turn.question.is_some() {
            return;
        }
        self.next_question += 1;
        let id = json!(self.next_question);
        turn.question = Some(id.clone());
        self.send(turn::question(id, &self.session, self.number));
    }

    fn receive(&mut self, msg: Value) {
        let id = msg["id"].clone();
        let sid = msg["params"]["sessionId"].as_str().unwrap_or_default();
        match msg["method"].as_str() {
            Some("initialize") => {
                self.attachment += 1;
                if let Some(turn) = &mut self.turn {
                    turn.question = None;
                }
                let requested = &msg["params"]["clientCapabilities"]["_meta"];
                self.turn_method =
                    if self.reverse_names && (requested[CONSTRUCT_META]["turn"] == true || !self.legacy_names) {
                        TURN_METHOD
                    } else {
                        LEGACY_TURN_METHOD
                    };
                // This release speaks the turn method it knows; a released
                // Construct cannot be taught the new one by the client.
                let mut meta = json!({});
                if self.legacy_names {
                    meta[LEGACY_CONSTRUCT_META] = json!({ "liveAttach": true });
                }
                if self.reverse_names {
                    meta[CONSTRUCT_META] = json!({ "liveAttach": true });
                }
                let mut deck = serde_json::Map::new();
                if self.boundary && self.reverse_names {
                    deck.insert("boundary".into(), json!(true));
                }
                // The capability exists under the current key only (contract
                // 1.2.0 §11.1); a release pinned to the legacy namespace does
                // not speak it.
                if self.github.is_some() && self.reverse_names {
                    deck.insert("github_operations".into(), json!(1));
                }
                if !deck.is_empty() {
                    meta[DECK_META] = Value::Object(deck);
                }
                if self.boundary && self.legacy_names {
                    meta[LEGACY_DECK_META] = json!({ "boundary": true });
                }
                self.reply(
                    id,
                    Ok(json!({
                        "protocolVersion": 1,
                        "agentInfo": { "name": "fake", "title": "fake agent", "version": "0" },
                        "agentCapabilities": { "loadSession": true,
                            "sessionCapabilities": { "resume": {}, "close": {} },
                            "_meta": meta,
                        }, "authMethods": [],
                    })),
                );
                self.notice();
            }
            Some("session/new") => {
                self.session =
                    if self.journal.persistent() { format!("fake-{:x}", super::now_nanos()) } else { "fake-1".into() };
                self.journal.create(&self.session);
                self.history.clear();
                self.number = 0;
                self.stop = None;
                self.attached = self.attachment;
                self.reply(id, Ok(json!({ "sessionId": self.session })));
            }
            Some(method @ ("session/load" | "session/resume")) => {
                let result = self.load(sid, method == "session/load");
                self.reply(id, result);
            }
            Some("session/close") => {
                if self.session == sid {
                    self.finish("cancelled");
                    self.session.clear();
                }
                if let Some(path) = std::env::var_os("DECK_FAKE_CLOSE_LOG") {
                    let _ = std::fs::write(path, sid);
                }
                self.reply(id, Ok(json!({})));
            }
            Some("session/cancel") => self.finish("cancelled"),
            Some(method) if self.accepts_boundary(method) => {
                // Construct queues this for its next model request; the fake
                // has no model, so it puts the same note in the transcript.
                self.chunk("agent_message_chunk", &format!("\n{}", boundary_note(&msg["params"])));
            }
            Some("session/prompt") if sid == self.session && !sid.is_empty() && self.turn.is_none() => {
                let asked = msg["params"]["prompt"][0]["text"].as_str().unwrap_or_default();
                self.journal.begin(&self.session, asked);
                self.history.push(
                    json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": asked } }),
                );
                self.number += 1;
                self.stop = None;
                self.turn = Some(Turn::new(id, self.attachment, asked));
                self.notice();
                if let Some(url) = self.github.clone() {
                    self.chunk("agent_thought_chunk", "publishing the task branch");
                    self.publish(&url, asked);
                    self.finish("end_turn");
                } else {
                    self.chunk("agent_thought_chunk", "planning a small change");
                }
            }
            None => {
                let current = self.turn.as_ref().and_then(|t| t.question.as_ref());
                if current == Some(&id) {
                    let outcome = msg["result"]["outcome"]["optionId"].as_str().unwrap_or("cancelled");
                    if let Some(turn) = &mut self.turn {
                        turn.waiting = false;
                        turn.question = None;
                    }
                    self.notice();
                    let status = if matches!(outcome, "allow" | "always") { "completed" } else { "failed" };
                    self.update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": format!("t{}", self.number), "status": status }));
                    self.chunk("agent_message_chunk", &format!("\npermission: {outcome}"));
                    self.finish("end_turn");
                }
            }
            _ if !id.is_null() => self.reply(id, Err((-32601, "unknown method".into()))),
            _ => {}
        }
    }

    fn load(&mut self, sid: &str, replay: bool) -> Result<Value, (i64, String)> {
        if self.session == sid && self.attached == self.attachment {
            return Err((-32602, format!("session {sid} is already loaded on this connection")));
        }
        if self.session != sid {
            let (updates, turns, stop) = self.journal.load(sid).map_err(|_| (-32002, format!("no session {sid}")))?;
            self.session = sid.into();
            self.history = updates;
            self.number = turns;
            self.stop = stop;
        }
        if replay {
            for update in self.history.clone() {
                self.send_update(update);
            }
        }
        self.attached = self.attachment;
        self.ask();
        if replay {
            self.notice();
            let turn = self.snapshot();
            let mut meta = json!({});
            if self.legacy_names {
                meta[LEGACY_CONSTRUCT_META] = json!({ "turn": turn.clone() });
            }
            if self.reverse_names {
                meta[CONSTRUCT_META] = json!({ "turn": turn });
            }
            Ok(json!({ "_meta": meta }))
        } else {
            Ok(json!({}))
        }
    }

    fn advance(&mut self) {
        let Some(turn) = &mut self.turn else { return };
        if turn.waiting {
            return;
        }
        if let Some(word) = turn.words.pop_front() {
            self.chunk("agent_message_chunk", &word);
        } else {
            turn.waiting = true;
            self.update(json!({ "sessionUpdate": "tool_call", "toolCallId": format!("t{}", self.number),
                "title": "shell git push origin deck/spike", "kind": "execute", "status": "pending" }));
            self.notice();
            if self.attached == self.attachment {
                self.ask();
            }
        }
    }

    fn finish(&mut self, reason: &str) {
        let Some(turn) = self.turn.take() else { return };
        if reason == "cancelled" && turn.waiting {
            self.update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": format!("t{}", self.number), "status": "failed" }));
        }
        self.journal.end(&self.session, reason);
        self.stop = Some(reason.into());
        self.notice();
        if turn.attachment == self.attachment {
            self.reply(turn.prompt, Ok(json!({ "stopReason": reason })));
        }
    }
}

/// The note a late decision adds, matching Construct's wording closely enough
/// that a Deck test can assert on one prefix.
fn boundary_note(params: &Value) -> String {
    let id = params["approval_id"].as_str().unwrap_or_default();
    let destination = params["destination"].as_str().unwrap_or("an unnamed destination");
    let outcome = params["outcome"].as_str().unwrap_or("decided");
    let mut text = format!("[deck boundary] {destination} (approval {id}) is now {outcome}");
    if let Some(scope) = params["scope"].as_str() {
        text.push_str(&format!(" (scope: {scope})"));
    }
    text.push_str(". This updates an earlier denial; it grants nothing. Retry only if the task needs it.");
    text
}
