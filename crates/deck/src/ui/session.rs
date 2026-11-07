//! What one agent session has said and asked, as the UI shows it: transcript
//! entries, the pending permission question, and the connection status.
//!
//! The design choice: pure state, folded from [`Inbound`] events, with no
//! GPUI types, so a whole turn is testable as a list of events in and
//! entries out. The clock is an argument ([`Session::apply_at`]) for the
//! same reason: tool durations are measured, not read from the agent.

use deck_acp::{Inbound, PermissionOption, ToolCall, ToolStatus, ToolUpdate, TurnState};
use serde_json::Value;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    You(String),
    Agent(String),
    Thought(String),
    Tool(Tool),
    Note(String),
}

/// A tool call in the transcript, with how long it ran when this window saw
/// it run.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub call: ToolCall,
    pub timing: Timing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timing {
    /// Replayed already finished, so this window never saw it run.
    Unknown,
    /// Running since this instant. A call that was already running when the
    /// window attached counts from the attach, so it reads short.
    Since(Instant),
    /// Ran for this long.
    Took(Duration),
}

impl Tool {
    fn new(call: ToolCall, now: Instant) -> Self {
        let timing = if call.status == ToolStatus::Running { Timing::Since(now) } else { Timing::Unknown };
        Tool { call, timing }
    }

    fn update(&mut self, update: ToolUpdate, now: Instant) {
        self.call.apply(update);
        self.timing = match (self.timing, self.call.status) {
            (Timing::Since(start), ToolStatus::Done | ToolStatus::Failed) => Timing::Took(now - start),
            (Timing::Unknown, ToolStatus::Running) => Timing::Since(now),
            (timing, _) => timing,
        };
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Connecting,
    Ready,
    Working,
    Waiting,
    /// The task is running in deckd, but this window has no ACP connection.
    Detached,
    Down(String),
}

#[derive(Debug, Clone)]
pub struct Ask {
    pub request: Value,
    pub title: String,
    pub options: Vec<PermissionOption>,
}

pub struct Session {
    pub agent: String,
    pub status: Status,
    pub entries: Vec<Entry>,
    pub ask: Option<Ask>,
    /// The live session this connection attached to, for extension
    /// notifications such as the boundary decision.
    pub session_id: Option<String>,
    questions: VecDeque<Ask>,
}

impl Session {
    pub fn new() -> Self {
        Session {
            agent: String::from("agent"),
            status: Status::Connecting,
            entries: vec![],
            ask: None,
            session_id: None,
            questions: VecDeque::new(),
        }
    }

    pub fn can_prompt(&self) -> bool {
        self.status == Status::Ready
    }

    /// Record a prompt the user sent.
    pub fn sent(&mut self, text: &str) {
        self.entries.push(Entry::You(text.to_string()));
        self.status = Status::Working;
    }

    /// Take the pending question once the user has answered it.
    pub fn answered(&mut self) -> Option<Ask> {
        let ask = self.ask.take()?;
        self.ask = self.questions.pop_front();
        self.status = if self.ask.is_some() { Status::Waiting } else { Status::Working };
        Some(ask)
    }

    pub fn apply(&mut self, ev: Inbound) {
        self.apply_at(ev, Instant::now());
    }

    /// [`Session::apply`] with the clock given, for tests.
    pub fn apply_at(&mut self, ev: Inbound, now: Instant) {
        match ev {
            Inbound::Ready { agent, session_id, turn } => {
                self.agent = agent;
                self.session_id = Some(session_id);
                self.status = if self.ask.is_some() { Status::Waiting } else { Status::Ready };
                if let Some(state) = turn {
                    self.set_turn(state);
                }
            }
            // The window keeps the capability; the transcript has nothing to show.
            Inbound::Publishes => {}
            Inbound::Detached => {
                self.ask = None;
                self.questions.clear();
                self.session_id = None;
                self.status = Status::Detached;
            }
            Inbound::Turn { state, .. } => self.set_turn(state),
            Inbound::UserMessage(t) => match self.entries.last_mut() {
                Some(Entry::You(s)) => s.push_str(&t),
                _ => self.entries.push(Entry::You(t)),
            },
            Inbound::Message(t) => match self.entries.last_mut() {
                Some(Entry::Agent(s)) => s.push_str(&t),
                _ => self.entries.push(Entry::Agent(t)),
            },
            Inbound::Thought(t) => match self.entries.last_mut() {
                Some(Entry::Thought(s)) => s.push_str(&t),
                _ => self.entries.push(Entry::Thought(t)),
            },
            Inbound::Tool(call) => self.entries.push(Entry::Tool(Tool::new(call, now))),
            Inbound::ToolUpdate(update) => {
                let tool = self.entries.iter_mut().rev().find_map(|e| match e {
                    Entry::Tool(tool) if tool.call.id == update.id => Some(tool),
                    _ => None,
                });
                if let Some(tool) = tool {
                    tool.update(update, now);
                }
            }
            Inbound::Permission { request, title, options } => {
                let ask = Ask { request, title, options };
                if self.ask.is_none() {
                    self.ask = Some(ask);
                } else {
                    self.questions.push_back(ask);
                }
                if self.status != Status::Connecting {
                    self.status = Status::Waiting;
                }
            }
            Inbound::TurnEnded(stop) => {
                if stop != "end_turn" {
                    self.entries.push(Entry::Note(format!("turn ended: {stop}")));
                }
                self.ask = None;
                self.questions.clear();
                self.status = Status::Ready;
            }
            Inbound::Failed(e) => {
                self.ask = None;
                self.questions.clear();
                self.entries.push(Entry::Note(e.clone()));
                if self.status == Status::Connecting || e.contains("exited") || e == "the connection closed" {
                    self.status = Status::Down(e);
                }
            }
        }
    }
    fn set_turn(&mut self, state: TurnState) {
        match state {
            TurnState::Working => {
                self.ask = None;
                self.questions.clear();
                self.status = Status::Working;
            }
            TurnState::Waiting => self.status = Status::Waiting,
            TurnState::Idle { stop_reason } => {
                if let Some(stop) = stop_reason
                    && stop != "end_turn"
                {
                    self.entries.push(Entry::Note(format!("turn ended: {stop}")));
                }
                self.ask = None;
                self.questions.clear();
                self.status = Status::Ready;
            }
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_acp::{ToolContent, ToolKind};

    fn call(id: &str, status: ToolStatus) -> ToolCall {
        ToolCall { id: id.into(), title: "read".into(), status, ..ToolCall::default() }
    }

    #[test]
    fn a_tool_keeps_its_content_and_how_long_it_ran() {
        let start = Instant::now();
        let mut s = Session::new();
        let edit = ToolCall { kind: ToolKind::Edit, ..call("e", ToolStatus::Running) };
        s.apply_at(Inbound::Tool(edit), start);
        s.apply_at(Inbound::Tool(call("replayed", ToolStatus::Done)), start);
        let diff = ToolContent::Diff { path: "a.rs".into(), old: Some("x".into()), new: "y".into() };
        let later = start + Duration::from_millis(1500);
        let content = ToolUpdate { id: "e".into(), content: Some(vec![diff.clone()]), ..ToolUpdate::default() };
        s.apply_at(Inbound::ToolUpdate(content), later);
        let Some(Entry::Tool(tool)) = s.entries.first() else { panic!("{:?}", s.entries) };
        assert_eq!(tool.timing, Timing::Since(start), "content alone does not finish a call");
        let done = ToolUpdate { id: "e".into(), status: Some(ToolStatus::Done), ..ToolUpdate::default() };
        s.apply_at(Inbound::ToolUpdate(done), later);
        let [Entry::Tool(edit), Entry::Tool(replayed)] = &s.entries[..] else { panic!("{:?}", s.entries) };
        assert_eq!(edit.call.kind, ToolKind::Edit);
        assert_eq!(edit.call.content, vec![diff]);
        assert_eq!(edit.timing, Timing::Took(Duration::from_millis(1500)));
        assert_eq!(replayed.timing, Timing::Unknown);
    }

    #[test]
    fn chunks_merge_and_tools_update_in_place() {
        let now = Instant::now();
        let mut s = Session::new();
        s.apply(Inbound::Ready { agent: "construct".into(), session_id: "x".into(), turn: None });
        assert!(s.can_prompt());
        s.sent("go");
        assert!(!s.can_prompt());
        for ev in [
            Inbound::Thought("plan".into()),
            Inbound::Message("a".into()),
            Inbound::Message("b".into()),
            Inbound::Tool(call("t", ToolStatus::Running)),
            Inbound::ToolUpdate(ToolUpdate { id: "t".into(), status: Some(ToolStatus::Done), ..ToolUpdate::default() }),
            Inbound::TurnEnded("end_turn".into()),
        ] {
            s.apply_at(ev, now);
        }
        assert_eq!(
            s.entries,
            vec![
                Entry::You("go".into()),
                Entry::Thought("plan".into()),
                Entry::Agent("ab".into()),
                Entry::Tool(Tool { call: call("t", ToolStatus::Done), timing: Timing::Took(Duration::ZERO) }),
            ]
        );
        assert!(s.can_prompt());
    }

    #[test]
    fn loading_history_rebuilds_both_sides_before_ready() {
        let mut s = Session::new();
        s.apply(Inbound::UserMessage("what ".into()));
        s.apply(Inbound::UserMessage("changed?".into()));
        s.apply(Inbound::Message("the daemon owns the task".into()));
        s.apply(Inbound::Ready { agent: "fake".into(), session_id: "saved".into(), turn: None });
        assert_eq!(
            s.entries,
            vec![Entry::You("what changed?".into()), Entry::Agent("the daemon owns the task".into())]
        );
        assert_eq!(s.status, Status::Ready);
    }

    #[test]
    fn replayed_questions_are_answered_in_order_and_idle_retires_the_rest() {
        let mut s = Session::new();
        for id in [901, 902, 903] {
            s.apply(Inbound::Permission { request: serde_json::json!(id), title: "read".into(), options: vec![] });
        }
        s.apply(Inbound::Ready { agent: "construct".into(), session_id: "s".into(), turn: Some(TurnState::Waiting) });
        assert_eq!(s.status, Status::Waiting);
        assert!(!s.can_prompt());
        assert_eq!(s.answered().unwrap().request, serde_json::json!(901));
        assert_eq!(s.status, Status::Waiting);
        assert_eq!(s.answered().unwrap().request, serde_json::json!(902));
        s.apply(Inbound::Turn { number: 1, state: TurnState::Idle { stop_reason: None } });
        assert!(s.answered().is_none());
        assert!(s.can_prompt());
    }

    #[test]
    fn a_detached_running_task_does_not_accept_prompts() {
        let mut s = Session::new();
        s.status = Status::Detached;
        assert!(!s.can_prompt());
    }

    #[test]
    fn a_handshake_failure_takes_the_session_down() {
        let mut s = Session::new();
        s.apply(Inbound::Failed("launching construct: not found".into()));
        assert!(matches!(s.status, Status::Down(_)));
    }
}
