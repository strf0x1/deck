//! A tool call as ACP describes it: what kind of work it is, what it was
//! given, what it produced and which files it touched. Views pick a card by
//! [`ToolKind`] and draw diffs and paths from here, never from raw JSON.
//!
//! The design choice: an update is a patch. ACP's `tool_call_update` may
//! carry any subset of a call's fields, and a field it carries replaces the
//! old value whole (a new `content` list is the whole list), so
//! [`ToolUpdate`] holds each field as an `Option` and [`ToolCall::apply`]
//! overwrites only what is present.

use serde_json::Value;

/// A tool call and everything the agent has said about it so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolStatus,
    /// ACP's `rawInput`: the arguments, as the agent sent them.
    pub input: Option<Value>,
    pub content: Vec<ToolContent>,
    pub locations: Vec<Location>,
}

/// The fields a `tool_call_update` carried; `None` leaves a field as it was.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolUpdate {
    pub id: String,
    pub title: Option<String>,
    pub kind: Option<ToolKind>,
    pub status: Option<ToolStatus>,
    pub input: Option<Value>,
    pub content: Option<Vec<ToolContent>>,
    pub locations: Option<Vec<Location>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolStatus {
    #[default]
    Running,
    Done,
    Failed,
}

/// ACP's tool kinds; anything else, or none, is [`ToolKind::Other`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    #[default]
    Other,
}

/// What a tool call produced. Content blocks other than text (images,
/// resources) and terminals are not shown yet and are dropped here.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolContent {
    Text(String),
    /// A file change. `old` is `None` for a new file.
    Diff {
        path: String,
        old: Option<String>,
        new: String,
    },
}

/// A file the call is working on, with a line when the agent names one.
#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    pub path: String,
    pub line: Option<u64>,
}

impl ToolCall {
    /// Overwrite the fields `update` carried.
    pub fn apply(&mut self, update: ToolUpdate) {
        if let Some(title) = update.title {
            self.title = title;
        }
        if let Some(kind) = update.kind {
            self.kind = kind;
        }
        if let Some(status) = update.status {
            self.status = status;
        }
        if update.input.is_some() {
            self.input = update.input;
        }
        if let Some(content) = update.content {
            self.content = content;
        }
        if let Some(locations) = update.locations {
            self.locations = locations;
        }
    }
}

/// A `tool_call` payload. Missing fields take ACP's defaults: `pending`
/// (shown as running) and kind `other`.
pub(crate) fn call(u: &Value) -> ToolCall {
    ToolCall {
        id: u["toolCallId"].as_str().unwrap_or_default().to_string(),
        title: u["title"].as_str().unwrap_or("tool").to_string(),
        kind: u["kind"].as_str().map(kind).unwrap_or_default(),
        status: status(u["status"].as_str().unwrap_or("pending")),
        input: raw_input(u),
        content: u.get("content").map(content).unwrap_or_default(),
        locations: u.get("locations").map(locations).unwrap_or_default(),
    }
}

/// A `tool_call_update` payload.
pub(crate) fn update(u: &Value) -> ToolUpdate {
    ToolUpdate {
        id: u["toolCallId"].as_str().unwrap_or_default().to_string(),
        title: u["title"].as_str().map(str::to_string),
        kind: u["kind"].as_str().map(kind),
        status: u["status"].as_str().map(status),
        input: raw_input(u),
        content: u.get("content").filter(|v| !v.is_null()).map(content),
        locations: u.get("locations").filter(|v| !v.is_null()).map(locations),
    }
}

fn raw_input(u: &Value) -> Option<Value> {
    u.get("rawInput").filter(|v| !v.is_null()).cloned()
}

fn status(status: &str) -> ToolStatus {
    match status {
        "completed" => ToolStatus::Done,
        "failed" => ToolStatus::Failed,
        _ => ToolStatus::Running,
    }
}

fn kind(kind: &str) -> ToolKind {
    match kind {
        "read" => ToolKind::Read,
        "edit" => ToolKind::Edit,
        "delete" => ToolKind::Delete,
        "move" => ToolKind::Move,
        "search" => ToolKind::Search,
        "execute" => ToolKind::Execute,
        "think" => ToolKind::Think,
        "fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

fn content(v: &Value) -> Vec<ToolContent> {
    let Some(items) = v.as_array() else { return vec![] };
    items
        .iter()
        .filter_map(|item| match item["type"].as_str()? {
            "content" if item["content"]["type"] == "text" => {
                Some(ToolContent::Text(item["content"]["text"].as_str()?.to_string()))
            }
            "diff" => Some(ToolContent::Diff {
                path: item["path"].as_str()?.to_string(),
                old: item["oldText"].as_str().map(str::to_string),
                new: item["newText"].as_str()?.to_string(),
            }),
            _ => None,
        })
        .collect()
}

fn locations(v: &Value) -> Vec<Location> {
    let Some(items) = v.as_array() else { return vec![] };
    items
        .iter()
        .filter_map(|item| Some(Location { path: item["path"].as_str()?.to_string(), line: item["line"].as_u64() }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_call_keeps_kind_input_content_and_locations() {
        let c = call(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "e1", "title": "edit src/lib.rs", "kind": "edit",
            "status": "in_progress", "rawInput": { "path": "src/lib.rs" },
            "content": [
                { "type": "diff", "path": "src/lib.rs", "oldText": "a\n", "newText": "b\n" },
                { "type": "diff", "path": "NEW.md", "oldText": null, "newText": "hi\n" },
                { "type": "content", "content": { "type": "text", "text": "ok" } },
                { "type": "content", "content": { "type": "image", "data": "", "mimeType": "image/png" } },
                { "type": "terminal", "terminalId": "term-1" },
            ],
            "locations": [{ "path": "src/lib.rs", "line": 4 }, { "path": "NEW.md" }, { "line": 9 }],
        }));
        assert_eq!(c.kind, ToolKind::Edit);
        assert_eq!(c.status, ToolStatus::Running);
        assert_eq!(c.input, Some(json!({ "path": "src/lib.rs" })));
        assert_eq!(
            c.content,
            vec![
                ToolContent::Diff { path: "src/lib.rs".into(), old: Some("a\n".into()), new: "b\n".into() },
                ToolContent::Diff { path: "NEW.md".into(), old: None, new: "hi\n".into() },
                ToolContent::Text("ok".into()),
            ]
        );
        assert_eq!(
            c.locations,
            vec![Location { path: "src/lib.rs".into(), line: Some(4) }, Location { path: "NEW.md".into(), line: None },]
        );
    }

    #[test]
    fn a_bare_call_takes_acp_defaults() {
        let c = call(&json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "kind": "teleport" }));
        assert_eq!(c, ToolCall { id: "t".into(), title: "tool".into(), kind: ToolKind::Other, ..ToolCall::default() });
    }

    #[test]
    fn an_update_replaces_only_the_fields_it_carries() {
        let mut c = call(&json!({
            "toolCallId": "x", "title": "shell ls", "kind": "execute", "status": "in_progress",
            "rawInput": { "command": "ls" }, "locations": [{ "path": "." }],
        }));
        c.apply(update(&json!({
            "toolCallId": "x", "status": "completed",
            "content": [{ "type": "content", "content": { "type": "text", "text": "a\nb" } }],
        })));
        assert_eq!(c.title, "shell ls");
        assert_eq!(c.kind, ToolKind::Execute);
        assert_eq!(c.status, ToolStatus::Done);
        assert_eq!(c.input, Some(json!({ "command": "ls" })));
        assert_eq!(c.content, vec![ToolContent::Text("a\nb".into())]);
        assert_eq!(c.locations, vec![Location { path: ".".into(), line: None }]);

        c.apply(update(&json!({ "toolCallId": "x", "content": [], "locations": null })));
        assert!(c.content.is_empty(), "a new list replaces the old one whole");
        assert_eq!(c.locations.len(), 1, "null is absent, not empty");
    }
}
