//! The fake's `deck` tool: a prompt `deck {"operation": …}` makes one call in
//! construct#222's shape over Deck's control surface (contract 1.4.0 §13),
//! so a window can show a coordinator's calls and a manual run can create and
//! prompt workers without a model.
//!
//! The design choice: the fake mirrors what the window reads and nothing
//! more. The call is titled `deck <operation> <id or project>` with the
//! arguments as `rawInput`, kind `execute`; it is sent as is to
//! `$DECK_CONTROL_URL/control` with `DECK_CONTROL_TOKEN` as the bearer, and
//! the reply becomes the call's text the way Construct summarizes it (a
//! `create` says "Created task <id> in <project>"). A refusal, or no surface
//! at all, is a failed call carrying the reason.

use super::github::Gateway;
use serde_json::Value;

/// The call a prompt scripts: its title, arguments and outcome text.
pub struct Scripted {
    pub title: String,
    pub input: Value,
    pub result: Result<String, String>,
}

/// The `deck` call `prompt` asks for, if it is one.
pub fn script(prompt: &str) -> Option<Scripted> {
    let input: Value = serde_json::from_str(prompt.trim().strip_prefix("deck ")?).ok()?;
    let op = input.get("operation")?.as_str()?.to_string();
    let target = input.get("id").or_else(|| input.get("project")).and_then(Value::as_str).unwrap_or_default();
    let title = format!("deck {op} {target}").trim_end().to_string();
    let result = send(&input).map(|reply| summary(&op, &input, &reply));
    Some(Scripted { title, input, result })
}

fn send(input: &Value) -> Result<Value, String> {
    let url = std::env::var("DECK_CONTROL_URL").map_err(|_| "this agent has no Deck control surface".to_string())?;
    let token = std::env::var("DECK_CONTROL_TOKEN").ok();
    Gateway::new(&url).exchange_as("/control", input.to_string().as_bytes(), token.as_deref())
}

/// The reply as text, in the words Construct's tool uses (construct#222).
fn summary(op: &str, input: &Value, reply: &Value) -> String {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let line = |task: &Value| format!("{} [{}] {}", s(task, "id"), s(task, "project"), s(task, "state"));
    match op {
        "create" => format!(
            "Created task {} in {}; its brief is its first prompt. Call deck wait with id {0} to see that turn end.",
            s(reply, "id"),
            s(input, "project")
        ),
        "prompt" => {
            format!("Task {} began turn {}.", s(input, "id"), reply.get("turn").and_then(Value::as_u64).unwrap_or(0))
        }
        "wait" if reply.get("timed_out").and_then(Value::as_bool) == Some(true) => {
            format!("Still going; wait again or do something else.\n{}", line(&reply["task"]))
        }
        "wait" => format!("Woke:\n{}", line(&reply["task"])),
        "task" => line(&reply["task"]),
        "tasks" => {
            let tasks = reply.get("tasks").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut out = format!("{} task{}:", tasks.len(), if tasks.len() == 1 { "" } else { "s" });
            for task in &tasks {
                out.push('\n');
                out.push_str(&line(task));
            }
            out
        }
        _ => format!("Task {} is {}.", s(input, "id"), s(reply, "state")),
    }
}
