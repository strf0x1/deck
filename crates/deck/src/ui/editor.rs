//! The composer's text and everything done to it: the selection, the IME's
//! marked range, caret moves by character, word and line, and recall of
//! earlier prompts.
//!
//! The design choice: pure state over UTF-8 byte offsets that always sit on
//! grapheme boundaries, with no GPUI types. The platform's input handler
//! speaks UTF-16, so the conversions live here too and the view only
//! forwards. Moves that need layout (up and down across wrapped rows) are
//! the view's; it hands back a byte offset.

use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation as _;

#[derive(Debug, Default)]
pub struct Editor {
    text: String,
    selection: Range<usize>,
    /// The caret is at the selection's start (it was extended leftward).
    reversed: bool,
    marked: Option<Range<usize>>,
    history: Vec<String>,
    /// Which history entry is shown, while recalling.
    recall: Option<usize>,
    /// The draft that was there before recall began.
    stash: String,
}

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn selection(&self) -> Range<usize> {
        self.selection.clone()
    }

    pub fn reversed(&self) -> bool {
        self.reversed
    }

    pub fn marked(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    /// Where the caret is: the moving end of the selection.
    pub fn cursor(&self) -> usize {
        if self.reversed { self.selection.start } else { self.selection.end }
    }

    pub fn selected_text(&self) -> &str {
        self.text.get(self.selection.clone()).unwrap_or_default()
    }

    /// Replace the marked text if the IME is composing, else the selection.
    pub fn insert(&mut self, text: &str) {
        let range = self.marked.clone().unwrap_or(self.selection.clone());
        self.replace(range, text);
    }

    /// Replace `range` (clamped to boundaries) with `text` and put the caret after it.
    pub fn replace(&mut self, range: Range<usize>, text: &str) {
        let range = self.clamp(range);
        self.text.replace_range(range.clone(), text);
        let at = range.start + text.len();
        self.selection = at..at;
        self.reversed = false;
        self.marked = None;
        self.recall = None;
    }

    /// The IME's composing edit: replace `range`, mark the new text, and
    /// select `selected` (relative to the new text) or put the caret after it.
    pub fn replace_and_mark(&mut self, range: Range<usize>, text: &str, selected: Option<Range<usize>>) {
        let range = self.clamp(range);
        self.text.replace_range(range.clone(), text);
        self.marked = (!text.is_empty()).then(|| range.start..range.start + text.len());
        let end = range.start + text.len();
        self.selection =
            selected.map(|s| (range.start + s.start).min(end)..(range.start + s.end).min(end)).unwrap_or(end..end);
        self.reversed = false;
        self.recall = None;
    }

    pub fn unmark(&mut self) {
        self.marked = None;
    }

    /// Move the caret to `at`, extending the selection when `extend`.
    pub fn move_to(&mut self, at: usize, extend: bool) {
        let at = self.boundary_at(at);
        if extend {
            let anchor = if self.reversed { self.selection.end } else { self.selection.start };
            self.reversed = at < anchor;
            self.selection = at.min(anchor)..at.max(anchor);
        } else {
            self.selection = at..at;
            self.reversed = false;
        }
    }

    /// Left by a character or a word; a plain move collapses a selection to its start.
    pub fn left(&mut self, extend: bool, word: bool) {
        if !extend && !self.selection.is_empty() && !word {
            return self.move_to(self.selection.start, false);
        }
        let at = if word { self.word_start(self.cursor()) } else { self.prev(self.cursor()) };
        self.move_to(at, extend);
    }

    pub fn right(&mut self, extend: bool, word: bool) {
        if !extend && !self.selection.is_empty() && !word {
            return self.move_to(self.selection.end, false);
        }
        let at = if word { self.word_end(self.cursor()) } else { self.next(self.cursor()) };
        self.move_to(at, extend);
    }

    /// The start of the caret's logical line.
    pub fn line_start(&mut self, extend: bool) {
        let at = self.before(self.cursor()).rfind('\n').map_or(0, |i| i + 1);
        self.move_to(at, extend);
    }

    pub fn line_end(&mut self, extend: bool) {
        let c = self.cursor();
        let at = self.after(c).find('\n').map_or(self.text.len(), |i| c + i);
        self.move_to(at, extend);
    }

    /// Select the word (or run of spaces or punctuation) under `at`.
    pub fn select_word(&mut self, at: usize) {
        let at = self.boundary_at(at);
        let (start, word) =
            self.text.split_word_bound_indices().find(|(i, w)| at < i + w.len()).unwrap_or((self.text.len(), ""));
        self.selection = start..start + word.len();
        self.reversed = false;
    }

    pub fn select_all(&mut self) {
        self.selection = 0..self.text.len();
        self.reversed = false;
    }

    /// Delete the selection, or the character (or word) before the caret.
    pub fn backspace(&mut self, word: bool) {
        if self.selection.is_empty() {
            let c = self.cursor();
            let from = if word { self.word_start(c) } else { self.prev(c) };
            self.selection = from..c;
        }
        self.replace(self.selection.clone(), "");
    }

    pub fn delete(&mut self, word: bool) {
        if self.selection.is_empty() {
            let c = self.cursor();
            let to = if word { self.word_end(c) } else { self.next(c) };
            self.selection = c..to;
        }
        self.replace(self.selection.clone(), "");
    }

    /// Hand over the draft for sending: it joins the history and the field empties.
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        if self.history.last() != Some(&text) && !text.trim().is_empty() {
            self.history.push(text.clone());
        }
        *self = Editor { history: std::mem::take(&mut self.history), ..Editor::default() };
        text
    }

    /// Show the previous prompt; `false` when there is none further back.
    pub fn recall_prev(&mut self) -> bool {
        let at = match self.recall {
            None if self.history.is_empty() => return false,
            None => {
                self.stash = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => return false,
            Some(i) => i - 1,
        };
        self.show(Some(at));
        true
    }

    /// Step back toward the draft; `false` when not recalling.
    pub fn recall_next(&mut self) -> bool {
        let Some(i) = self.recall else { return false };
        self.show((i + 1 < self.history.len()).then_some(i + 1));
        true
    }

    fn show(&mut self, at: Option<usize>) {
        self.text = match at {
            Some(i) => self.history[i].clone(),
            None => std::mem::take(&mut self.stash),
        };
        let end = self.text.len();
        self.selection = end..end;
        self.reversed = false;
        self.marked = None;
        self.recall = at;
    }

    pub fn to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    pub fn from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    pub fn offset_to_utf16(&self, offset: usize) -> usize {
        self.before(offset).chars().map(char::len_utf16).sum()
    }

    pub fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut units = 0;
        for (i, ch) in self.text.char_indices() {
            if units >= offset {
                return i;
            }
            units += ch.len_utf16();
        }
        self.text.len()
    }

    fn clamp(&self, range: Range<usize>) -> Range<usize> {
        let (a, b) = (self.char_at(range.start), self.char_at(range.end));
        a.min(b)..a.max(b)
    }

    /// `at` within the text and on a char boundary (the IME may split a grapheme).
    fn char_at(&self, at: usize) -> usize {
        let mut at = at.min(self.text.len());
        while !self.text.is_char_boundary(at) {
            at -= 1;
        }
        at
    }

    /// The text before `at`, empty if `at` is not a boundary.
    fn before(&self, at: usize) -> &str {
        self.text.get(..at.min(self.text.len())).unwrap_or_default()
    }

    fn after(&self, at: usize) -> &str {
        self.text.get(at..).unwrap_or_default()
    }

    /// `at` moved back to the grapheme boundary it falls in.
    fn boundary_at(&self, at: usize) -> usize {
        let at = self.char_at(at);
        if at == self.text.len() {
            return at;
        }
        self.text.grapheme_indices(true).map(|(i, _)| i).take_while(|&i| i <= at).last().unwrap_or(0)
    }

    fn prev(&self, at: usize) -> usize {
        self.before(at).grapheme_indices(true).next_back().map_or(0, |(i, _)| i)
    }

    fn next(&self, at: usize) -> usize {
        self.after(at).graphemes(true).next().map_or(at, |g| at + g.len())
    }

    /// The start of the word before `at`, skipping whitespace first.
    fn word_start(&self, at: usize) -> usize {
        self.before(at).split_word_bound_indices().rev().find(|(_, w)| !w.trim().is_empty()).map_or(0, |(i, _)| i)
    }

    fn word_end(&self, at: usize) -> usize {
        self.after(at)
            .split_word_bound_indices()
            .find(|(_, w)| !w.trim().is_empty())
            .map_or(self.text.len(), |(i, w)| at + i + w.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(text: &str) -> Editor {
        let mut e = Editor::new();
        e.insert(text);
        e
    }

    #[test]
    fn typing_replaces_the_selection_and_moves_by_grapheme() {
        let mut e = typed("héllo 👍🏽!");
        e.left(false, false);
        e.left(false, false);
        assert_eq!(e.cursor(), "héllo ".len(), "a skin-toned emoji is one step");
        e.left(true, false);
        e.left(true, false);
        assert_eq!(e.selected_text(), "o ");
        assert!(e.reversed());
        e.insert("O_");
        assert_eq!(e.text(), "héllO_👍🏽!");
        e.backspace(false);
        e.backspace(false);
        assert_eq!(e.text(), "héll👍🏽!");
    }

    #[test]
    fn words_and_lines() {
        let mut e = typed("ship the  spike\nnext line");
        e.backspace(true);
        assert_eq!(e.text(), "ship the  spike\nnext ");
        e.line_start(false);
        assert_eq!(e.cursor(), "ship the  spike\n".len());
        e.left(false, true);
        assert_eq!(e.cursor(), "ship the  ".len());
        e.left(false, true);
        assert_eq!(e.cursor(), "ship ".len());
        e.right(true, true);
        assert_eq!(e.selected_text(), "the");
        e.line_end(true);
        assert_eq!(e.selected_text(), "the  spike");
        e.select_word(2);
        assert_eq!(e.selected_text(), "ship");
        e.select_all();
        e.delete(false);
        assert!(e.is_empty());
    }

    #[test]
    fn a_plain_move_collapses_a_selection_to_its_side() {
        let mut e = typed("abcd");
        e.select_all();
        e.left(false, false);
        assert_eq!(e.selection(), 0..0);
        e.select_all();
        e.right(false, false);
        assert_eq!(e.selection(), 4..4);
    }

    #[test]
    fn the_ime_composes_then_commits() {
        let mut e = typed("a");
        e.replace_and_mark(1..1, "ni", Some(2..2));
        assert_eq!((e.text(), e.marked(), e.selection()), ("ani", Some(1..3), 3..3));
        e.replace_and_mark(e.marked().unwrap(), "に", None);
        assert_eq!((e.text(), e.marked()), ("aに", Some(1..4)));
        e.insert("日");
        assert_eq!((e.text(), e.marked(), e.cursor()), ("a日", None, 4));
    }

    #[test]
    fn utf16_offsets_round_trip_through_astral_characters() {
        let e = typed("a👍b");
        assert_eq!(e.to_utf16(&(0..e.text().len())), 0..4);
        assert_eq!(e.from_utf16(&(1..3)), 1..5);
        assert_eq!(e.offset_from_utf16(99), e.text().len());
    }

    #[test]
    fn history_recalls_earlier_prompts_and_returns_to_the_draft() {
        let mut e = Editor::new();
        for p in ["one", "two", "two", "  "] {
            e.insert(p);
            e.take();
        }
        e.insert("dra");
        assert!(e.recall_prev());
        assert_eq!(e.text(), "two", "a repeat is stored once and blanks not at all");
        assert!(e.recall_prev());
        assert_eq!(e.text(), "one");
        assert!(!e.recall_prev());
        assert!(e.recall_next());
        assert!(e.recall_next());
        assert_eq!((e.text(), e.cursor()), ("dra", 3));
        assert!(!e.recall_next());
    }

    #[test]
    fn out_of_range_and_mid_character_offsets_are_clamped() {
        let mut e = typed("é");
        e.replace(1..99, "x");
        assert_eq!(e.text(), "x");
        e.move_to(99, false);
        assert_eq!(e.cursor(), 1);
    }
}
