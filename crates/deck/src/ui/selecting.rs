//! Selecting transcript text with the mouse and copying it (ADR 0013): a
//! press on a run, a drag, double and triple clicks for words and lines,
//! cmd/ctrl-c and escape.
//!
//! The design choice: a press only arms a selection; it starts once the
//! pointer moves a few pixels, so clicks keep meaning what they did (a
//! card header still opens its card). A press on text is remembered, so the
//! row it lands on does not also take it as a click to fold. One selection
//! at a time: a transcript press clears the composer's, and selecting in
//! the composer clears this one.

use super::root::Deck;
use super::selectable::{Hits, Located};
use super::selection::{Pos, Selection, Span, Unit, unit_at};
use gpui::{ClipboardItem, Context, KeyDownEvent, MouseDownEvent, MouseMoveEvent, Pixels, Point};

/// How far a press moves before it becomes a selection.
const DRAG: f32 = 3.;

/// The transcript's selection, the press that may start one, and the runs
/// drawn this frame.
#[derive(Default)]
pub struct TextSelect {
    pub(super) selection: Option<Selection>,
    /// The selection resolved against this frame's rows.
    pub(super) span: Option<Span>,
    pub(super) hits: Hits,
    press: Option<Press>,
    /// The last press landed on text, so the row under it ignores the click.
    pub(super) on_text: bool,
    /// The task the selection was made in; another task's rows share keys.
    task: Option<String>,
}

struct Press {
    at: (Pos, Pos),
    origin: Point<Pixels>,
    unit: Unit,
}

fn unit_of(l: &Located, unit: Unit) -> (Pos, Pos) {
    unit_at(&l.row, l.run, &l.text, l.offset, unit)
}

impl Deck {
    /// A left press in the transcript: clear what was selected, and arm a
    /// new selection when it lands on text; a double or triple click
    /// selects the word or line at once.
    pub(super) fn transcript_press(&mut self, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        let s = &mut self.text_select;
        s.selection = None;
        s.press = None;
        s.on_text = false;
        let Some(hit) = s.hits.locate(ev.position).filter(|h| h.inside) else { return cx.notify() };
        let unit = Unit::of_clicks(ev.click_count);
        let at = unit_of(&hit, unit);
        if unit != Unit::Char {
            s.selection = Some(Selection::new(at.clone(), unit));
        }
        s.press = Some(Press { at, origin: ev.position, unit });
        s.on_text = true;
        s.task = self.selected.clone();
        let cursor = self.current().editor.cursor();
        self.current_mut().editor.move_to(cursor, false);
        cx.notify();
    }

    /// The pointer moved with the button down: extend the selection to the
    /// run under it, once it has moved far enough from the press.
    pub(super) fn transcript_drag(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        let s = &mut self.text_select;
        let Some(press) = s.press.as_ref().filter(|_| ev.dragging()) else { return };
        let moved = ev.position - press.origin;
        if s.selection.is_none() && f32::from(moved.x).hypot(f32::from(moved.y)) < DRAG {
            return;
        }
        let Some(to) = s.hits.locate(ev.position) else { return };
        let to = unit_of(&to, press.unit);
        s.selection.get_or_insert_with(|| Selection::new(press.at.clone(), press.unit)).extend(to);
        cx.notify();
    }

    /// The button came up: the selection stays, the press is over.
    pub(super) fn transcript_release(&mut self) {
        self.text_select.press = None;
    }

    /// Drop the selection when the task it was made in is no longer shown,
    /// then resolve it against `rows` for this frame.
    pub(super) fn resolve_selection(&mut self, rows: &[String]) {
        let s = &mut self.text_select;
        if s.selection.is_some() && s.task != self.selected {
            s.selection = None;
        }
        s.span = s.selection.as_ref().and_then(|sel| sel.span(rows));
    }

    /// The selected text, read from the session rather than the screen.
    pub fn transcript_selection(&self) -> Option<String> {
        let span = self.text_select.span?;
        let entries = &self.session().entries;
        let rows = super::transcript::rows(entries, self.session().idle());
        let text = span.text(|ix| {
            let row = &rows[ix];
            super::runs::runs(entries, row, self.expanded.contains(&row.key(entries)))
        });
        (!text.is_empty()).then_some(text)
    }

    /// The selection for the control socket: the rows it starts and ends
    /// in and how many bytes it copies, never the text itself.
    pub fn selection_extent(&self) -> Option<(String, String, usize)> {
        let span = self.text_select.span?;
        let entries = &self.session().entries;
        let rows = super::transcript::rows(entries, self.session().idle());
        let key = |ix: usize| rows.get(ix).map(|r| r.key(entries)).unwrap_or_default();
        let len = self.transcript_selection().map_or(0, |t| t.len());
        Some((key(span.rows().start), key(span.rows().end - 1), len))
    }

    /// Copy and escape for a transcript selection; false when the key is
    /// not one, or the composer has a selection of its own to copy.
    pub(super) fn selection_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let k = &ev.keystroke;
        let command = k.modifiers.control || k.modifiers.platform;
        match k.key.as_str() {
            "c" if command && self.current().editor.selection().is_empty() => {
                let Some(text) = self.transcript_selection() else { return false };
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                true
            }
            "escape" if self.text_select.selection.is_some() => {
                self.clear_selection(cx);
                true
            }
            _ => false,
        }
    }

    pub(super) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.text_select.selection = None;
        self.text_select.span = None;
        cx.notify();
    }

    /// The runs drawn last frame, `(row, run, text)`, for tests that check
    /// them against [`Deck::listed_runs`].
    pub fn drawn_runs(&self) -> Vec<(String, usize, String)> {
        self.text_select.hits.drawn()
    }

    /// Every row's runs as `runs.rs` lists them, `(row, run, text)`.
    pub fn listed_runs(&self) -> Vec<(String, usize, String)> {
        let entries = &self.session().entries;
        let rows = super::transcript::rows(entries, self.session().idle());
        rows.iter()
            .flat_map(|row| {
                let key = row.key(entries);
                let runs = super::runs::runs(entries, row, self.expanded.contains(&key));
                runs.into_iter().enumerate().map(move |(n, r)| (key.clone(), n, r.text))
            })
            .collect()
    }

    /// Where run `run` of row `row` was drawn last frame, in window pixels.
    pub fn run_bounds(&self, row: &str, run: usize) -> Option<gpui::Bounds<Pixels>> {
        self.text_select.hits.bounds(row, run)
    }
}
