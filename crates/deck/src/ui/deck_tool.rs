//! A coordinator's `deck` tool call as the window reads it (ADR 0011 §9,
//! construct#222): which operation it is and which tasks it names.
//!
//! The design choice: recognition is strict and falls back. Construct titles
//! the call `deck <operation> <id or project>` and sends its arguments as
//! `rawInput`; only a call with both, and an operation in the contract's
//! closed set, is drawn as a `deck` line. Anything else, and any failed call
//! (whose error the user should read in full), stays an ordinary tool card.
//! Task ids come from the input's `id`, a `create` reply's "Created task
//! <id>", and any id the window knows that appears in the reply.

use super::session::Entry;
use deck_acp::{ToolCall, ToolStatus};

/// The contract's operations (contract 1.4.0 §13.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Tasks,
    Task,
    Create,
    Prompt,
    Wait,
    Stop,
}

impl Op {
    fn parse(name: &str) -> Option<Op> {
        Some(match name {
            "tasks" => Op::Tasks,
            "task" => Op::Task,
            "create" => Op::Create,
            "prompt" => Op::Prompt,
            "wait" => Op::Wait,
            "stop" => Op::Stop,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Op::Tasks => "tasks",
            Op::Task => "task",
            Op::Create => "create",
            Op::Prompt => "prompt",
            Op::Wait => "wait",
            Op::Stop => "stop",
        }
    }

    /// Reads fold like a read: their reply is a click away.
    pub fn folds(self) -> bool {
        matches!(self, Op::Tasks | Op::Task)
    }
}

/// A recognized `deck` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub op: Op,
    /// The tasks it names, in order, without repeats.
    pub ids: Vec<String>,
    /// The title a `create` asked for, until its task is listed.
    pub title: Option<String>,
}

/// The `deck` call `call` is, or `None` to draw it as an ordinary card.
pub fn parse(call: &ToolCall, known: &[&str]) -> Option<Call> {
    if call.status == ToolStatus::Failed || !(call.title == "deck" || call.title.starts_with("deck ")) {
        return None;
    }
    let input = call.input.as_ref()?.as_object()?;
    let op = Op::parse(input.get("operation")?.as_str()?)?;
    let output = super::transcript::output(call);
    let mut ids: Vec<String> = Vec::new();
    let mut push = |id: &str| {
        if !id.is_empty() && !ids.iter().any(|known| known == id) {
            ids.push(id.to_string());
        }
    };
    if let Some(id) = input.get("id").and_then(|v| v.as_str()) {
        push(id);
    }
    if op == Op::Create
        && let Some(id) = output.strip_prefix("Created task ").and_then(|rest| rest.split_whitespace().next())
    {
        push(id.trim_end_matches([';', ',', '.']));
    }
    // A listing names tasks by id at the start of each line.
    for id in known {
        if output.contains(id) {
            push(id);
        }
    }
    let title = input.get("title").and_then(|v| v.as_str()).map(str::to_owned);
    Some(Call { op, ids, title })
}

/// Whether `entry` is a `deck wait`: consecutive waits fold into one line.
pub fn is_wait(entry: &Entry) -> bool {
    matches!(entry, Entry::Tool(t) if parse(&t.call, &[]).is_some_and(|c| c.op == Op::Wait))
}

/// What a finished `wait` learned, from the first line of its reply.
pub fn woke(call: &ToolCall) -> &'static str {
    match super::transcript::output(call).lines().next() {
        Some(line) if line.starts_with("Still going") => "still going",
        Some(_) => "woke",
        None => "",
    }
}

/// A short stand-in for a task the window has no record of.
pub fn short(id: &str) -> String {
    let head: String = id.chars().take(8).collect();
    format!("task {head}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_acp::{ToolContent, ToolKind};
    use serde_json::json;

    fn call(title: &str, input: serde_json::Value, status: ToolStatus, output: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            title: title.into(),
            kind: ToolKind::Execute,
            status,
            input: Some(input),
            content: if output.is_empty() { vec![] } else { vec![ToolContent::Text(output.into())] },
            locations: vec![],
        }
    }

    #[test]
    fn a_create_names_the_task_its_reply_created() {
        let c = call(
            "deck create git:abc",
            json!({ "operation": "create", "project": "git:abc", "brief": "fix it", "title": "Fix it" }),
            ToolStatus::Done,
            "Created task 1a2b-3c in git:abc; its brief is its first prompt. Call deck wait with id 1a2b-3c to see that turn end.",
        );
        let parsed = parse(&c, &[]).unwrap();
        assert_eq!(parsed.op, Op::Create);
        assert_eq!(parsed.ids, vec!["1a2b-3c".to_string()]);
        assert_eq!(parsed.title.as_deref(), Some("Fix it"));
        let running = call("deck create git:abc", json!({ "operation": "create" }), ToolStatus::Running, "");
        assert_eq!(parse(&running, &[]).unwrap().ids, Vec::<String>::new(), "no id before the reply");
    }

    #[test]
    fn prompts_and_listings_name_their_tasks_once() {
        let prompt = call(
            "deck prompt t1",
            json!({ "operation": "prompt", "id": "t1", "text": "go" }),
            ToolStatus::Done,
            "Task t1 began turn 2.",
        );
        assert_eq!(parse(&prompt, &["t1"]).unwrap().ids, vec!["t1".to_string()]);
        let tasks = call(
            "deck tasks",
            json!({ "operation": "tasks" }),
            ToolStatus::Done,
            "2 tasks:\nt1 [git:a] running\nt2 [git:a] stopped",
        );
        let parsed = parse(&tasks, &["t2", "t1", "t9"]).unwrap();
        assert!(parsed.op.folds());
        assert_eq!(parsed.ids, vec!["t2".to_string(), "t1".to_string()]);
    }

    #[test]
    fn anything_else_stays_an_ordinary_card() {
        let failed = call("deck create git:a", json!({ "operation": "create" }), ToolStatus::Failed, "cap");
        assert!(parse(&failed, &[]).is_none(), "a refusal is read in full");
        let other = call("shell ls", json!({ "operation": "create" }), ToolStatus::Done, "");
        assert!(parse(&other, &[]).is_none(), "another tool with an operation field");
        let unknown = call("deck destroy t1", json!({ "operation": "destroy", "id": "t1" }), ToolStatus::Done, "");
        assert!(parse(&unknown, &[]).is_none(), "outside the closed set");
        let raw = ToolCall { input: None, ..call("deck wait t1", json!({}), ToolStatus::Done, "") };
        assert!(parse(&raw, &[]).is_none(), "no arguments to read");
    }

    #[test]
    fn a_wait_says_whether_it_woke() {
        let woke_up = call("deck wait t1", json!({ "operation": "wait", "id": "t1" }), ToolStatus::Done, "Woke:\nt1");
        assert_eq!(woke(&woke_up), "woke");
        let still = call(
            "deck wait t1",
            json!({ "operation": "wait", "id": "t1" }),
            ToolStatus::Done,
            "Still going after 120000 ms; wait again or do something else.\nt1",
        );
        assert_eq!(woke(&still), "still going");
        let entry =
            Entry::Tool(super::super::session::Tool { call: still, timing: super::super::session::Timing::Unknown });
        assert!(is_wait(&entry));
    }
}
