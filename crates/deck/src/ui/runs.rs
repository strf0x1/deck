//! The text each transcript row draws, as the runs a selection indexes
//! (ADR 0013): a message, each markdown paragraph and code block, a
//! command, an output tail, each diff line.
//!
//! The design choice: this is the one list of what is selectable, read
//! from the session alone, so copy works for rows the virtualized list has
//! not drawn. The renderers (`cards.rs`, `prose.rs`, `card_body.rs`) draw
//! the same runs in the same order through `selectable.rs`, and a view test
//! checks the two agree. Only what is shown is a run: a folded card's
//! hidden lines, headers, chips and diff gutters are not.

use super::cards::{CAP, PREVIEW, TAIL};
use super::diff::Row as DiffRow;
use super::markdown::{Block, parse};
use super::selection::Run;
use super::session::Entry;
use super::transcript::{self, Row};
use deck_acp::{ToolCall, ToolKind};

/// What separates two prose blocks, and two lines of machine text.
pub const PARAGRAPH: &str = "\n\n";
pub const LINE: &str = "\n";

/// The runs `row` draws, `open` when it is expanded.
pub fn runs(entries: &[Entry], row: &Row, open: bool) -> Vec<Run> {
    let Row::One(i) = row else { return vec![] };
    match &entries[*i] {
        Entry::You(t) | Entry::Note(t) => vec![run(t, PARAGRAPH)],
        Entry::Agent(t) => prose(t),
        Entry::Thought(t) if open => vec![run(t, PARAGRAPH)],
        Entry::Thought(_) => vec![],
        Entry::Tool(tool) => tool_runs(&tool.call, open),
    }
}

/// An agent message's runs: its paragraphs, headings and code blocks in
/// reading order, quotes and list items included.
pub fn prose(text: &str) -> Vec<Run> {
    fn walk(blocks: &[Block], out: &mut Vec<Run>) {
        for b in blocks {
            match b {
                Block::Paragraph(i) | Block::Heading(_, i) => out.push(run(&i.text, PARAGRAPH)),
                Block::Code { text, .. } => out.push(run(text, PARAGRAPH)),
                Block::Quote(inner) => walk(inner, out),
                Block::List { items, .. } => items.iter().for_each(|item| walk(item, out)),
                Block::Rule => {}
                Block::Table { rows } => {
                    for (r, row) in rows.iter().enumerate() {
                        for (c, cell) in row.iter().enumerate() {
                            out.push(run(
                                &cell.text,
                                if c > 0 {
                                    "\t"
                                } else if r > 0 {
                                    LINE
                                } else {
                                    PARAGRAPH
                                },
                            ));
                        }
                    }
                }
            }
        }
    }
    let mut out = vec![];
    walk(&parse(text), &mut out);
    out
}

fn tool_runs(call: &ToolCall, open: bool) -> Vec<Run> {
    // A `deck` line shows its reply only when a folding read is open.
    if let Some(deck) = super::deck_tool::parse(call, &[]) {
        if !(open && deck.op.folds()) {
            return vec![];
        }
        return output_text(&transcript::output(call), true).map(|t| run(&t, LINE)).into_iter().collect();
    }
    let changes = transcript::changes(call);
    match call.kind {
        ToolKind::Execute => {
            let command = open.then(|| transcript::command(call)).flatten().map(|c| run(command_text(&c), LINE));
            command.into_iter().chain(output_text(&transcript::output(call), open).map(|t| run(&t, LINE))).collect()
        }
        _ if !changes.is_empty() => changes
            .iter()
            .flat_map(|c| c.diff.preview(2, if open { CAP } else { PREVIEW }).0)
            .filter_map(|r| match r {
                DiffRow::Line(l) => Some(run(&l.text, LINE)),
                DiffRow::Gap(_) => None,
            })
            .collect(),
        _ if open => output_text(&transcript::output(call), true).map(|t| run(&t, LINE)).into_iter().collect(),
        _ => vec![],
    }
}

/// An opened execute card's command as drawn.
pub fn command_text(command: &str) -> &str {
    command.trim_end_matches('\n')
}

/// The output tail a card draws, and how many earlier lines it hides; none
/// for blank output.
pub fn output_tail(text: &str, open: bool) -> Option<(String, usize)> {
    if text.trim().is_empty() {
        return None;
    }
    let (lines, hidden) = transcript::tail(text, if open { CAP } else { TAIL });
    Some((lines.join("\n"), hidden))
}

fn output_text(text: &str, open: bool) -> Option<String> {
    output_tail(text, open).map(|(t, _)| t)
}

fn run(text: &str, joint: &'static str) -> Run {
    Run { text: text.to_string(), joint }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(runs: Vec<Run>) -> Vec<String> {
        runs.into_iter().map(|r| r.text).collect()
    }

    #[test]
    fn prose_lists_blocks_in_reading_order() {
        let md = "# Title\n\nSome **bold** text.\n\n- one\n- two\n\n> quoted\n\n```rust\nfn x() {}\n```\n\n---\n";
        assert_eq!(texts(prose(md)), ["Title", "Some bold text.", "one", "two", "quoted", "fn x() {}"]);
    }

    #[test]
    fn copying_tables_keeps_empty_cells_and_block_separators() {
        use super::super::selection::{Selection, Unit, unit_at};
        let md = "Before\n\n| | B | C |\n|---|---|---|\n| x | | |\n\nAfter";
        let listed = prose(md);
        let start = unit_at("row", 0, "Before", 0, Unit::Char);
        let end = unit_at("row", 7, "After", 5, Unit::Char);
        let mut selection = Selection::new(start, Unit::Char);
        selection.extend(end);
        let copied = selection.span(&["row".into()]).unwrap().text(|_| listed.clone());
        assert_eq!(copied, "Before\n\n\tB\tC\nx\t\t\n\nAfter");
    }

    #[test]
    fn a_thought_is_selectable_only_when_open() {
        let entries = vec![Entry::Thought("why".into())];
        assert!(runs(&entries, &Row::One(0), false).is_empty());
        assert_eq!(texts(runs(&entries, &Row::One(0), true)), ["why"]);
    }

    #[test]
    fn folds_and_footers_draw_no_runs() {
        let entries = vec![Entry::You("a".into()), Entry::You("b".into())];
        assert!(runs(&entries, &Row::Reads(0..2), true).is_empty());
        assert!(runs(&entries, &Row::Summary(0..2), true).is_empty());
    }

    #[test]
    fn an_output_tail_is_one_run_of_its_last_lines() {
        let text = (1..=9).map(|n| n.to_string()).collect::<Vec<_>>().join("\n");
        assert_eq!(output_tail(&text, false), Some(("5\n6\n7\n8\n9".to_string(), 4)));
        assert_eq!(output_tail(" \n", false), None);
    }
}
