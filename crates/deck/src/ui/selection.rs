// Portions ported from gpui-kit v0.9.1, crates/base/src/text_boundary.rs
// (`word_range_at`, `line_range_at` and the character classes behind them).
// Copyright 2024 - 2026 Longbridge <https://longbridge.com>.
// Licensed under the Apache License, Version 2.0:
// http://www.apache.org/licenses/LICENSE-2.0
// Changed for deck: a word at the end of the text is an empty range rather
// than none, so a double click past the last character selects nothing.

//! Text selected in the transcript, held as positions in the text, never as
//! pixels (ADR 0013).
//!
//! The design choice: a position is a row key, a run (one piece of text the
//! row draws, listed by `runs.rs`) and a byte offset into it, so a selection
//! outlives its rows leaving the virtualized list, and copy reads the
//! session's text rather than drawn elements. Each end is a range, which a
//! double or triple click widens to a word or line; dragging from it keeps
//! that unit selected whichever way it goes.

use std::cmp::Ordering;
use std::ops::Range;

/// A place in the transcript's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pos {
    pub row: String,
    pub run: usize,
    pub offset: usize,
}

/// What a press selects: a point, or the word or line under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Char,
    Word,
    Line,
}

impl Unit {
    /// The unit `n` clicks select.
    pub fn of_clicks(n: usize) -> Self {
        match n {
            0 | 1 => Unit::Char,
            2 => Unit::Word,
            _ => Unit::Line,
        }
    }
}

/// One piece of text a row draws, and what copy puts before it when an
/// earlier run of the same row was copied too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub joint: &'static str,
}

/// What separates two rows in copied text.
const ROW_JOINT: &str = "\n\n";

#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    anchor: (Pos, Pos),
    head: (Pos, Pos),
    unit: Unit,
}

impl Selection {
    /// A selection of `at`, a point or the unit a multi-click covered.
    pub fn new(at: (Pos, Pos), unit: Unit) -> Self {
        Selection { anchor: at.clone(), head: at, unit }
    }

    pub fn unit(&self) -> Unit {
        self.unit
    }

    /// Move the free end to `to`, the unit under the pointer.
    pub fn extend(&mut self, to: (Pos, Pos)) {
        self.head = to;
    }

    /// Start before end, in the order of `rows` (row keys, top to bottom);
    /// none when it covers nothing or one of its rows has gone.
    pub fn span(&self, rows: &[String]) -> Option<Span> {
        let rank = |p: &Pos| rows.iter().position(|k| *k == p.row).map(|ix| (ix, p.run, p.offset));
        let (a0, a1, h0, h1) = (rank(&self.anchor.0)?, rank(&self.anchor.1)?, rank(&self.head.0)?, rank(&self.head.1)?);
        let (start, end) = (h0.min(a0), h1.max(a1));
        (start < end).then_some(Span { start, end })
    }
}

/// A non-empty selection resolved against the current rows: each end as
/// (row index, run, offset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    start: (usize, usize, usize),
    end: (usize, usize, usize),
}

impl Span {
    /// The rows it touches, by index.
    pub fn rows(&self) -> Range<usize> {
        self.start.0..self.end.0 + 1
    }

    /// The bytes of run `run` in row `row` it covers, `text` being that
    /// run's text; offsets past the end or inside a character are clipped.
    pub fn covers(&self, row: usize, run: usize, text: &str) -> Option<Range<usize>> {
        let at = (row, run);
        if at < (self.start.0, self.start.1) || at > (self.end.0, self.end.1) {
            return None;
        }
        let a = if at == (self.start.0, self.start.1) { clip(text, self.start.2) } else { 0 };
        let b = if at == (self.end.0, self.end.1) { clip(text, self.end.2) } else { text.len() };
        (a < b).then_some(a..b)
    }

    /// The covered text, rows joined by a blank line and runs by their
    /// joint; `runs(row)` lists a row's runs.
    pub fn text(&self, mut runs: impl FnMut(usize) -> Vec<Run>) -> String {
        let mut out = String::new();
        for row in self.rows() {
            let mut first_in_row = true;
            for (n, run) in runs(row).iter().enumerate() {
                let Some(range) = self.covers(row, n, &run.text) else { continue };
                if !out.is_empty() {
                    out.push_str(if first_in_row { ROW_JOINT } else { run.joint });
                }
                out.push_str(run.text.get(range).unwrap_or_default());
                first_in_row = false;
            }
        }
        out
    }
}

/// The two ends of what `unit` selects at `offset` in run `run` of `row`.
pub fn unit_at(row: &str, run: usize, text: &str, offset: usize, unit: Unit) -> (Pos, Pos) {
    let range = match unit {
        Unit::Char => clip(text, offset)..clip(text, offset),
        Unit::Word => word_at(text, offset),
        Unit::Line => line_at(text, offset),
    };
    let pos = |offset| Pos { row: row.to_string(), run, offset };
    (pos(range.start), pos(range.end))
}

/// Compare two positions in the order of `rows`; a position whose row has
/// gone sorts last.
pub fn order(rows: &[String], a: &Pos, b: &Pos) -> Ordering {
    let rank = |p: &Pos| (rows.iter().position(|k| *k == p.row).unwrap_or(usize::MAX), p.run, p.offset);
    rank(a).cmp(&rank(b))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Newline,
    Other,
}

fn class(c: char) -> Class {
    if c == '_'
        || c.is_ascii_alphanumeric()
        || matches!(c, '\u{00C0}'..='\u{024F}' | '\u{0400}'..='\u{04FF}' | '\u{1E00}'..='\u{1EFF}' | '\u{0300}'..='\u{036F}')
    {
        Class::Word
    } else if matches!(c, '\n' | '\r') {
        Class::Newline
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// The word, run of spaces, or single other character at `offset`.
pub fn word_at(text: &str, offset: usize) -> Range<usize> {
    let offset = clip(text, offset);
    let Some(c) = text.get(offset..).unwrap_or_default().chars().next() else { return offset..offset };
    let kind = class(c);
    let joins = |next: char| matches!((kind, class(next)), (Class::Word, Class::Word) | (Class::Space, Class::Space));
    let start = text
        .get(..offset)
        .unwrap_or_default()
        .chars()
        .rev()
        .take(128)
        .take_while(|c| joins(*c))
        .fold(offset, |at, c| at - c.len_utf8());
    let end = text
        .get(offset + c.len_utf8()..)
        .unwrap_or_default()
        .chars()
        .take(128)
        .take_while(|c| joins(*c))
        .fold(offset + c.len_utf8(), |at, c| at + c.len_utf8());
    start..end
}

/// The line holding `offset`, without its newline.
pub fn line_at(text: &str, offset: usize) -> Range<usize> {
    let offset = clip(text, offset);
    let start = text.get(..offset).unwrap_or_default().rfind('\n').map_or(0, |n| n + 1);
    let end = text.get(offset..).unwrap_or_default().find('\n').map_or(text.len(), |n| offset + n);
    start..end
}

/// `offset` within `text`, moved back to a character boundary.
fn clip(text: &str, offset: usize) -> usize {
    let mut at = offset.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(row: &str, run: usize, offset: usize) -> Pos {
        Pos { row: row.into(), run, offset }
    }

    fn point(row: &str, run: usize, offset: usize) -> (Pos, Pos) {
        (pos(row, run, offset), pos(row, run, offset))
    }

    fn slice(text: &str, r: Range<usize>) -> &str {
        text.get(r).unwrap()
    }

    fn rows() -> Vec<String> {
        ["a", "b", "c"].map(String::from).to_vec()
    }

    fn run(text: &str) -> Run {
        Run { text: text.into(), joint: "\n" }
    }

    #[test]
    fn a_drag_selects_between_press_and_pointer_whichever_way_it_goes() {
        let mut s = Selection::new(point("b", 0, 3), Unit::Char);
        assert_eq!(s.span(&rows()), None, "a press alone selects nothing");
        s.extend(point("c", 1, 2));
        let down = s.span(&rows()).unwrap();
        s.extend(point("a", 0, 1));
        let up = s.span(&rows()).unwrap();
        assert_eq!(down.covers(1, 0, "hello"), Some(3..5));
        assert_eq!(down.covers(2, 0, "whole"), Some(0..5));
        assert_eq!(down.covers(2, 1, "xyz"), Some(0..2));
        assert_eq!(down.covers(0, 0, "above"), None);
        assert_eq!(up.covers(0, 0, "above"), Some(1..5));
        assert_eq!(up.covers(1, 0, "hello"), Some(0..3));
        assert_eq!(up.covers(2, 0, "below"), None);
    }

    #[test]
    fn a_word_selection_keeps_its_word_when_dragged_backwards() {
        let text = "one two three";
        let mut s = Selection::new(unit_at("b", 0, text, 5, Unit::Word), Unit::Word);
        assert_eq!(s.span(&rows()).unwrap().covers(1, 0, text), Some(4..7));
        s.extend(unit_at("b", 0, text, 1, Unit::Word));
        assert_eq!(s.span(&rows()).unwrap().covers(1, 0, text), Some(0..7), "from 'one' through 'two'");
        s.extend(unit_at("b", 0, text, 10, Unit::Word));
        assert_eq!(s.span(&rows()).unwrap().covers(1, 0, text), Some(4..13));
    }

    #[test]
    fn offsets_past_the_text_or_inside_a_character_are_clipped() {
        let mut s = Selection::new(point("a", 0, 2), Unit::Char);
        s.extend(point("a", 0, 99));
        let span = s.span(&rows()).unwrap();
        // "é" is two bytes at 1..3: offset 2 falls back to 1.
        assert_eq!(span.covers(0, 0, "héllo"), Some(1..6));
        assert_eq!(span.covers(0, 0, "h"), None, "a run now shorter than the start");
    }

    #[test]
    fn a_selection_whose_row_has_gone_covers_nothing() {
        let mut s = Selection::new(point("b", 0, 0), Unit::Char);
        s.extend(point("z", 0, 1));
        assert_eq!(s.span(&rows()), None);
    }

    #[test]
    fn an_end_past_a_rows_last_run_takes_the_whole_row() {
        let mut s = Selection::new(point("a", 0, 0), Unit::Char);
        s.extend(point("a", 7, 0));
        assert_eq!(s.span(&rows()).unwrap().covers(0, 1, "second"), Some(0..6));
    }

    #[test]
    fn copied_text_joins_runs_by_their_joint_and_rows_by_a_blank_line() {
        let mut s = Selection::new(point("a", 0, 2), Unit::Char);
        s.extend(point("c", 0, 3));
        let text = s.span(&rows()).unwrap().text(|row| match row {
            0 => vec![run("first"), Run { text: "para".into(), joint: "\n\n" }],
            1 => vec![run("l1"), run("l2")],
            _ => vec![run("tail end")],
        });
        assert_eq!(text, "rst\n\npara\n\nl1\nl2\n\ntai");
    }

    #[test]
    fn rows_with_no_runs_add_no_blank_lines() {
        let mut s = Selection::new(point("a", 0, 0), Unit::Char);
        s.extend(point("c", 0, 2));
        let text = s.span(&rows()).unwrap().text(|row| if row == 1 { vec![] } else { vec![run("xy")] });
        assert_eq!(text, "xy\n\nxy");
    }

    #[test]
    fn positions_order_by_row_then_run_then_offset() {
        let r = rows();
        assert_eq!(order(&r, &pos("a", 3, 9), &pos("b", 0, 0)), Ordering::Less);
        assert_eq!(order(&r, &pos("b", 1, 0), &pos("b", 0, 9)), Ordering::Greater);
        assert_eq!(order(&r, &pos("gone", 0, 0), &pos("c", 9, 9)), Ordering::Greater);
    }

    #[test]
    fn words_spaces_and_lines() {
        let text = "let x_1 = a.b;\nnext  line";
        assert_eq!(slice(text, word_at(text, 5)), "x_1");
        assert_eq!(slice(text, word_at(text, 10)), "a");
        assert_eq!(slice(text, word_at(text, 11)), ".");
        assert_eq!(slice(text, word_at(text, 19)), "  ");
        assert_eq!(word_at(text, text.len()), text.len()..text.len());
        assert_eq!(slice(text, line_at(text, 3)), "let x_1 = a.b;");
        assert_eq!(slice(text, line_at(text, 17)), "next  line");
        assert_eq!(slice("naïve café", word_at("naïve café", 3)), "naïve");
    }

    #[test]
    fn clicks_choose_the_unit() {
        assert_eq!(Unit::of_clicks(1), Unit::Char);
        assert_eq!(Unit::of_clicks(2), Unit::Word);
        assert_eq!(Unit::of_clicks(5), Unit::Line);
    }
}
