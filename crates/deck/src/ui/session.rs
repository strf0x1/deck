//! What one agent session has said and asked, as the UI shows it: transcript
//! entries, the pending permission question, and the connection status.
//!
//! The design choice: pure state, folded from [`Inbound`] events, with no
//! GPUI types, so a whole turn is testable as a list of events in and
//! entries out.

use deck_acp::{Inbound, PermissionOption, ToolStatus};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    You(String),
    Agent(String),
    Thought(String),
    Tool { id: String, title: String, status: ToolStatus },
    Note(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Connecting,
    Ready,
    Working,
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
}

impl Session {
    pub fn new() -> Self {
        Session { agent: String::from("agent"), status: Status::Connecting, entries: vec![], ask: None }
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
        self.ask.take()
    }

    pub fn apply(&mut self, ev: Inbound) {
        match ev {
            Inbound::Ready { agent, .. } => {
                self.agent = agent;
                self.status = Status::Ready;
            }
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
            Inbound::Tool { id, title, status } => self.entries.push(Entry::Tool { id, title, status }),
            Inbound::ToolStatus { id, status } => {
                let tool = self.entries.iter_mut().rev().find_map(|e| match e {
                    Entry::Tool { id: tid, status, .. } if *tid == id => Some(status),
                    _ => None,
                });
                if let Some(s) = tool {
                    *s = status;
                }
            }
            Inbound::Permission { request, title, options } => self.ask = Some(Ask { request, title, options }),
            Inbound::TurnEnded(stop) => {
                if stop != "end_turn" {
                    self.entries.push(Entry::Note(format!("turn ended: {stop}")));
                }
                self.ask = None;
                self.status = Status::Ready;
            }
            Inbound::Failed(e) => {
                self.entries.push(Entry::Note(e.clone()));
                if self.status == Status::Connecting || e.contains("exited") || e == "the connection closed" {
                    self.status = Status::Down(e);
                }
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

    #[test]
    fn chunks_merge_and_tools_update_in_place() {
        let mut s = Session::new();
        s.apply(Inbound::Ready { agent: "construct".into(), session_id: "x".into() });
        assert!(s.can_prompt());
        s.sent("go");
        assert!(!s.can_prompt());
        for ev in [
            Inbound::Thought("plan".into()),
            Inbound::Message("a".into()),
            Inbound::Message("b".into()),
            Inbound::Tool { id: "t".into(), title: "read".into(), status: ToolStatus::Running },
            Inbound::ToolStatus { id: "t".into(), status: ToolStatus::Done },
            Inbound::TurnEnded("end_turn".into()),
        ] {
            s.apply(ev);
        }
        assert_eq!(
            s.entries,
            vec![
                Entry::You("go".into()),
                Entry::Thought("plan".into()),
                Entry::Agent("ab".into()),
                Entry::Tool { id: "t".into(), title: "read".into(), status: ToolStatus::Done },
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
        s.apply(Inbound::Ready { agent: "fake".into(), session_id: "saved".into() });
        assert_eq!(
            s.entries,
            vec![Entry::You("what changed?".into()), Entry::Agent("the daemon owns the task".into())]
        );
        assert_eq!(s.status, Status::Ready);
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
