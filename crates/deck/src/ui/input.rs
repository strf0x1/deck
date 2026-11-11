//! The composer's input: keys that edit without typing (moves, deletes,
//! clipboard, send, prompt recall) and the platform input handler that
//! delivers typed and IME-composed text.
//!
//! The design choice: a key is the Deck key handler's first, and text is
//! the input handler's only after [`Deck::edit`] declines it, which is the
//! order GPUI's platforms dispatch in. So a key such as enter or tab, which
//! also carries text, never lands in the field twice. Offsets crossing the
//! platform boundary are UTF-16; [`Editor`](super::editor::Editor)
//! converts them.

use super::editor::Editor;
use super::root::Deck;
use gpui::{Bounds, ClipboardItem, Context, EntityInputHandler, KeyDownEvent, Pixels, Point, UTF16Selection, Window};
use std::ops::Range;

impl Deck {
    /// The editor typed text goes to: the settings form's focused field while
    /// the panel has one open, else the composer.
    pub(super) fn input_editor(&self) -> &Editor {
        match self.settings.is_open().then(|| self.settings.editor()).flatten() {
            Some(editor) => editor,
            None => &self.editor,
        }
    }

    fn input_editor_mut(&mut self) -> &mut Editor {
        if self.settings.is_open()
            && self.settings.editor().is_some()
            && let Some(editor) = self.settings.editor_mut()
        {
            return editor;
        }
        &mut self.editor
    }

    /// A composer key that is not text: moves, deletes, clipboard, send.
    /// Returns whether it was one; text itself arrives through the input
    /// handler (`composer.rs`), after this declines it.
    pub(super) fn edit(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let m = &ev.keystroke.modifiers;
        let shift = m.shift;
        // A word at a time is alt on macOS and ctrl elsewhere; cmd moves by line.
        let word = m.alt || m.control;
        let command = m.control || m.platform;
        // Cards before text: alt-up/down select, tab opens, escape lets go.
        match ev.keystroke.key.as_str() {
            "up" | "down" if m.alt && !m.shift => {
                self.select_card(ev.keystroke.key == "up", cx);
                return true;
            }
            "tab" => {
                self.toggle_selected(cx);
                return true;
            }
            "escape" if self.focused.is_some() => {
                self.focused = None;
                return true;
            }
            _ => {}
        }
        let e = &mut self.editor;
        match ev.keystroke.key.as_str() {
            "enter" if shift || m.alt => e.insert("\n"),
            "enter" => self.submit(cx),
            "backspace" => e.backspace(word),
            "delete" => e.delete(word),
            "left" if m.platform => e.line_start(shift),
            "right" if m.platform => e.line_end(shift),
            "left" => e.left(shift, word),
            "right" => e.right(shift, word),
            "home" => e.line_start(shift),
            "end" => e.line_end(shift),
            "up" | "down" => {
                let up = ev.keystroke.key == "up";
                match self.composer.vertical(e.cursor(), up) {
                    Some(at) => e.move_to(at, shift),
                    // Past the first or last row: earlier prompts, or the line's end.
                    None if shift => {
                        if up {
                            e.move_to(0, true)
                        } else {
                            e.move_to(e.text().len(), true)
                        }
                    }
                    None if up => {
                        e.recall_prev();
                    }
                    None => {
                        if !e.recall_next() {
                            e.move_to(e.text().len(), false);
                        }
                    }
                }
            }
            "a" if command => e.select_all(),
            "c" | "x" if command => {
                let selected = e.selected_text();
                if !selected.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(selected.to_string()));
                    if ev.keystroke.key == "x" {
                        e.insert("");
                    }
                }
            }
            "v" if command => {
                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    e.insert(&text);
                }
            }
            "escape" if !e.selection().is_empty() => {
                let at = e.cursor();
                e.move_to(at, false);
            }
            "escape" => {
                if let Some(c) = self.current().client.as_ref() {
                    let _ = c.cancel();
                }
            }
            _ => return false,
        }
        true
    }
}

impl EntityInputHandler for Deck {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let editor = self.input_editor();
        let range = editor.from_utf16(&range);
        actual.replace(editor.to_utf16(&range));
        editor.text().get(range).map(str::to_string)
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        let e = self.input_editor();
        Some(UTF16Selection { range: e.to_utf16(&e.selection()), reversed: e.reversed() })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        let e = self.input_editor();
        e.marked().map(|m| e.to_utf16(&m))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.input_editor_mut().unmark();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let e = self.input_editor_mut();
        match range {
            Some(r) => {
                let r = e.from_utf16(&r);
                e.replace(r, text)
            }
            None => e.insert(text),
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let e = self.input_editor_mut();
        let range = range.map(|r| e.from_utf16(&r)).or(e.marked()).unwrap_or(e.selection());
        // `selected` is in UTF-16 units of the new text.
        let selected = selected.map(|s| utf16_to_byte(text, s.start)..utf16_to_byte(text, s.end));
        e.replace_and_mark(range, text, selected);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.composer.bounds_for(self.input_editor().from_utf16(&range), bounds)
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.input_editor().offset_to_utf16(self.composer.index_at(position)))
    }
}

/// The byte offset in `text` of UTF-16 offset `units`, clamped to its end.
fn utf16_to_byte(text: &str, units: usize) -> usize {
    let mut seen = 0;
    for (i, c) in text.char_indices() {
        if seen >= units {
            return i;
        }
        seen += c.len_utf16();
    }
    text.len()
}
