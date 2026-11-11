//! The transcript as rows to draw, and what each tool card shows: an edit's
//! diffs, a command and its output's tail, how long a call ran.
//!
//! The design choice: rows are derived from session entries on demand, and
//! each row has a signature (a cheap hash of what it draws), so the view's
//! virtualized list re-measures only from the first row whose signature
//! changed, which while streaming is the last one. Signatures are checked
//! every render, so no code path that changes the session can forget to. Runs of reads and
//! searches fold into one row. When an agent sends no ACP `diff`, an edit's
//! own arguments (Construct's `edit` and `write`) still give a preview.

use super::diff::{Diff, diff};
use super::session::{Entry, Timing, Tool};
use deck_acp::{ToolCall, ToolContent, ToolKind};
use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    One(usize),
    /// Two or more consecutive read or search calls.
    Reads(Range<usize>),
    /// The footer of a finished turn that changed files: the turn's entries.
    Summary(Range<usize>),
}

impl Row {
    /// The key a row is expanded under.
    pub fn key(&self, entries: &[Entry]) -> String {
        match self {
            Row::One(i) => match &entries[*i] {
                Entry::Tool(t) => format!("tool:{}", t.call.id),
                _ => format!("entry:{i}"),
            },
            Row::Reads(r) => format!("reads:{}", r.start),
            Row::Summary(r) => format!("turn:{}", r.start),
        }
    }

    /// The entries this row draws.
    pub fn range(&self) -> Range<usize> {
        match self {
            Row::One(i) => *i..*i + 1,
            Row::Reads(r) | Row::Summary(r) => r.clone(),
        }
    }
}

fn lookup(entry: &Entry) -> bool {
    matches!(entry, Entry::Tool(t) if matches!(t.call.kind, ToolKind::Read | ToolKind::Search))
}

/// The rows to draw. A turn runs from one prompt to the next; a finished
/// turn (one a later prompt follows, or the last when the agent is `idle`)
/// that changed files ends with a [`Row::Summary`].
pub fn rows(entries: &[Entry], idle: bool) -> Vec<Row> {
    let mut rows = vec![];
    let mut turn = 0;
    let close = |rows: &mut Vec<Row>, range: Range<usize>| {
        if !touched(&entries[range.clone()]).is_empty() {
            rows.push(Row::Summary(range));
        }
    };
    let mut i = 0;
    while i < entries.len() {
        if matches!(entries[i], Entry::You(_)) && i > turn {
            close(&mut rows, turn..i);
            turn = i;
        }
        let run = entries[i..].iter().take_while(|e| lookup(e)).count();
        if run >= 2 {
            rows.push(Row::Reads(i..i + run));
            i += run;
        } else {
            rows.push(Row::One(i));
            i += 1;
        }
    }
    if idle && turn < entries.len() {
        close(&mut rows, turn..entries.len());
    }
    rows
}

/// A file a turn changed: lines added and removed over its successful
/// edits, and the entry of the last edit that touched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Touch {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub entry: usize,
}

/// The files `entries` changed, in the order first touched; indices are
/// relative to the slice. A failed edit changed nothing.
pub fn touched(entries: &[Entry]) -> Vec<Touch> {
    let mut out: Vec<Touch> = vec![];
    for (i, e) in entries.iter().enumerate() {
        let Entry::Tool(t) = e else { continue };
        if t.call.status != deck_acp::ToolStatus::Done {
            continue;
        }
        for c in changes(&t.call) {
            match out.iter_mut().find(|x| x.path == c.path) {
                Some(x) => {
                    x.added += c.diff.added;
                    x.removed += c.diff.removed;
                    x.entry = i;
                }
                None => out.push(Touch { path: c.path, added: c.diff.added, removed: c.diff.removed, entry: i }),
            }
        }
    }
    out
}

/// The rows that open and fold (tool cards, folded reads, thoughts): the
/// ones keyboard selection steps through.
pub fn expandable(entries: &[Entry], rows: &[Row]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| match r {
            Row::Reads(_) => true,
            Row::One(i) => matches!(entries[*i], Entry::Tool(_) | Entry::Thought(_)),
            Row::Summary(_) => false,
        })
        .map(|(i, _)| i)
        .collect()
}

/// The next selectable row from `current` (`up` toward the start); with
/// nothing selected, the last one. Stays put at either end.
pub fn step(selectable: &[usize], current: Option<usize>, up: bool) -> Option<usize> {
    let Some(cur) = current else { return selectable.last().copied() };
    let found = if up { selectable.iter().rev().find(|&&r| r < cur) } else { selectable.iter().find(|&&r| r > cur) };
    found.copied().or(Some(cur))
}

/// The row that draws entry `entry`.
pub fn row_of(rows: &[Row], entry: usize) -> Option<usize> {
    rows.iter().position(|r| !matches!(r, Row::Summary(_)) && r.range().contains(&entry))
}

/// A hash of everything `row` draws, so a changed row is re-measured.
pub fn signatures(entries: &[Entry], rows: &[Row], expanded: &HashSet<String>) -> Vec<u64> {
    rows.iter()
        .map(|row| {
            let mut h = DefaultHasher::new();
            expanded.contains(&row.key(entries)).hash(&mut h);
            let range = row.range();
            (matches!(row, Row::Summary(_)), &range).hash(&mut h);
            for e in &entries[range] {
                shape(e, &mut h);
            }
            h.finish()
        })
        .collect()
}

/// What changes an entry's drawing, cheaply: text only grows while it
/// streams, so its length stands for it; a tool's status, content and
/// timing are hashed by kind and size, never by walking its text.
fn shape(entry: &Entry, h: &mut DefaultHasher) {
    match entry {
        Entry::You(t) => (0u8, t.len()).hash(h),
        Entry::Agent(t) => (1u8, t.len()).hash(h),
        Entry::Thought(t) => (2u8, t.len()).hash(h),
        Entry::Note(t) => (3u8, t.len()).hash(h),
        Entry::Tool(t) => {
            let c = &t.call;
            (4u8, &c.id, &c.title, c.input.is_some(), c.locations.len()).hash(h);
            (format!("{:?}{:?}", c.kind, c.status), matches!(t.timing, Timing::Took(_))).hash(h);
            for part in &c.content {
                match part {
                    ToolContent::Text(x) => (0u8, x.len()).hash(h),
                    ToolContent::Diff { path, old, new } => {
                        (1u8, path, old.as_ref().map(String::len), new.len()).hash(h)
                    }
                }
            }
        }
    }
}

/// The first index where two signature lists differ (the shorter length if
/// one is a prefix of the other).
pub fn first_change(old: &[u64], new: &[u64]) -> usize {
    old.iter().zip(new).take_while(|(a, b)| a == b).count()
}

/// One file change a card shows. `numbered` is false when the diff was
/// built from an edit's arguments, whose line numbers are the snippet's.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub path: String,
    pub diff: Diff,
    pub numbered: bool,
    /// The texts the diff was built from; its line numbers index them, so
    /// the view can highlight each whole and keep multi-line context.
    pub old: String,
    pub new: String,
}

impl Change {
    fn of(path: String, old: &str, new: &str, numbered: bool) -> Self {
        Change { path, diff: diff(old, new), numbered, old: old.to_string(), new: new.to_string() }
    }
}

pub fn changes(call: &ToolCall) -> Vec<Change> {
    let sent: Vec<Change> = call
        .content
        .iter()
        .filter_map(|c| match c {
            ToolContent::Diff { path, old, new } => {
                Some(Change::of(path.clone(), old.as_deref().unwrap_or_default(), new, true))
            }
            ToolContent::Text(_) => None,
        })
        .collect();
    if !sent.is_empty() || call.kind != ToolKind::Edit {
        return sent;
    }
    let Some(input) = &call.input else { return vec![] };
    let s = |k: &str| input.get(k).and_then(|v| v.as_str());
    let path = s("path").or(s("file_path")).unwrap_or_default().to_string();
    match (s("old_string"), s("new_string"), s("content")) {
        (Some(old), Some(new), _) => vec![Change::of(path, old, new, false)],
        (_, _, Some(content)) => vec![Change::of(path, "", content, false)],
        _ => vec![],
    }
}

/// The command an execute call ran, from its arguments.
pub fn command(call: &ToolCall) -> Option<String> {
    let v = call.input.as_ref()?.get("command")?;
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(a) => Some(a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" ")),
        _ => None,
    }
}

/// `text` as one line: each line trimmed, blanks dropped, lines joined by a
/// visible break. A card header is a fixed row, and a hard newline always
/// makes another text line whatever the white-space style.
pub fn oneline(text: &str) -> String {
    text.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>().join(" ⏎ ")
}

/// The call's text output, joined.
pub fn output(call: &ToolCall) -> String {
    call.content
        .iter()
        .filter_map(|c| match c {
            ToolContent::Text(t) => Some(t.trim_end_matches('\n')),
            ToolContent::Diff { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The last `n` lines of `text` (trailing newline ignored) and how many came before.
pub fn tail(text: &str, n: usize) -> (Vec<&str>, usize) {
    let lines: Vec<&str> = text.trim_end_matches('\n').lines().collect();
    let hidden = lines.len().saturating_sub(n);
    (lines[hidden..].to_vec(), hidden)
}

/// `350ms`, `4.2s`, `2m 05s`; nothing under 100ms, which is noise on a card.
pub fn took(tool: &Tool) -> Option<String> {
    let Timing::Took(d) = tool.timing else { return None };
    let ms = d.as_millis();
    Some(match ms {
        0..100 => return None,
        100..1_000 => format!("{ms}ms"),
        1_000..60_000 => format!("{:.1}s", d.as_secs_f32()),
        _ => format!("{}m {:02}s", ms / 60_000, (ms / 1_000) % 60),
    })
}

/// A path's last component, for a chip.
pub fn file_name(path: &str) -> &str {
    path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path)
}

/// The paths a read or search touched: its locations, else its `path` argument, else its title.
pub fn paths(call: &ToolCall) -> Vec<String> {
    if !call.locations.is_empty() {
        return call.locations.iter().map(|l| l.path.clone()).collect();
    }
    let arg = call.input.as_ref().and_then(|i| i.get("path").or(i.get("pattern"))).and_then(|v| v.as_str());
    vec![arg.map_or_else(|| call.title.clone(), str::to_string)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_acp::{Location, ToolStatus};
    use serde_json::json;
    use std::time::Duration;

    fn tool(id: &str, kind: ToolKind) -> Entry {
        Entry::Tool(Tool {
            call: ToolCall { id: id.into(), kind, status: ToolStatus::Done, ..ToolCall::default() },
            timing: Timing::Unknown,
        })
    }

    #[test]
    fn runs_of_reads_fold_into_one_row() {
        let e = vec![
            Entry::You("go".into()),
            tool("r1", ToolKind::Read),
            tool("s1", ToolKind::Search),
            tool("r2", ToolKind::Read),
            tool("e1", ToolKind::Edit),
            tool("r3", ToolKind::Read),
            Entry::Agent("done".into()),
        ];
        assert_eq!(rows(&e, false), vec![Row::One(0), Row::Reads(1..4), Row::One(4), Row::One(5), Row::One(6)]);
        assert_eq!(Row::One(4).key(&e), "tool:e1");
    }

    #[test]
    fn a_finished_turn_that_changed_files_gets_a_summary() {
        let edit = |id: &str, path: &str, status: ToolStatus| {
            Entry::Tool(Tool {
                call: ToolCall {
                    id: id.into(),
                    kind: ToolKind::Edit,
                    status,
                    content: vec![ToolContent::Diff {
                        path: path.into(),
                        old: Some("a\n".into()),
                        new: "b\nc\n".into(),
                    }],
                    ..ToolCall::default()
                },
                timing: Timing::Unknown,
            })
        };
        let e = vec![
            Entry::You("one".into()),
            edit("e1", "a.rs", ToolStatus::Done),
            edit("e2", "b.rs", ToolStatus::Failed),
            edit("e3", "a.rs", ToolStatus::Done),
            Entry::You("two".into()),
            edit("e4", "c.rs", ToolStatus::Done),
        ];
        let working = rows(&e, false);
        assert_eq!(working[4], Row::Summary(0..4), "the first turn is over once another prompt follows");
        assert!(!working.contains(&Row::Summary(4..6)), "the live turn has no footer yet");
        assert_eq!(rows(&e, true).last(), Some(&Row::Summary(4..6)));
        assert_eq!(
            touched(&e[0..4]),
            vec![Touch { path: "a.rs".into(), added: 4, removed: 2, entry: 3 }],
            "a failed edit changed nothing, a file edited twice is one line"
        );
        assert_eq!(row_of(&working, 3), Some(3));
    }

    #[test]
    fn selection_steps_through_expandable_rows_and_stops_at_the_ends() {
        let e = vec![
            Entry::You("go".into()),
            tool("r1", ToolKind::Read),
            tool("r2", ToolKind::Read),
            Entry::Thought("hm".into()),
            Entry::Agent("a".into()),
            tool("x1", ToolKind::Execute),
        ];
        let rows = rows(&e, false);
        let sel = expandable(&e, &rows);
        assert_eq!(sel, vec![1, 2, 4]);
        assert_eq!(step(&sel, None, true), Some(4), "the first press picks the latest card");
        assert_eq!(step(&sel, Some(4), true), Some(2));
        assert_eq!(step(&sel, Some(1), true), Some(1));
        assert_eq!(step(&sel, Some(4), false), Some(4));
    }

    #[test]
    fn streaming_changes_only_the_last_signature_and_expanding_changes_its_row() {
        let mut e = vec![Entry::You("go".into()), tool("e1", ToolKind::Edit), Entry::Agent("a".into())];
        let none = HashSet::new();
        let before = signatures(&e, &rows(&e, false), &none);
        e[2] = Entry::Agent("ab".into());
        let after = signatures(&e, &rows(&e, false), &none);
        assert_eq!(first_change(&before, &after), 2);
        let open = HashSet::from(["tool:e1".to_string()]);
        assert_eq!(first_change(&after, &signatures(&e, &rows(&e, false), &open)), 1);
        assert_eq!(first_change(&after, &after[..2]), 2);
    }

    #[test]
    fn an_edit_shows_the_sent_diff_else_its_arguments() {
        let sent = ToolCall {
            kind: ToolKind::Edit,
            content: vec![ToolContent::Diff { path: "a.rs".into(), old: Some("x\n".into()), new: "y\n".into() }],
            ..ToolCall::default()
        };
        let [c] = &changes(&sent)[..] else { panic!() };
        assert!(c.numbered && c.path == "a.rs" && (c.diff.added, c.diff.removed) == (1, 1));

        let edit = ToolCall {
            kind: ToolKind::Edit,
            input: Some(json!({ "path": "b.rs", "old_string": "keep\nold", "new_string": "keep\nnew\nmore" })),
            ..ToolCall::default()
        };
        let [c] = &changes(&edit)[..] else { panic!() };
        assert!(!c.numbered && c.path == "b.rs" && (c.diff.added, c.diff.removed) == (2, 1));

        let write = ToolCall {
            kind: ToolKind::Edit,
            input: Some(json!({ "path": "n.md", "content": "a\nb\n" })),
            ..ToolCall::default()
        };
        assert_eq!(changes(&write)[0].diff.added, 2);
        assert!(changes(&ToolCall { kind: ToolKind::Execute, ..edit }).is_empty());
    }

    #[test]
    fn commands_outputs_tails_and_durations() {
        let call = ToolCall {
            input: Some(json!({ "command": ["git", "push"] })),
            content: vec![ToolContent::Text("1\n2\n3\n".into()), ToolContent::Text("4".into())],
            locations: vec![Location { path: "/w/src/lib.rs".into(), line: None }],
            ..ToolCall::default()
        };
        assert_eq!(command(&call).as_deref(), Some("git push"));
        assert_eq!(tail(&output(&call), 2), (vec!["3", "4"], 2));
        assert_eq!(paths(&call), ["/w/src/lib.rs"]);
        assert_eq!(file_name("/w/src/lib.rs"), "lib.rs");
        let t = |ms| took(&Tool { call: ToolCall::default(), timing: Timing::Took(Duration::from_millis(ms)) });
        assert_eq!((t(350), t(4_200), t(125_000)), (Some("350ms".into()), Some("4.2s".into()), Some("2m 05s".into())));
        assert_eq!(t(40), None);
        assert_eq!(oneline("cargo test"), "cargo test");
        assert_eq!(
            oneline("python3 - <<'PY'\n  import json\n\n  print(1)\nPY\n"),
            "python3 - <<'PY' ⏎ import json ⏎ print(1) ⏎ PY",
            "each line trimmed, blanks dropped, breaks visible"
        );
        assert_eq!(oneline("  \n\n "), "");
    }
}
