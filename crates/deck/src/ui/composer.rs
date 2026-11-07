//! The composer field as a GPUI element: wrapped text, selection, the IME's
//! marked text and the caret, plus the platform input handler that feeds
//! typed and composed text into [`Editor`](super::editor::Editor).
//!
//! The design choice: the element owns no state. It reads the editor from
//! [`Deck`], shapes the text at the width layout gives it, and leaves the
//! shaped rows in [`Layout`] on the Deck, so hit tests, the IME's caret
//! rectangle and up/down moves all use what was actually drawn. Text input
//! goes through the window's input handler (registered at paint, on the
//! Deck's one focus handle); keys that edit without inserting text are the
//! Deck's key handler.

use super::root::Deck;
use super::theme::{Tokens, tint};
use gpui::{
    App, AvailableSpace, Bounds, ContentMask, Element, ElementId, ElementInputHandler, Entity, GlobalElementId, Hsla,
    InspectorElementId, IntoElement, LayoutId, Pixels, Point, SharedString, Style, TextRun, UnderlineStyle, Window,
    WrappedLine, fill, point, px, relative, size,
};
use std::ops::Range;

/// The field grows to this many rows, then scrolls.
pub const MAX_ROWS: usize = 8;

/// What the composer drew last: rows, where, and how far it is scrolled.
#[derive(Default)]
pub struct Layout {
    rows: Vec<Row>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    scroll: Pixels,
}

/// One logical line (the text between newlines), wrapped into visual rows.
struct Row {
    line: WrappedLine,
    start: usize,
    top: Pixels,
}

impl Layout {
    fn height(&self) -> Pixels {
        self.rows.last().map_or(self.line_height, |r| r.top + r.line.size(self.line_height).height)
    }

    /// Where byte `index` is drawn, relative to the unscrolled content.
    pub fn position(&self, index: usize) -> Option<Point<Pixels>> {
        let row = self.rows.iter().find(|r| index <= r.start + r.line.len())?;
        let at = row.line.position_for_index(index.saturating_sub(row.start), self.line_height)?;
        Some(point(at.x, at.y + row.top))
    }

    /// The byte index nearest a window position.
    pub fn index_at(&self, position: Point<Pixels>) -> usize {
        let local = position - self.bounds.origin;
        self.index_in_content(local.x, local.y + self.scroll)
    }

    fn index_in_content(&self, x: Pixels, y: Pixels) -> usize {
        let Some(row) = self.rows.iter().rev().find(|r| r.top <= y).or(self.rows.first()) else { return 0 };
        let row_height = row.line.size(self.line_height).height;
        let y = (y - row.top).clamp(px(0.), row_height - px(1.));
        let i = row.line.closest_index_for_position(point(x.max(px(0.)), y), self.line_height);
        row.start + i.unwrap_or_else(|i| i)
    }

    /// Where `range` is drawn in window coordinates, for the IME's
    /// candidate window; a range over several rows reports its first.
    pub fn bounds_for(&self, range: Range<usize>, element: Bounds<Pixels>) -> Option<Bounds<Pixels>> {
        let (a, b) = (self.position(range.start)?, self.position(range.end)?);
        let origin = point(element.origin.x, element.origin.y - self.scroll);
        let b = if b.y == a.y { b } else { a };
        Some(Bounds::from_corners(a, point(b.x, b.y + self.line_height)) + origin)
    }

    /// The index one visual row above (`up`) or below `index`, keeping its
    /// x; `None` past the first or last row.
    pub fn vertical(&self, index: usize, up: bool) -> Option<usize> {
        let at = self.position(index)?;
        let y = if up { at.y - self.line_height } else { at.y + self.line_height };
        if y < px(0.) || y >= self.height() {
            return None;
        }
        Some(self.index_in_content(at.x, y + self.line_height / 2.))
    }
}

/// The composer's text, drawn from the Deck's editor.
pub struct Field {
    deck: Entity<Deck>,
    caret: Hsla,
    placeholder: SharedString,
}

impl Field {
    pub fn new(deck: Entity<Deck>, caret: Hsla, placeholder: impl Into<SharedString>) -> Self {
        Field { deck, caret, placeholder: placeholder.into() }
    }

    /// The text to shape and its runs: the draft (marked text underlined),
    /// or the faint placeholder when it is empty.
    fn runs(&self, window: &Window, cx: &App) -> (SharedString, Vec<TextRun>) {
        let style = window.text_style();
        let editor = &self.deck.read(cx).editor;
        let (text, color): (SharedString, Hsla) = if editor.is_empty() {
            (self.placeholder.clone(), Tokens::text_faint().into())
        } else {
            (editor.text().to_string().into(), style.color)
        };
        let run = TextRun { len: text.len(), font: style.font(), color, ..TextRun::default() };
        let runs = match editor.marked().filter(|_| !editor.is_empty()) {
            Some(m) => [
                TextRun { len: m.start, ..run.clone() },
                TextRun {
                    len: m.end - m.start,
                    underline: Some(UnderlineStyle { color: Some(color), thickness: px(1.), wavy: false }),
                    ..run.clone()
                },
                TextRun { len: text.len() - m.end, ..run },
            ]
            .into_iter()
            .filter(|r| r.len > 0)
            .collect(),
            None => vec![run],
        };
        (text, runs)
    }
}

/// The element's font size, read while its text style is in effect (layout
/// measures later, outside it).
fn font_size(window: &Window) -> Pixels {
    window.text_style().font_size.to_pixels(window.rem_size())
}

fn shape(text: &SharedString, runs: &[TextRun], font_size: Pixels, width: Pixels, window: &Window) -> Vec<WrappedLine> {
    window
        .text_system()
        .shape_text(text.clone(), font_size, runs, Some(width), None)
        .map_or(vec![], |l| l.into_iter().collect())
}

impl IntoElement for Field {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for Field {
    type RequestLayoutState = ();
    type PrepaintState = Vec<WrappedLine>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let (text, runs) = self.runs(window, cx);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let (font_size, line_height) = (font_size(window), window.line_height());
        let id = window.request_measured_layout(style, move |known, available, window, _| {
            let width = known.width.unwrap_or(match available.width {
                AvailableSpace::Definite(w) => w,
                _ => Pixels::MAX,
            });
            let rows: usize =
                shape(&text, &runs, font_size, width, window).iter().map(|l| l.wrap_boundaries().len() + 1).sum();
            size(width, line_height * rows.clamp(1, MAX_ROWS) as f32)
        });
        (id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<WrappedLine> {
        let (text, runs) = self.runs(window, cx);
        shape(&text, &runs, font_size(window), bounds.size.width, window)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        lines: &mut Vec<WrappedLine>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let line_height = window.line_height();
        let (focus, text_len, selection, cursor, empty, old_scroll) = {
            let d = self.deck.read(cx);
            let e = &d.editor;
            (d.focus.clone(), e.text().len(), e.selection(), e.cursor(), e.is_empty(), d.composer.scroll)
        };
        let mut rows = Vec::with_capacity(lines.len());
        let mut start = 0;
        let mut top = px(0.);
        for line in lines.drain(..) {
            let len = line.len();
            let height = line.size(line_height).height;
            rows.push(Row { line, start, top });
            start += len + 1;
            top += height;
        }
        let mut layout = Layout { rows, bounds, line_height, scroll: old_scroll };
        // The field scrolls only to keep the caret in view.
        let caret = if empty { Some(point(px(0.), px(0.))) } else { layout.position(cursor) };
        if let Some(at) = caret {
            let visible = bounds.size.height;
            layout.scroll = layout.scroll.min(at.y).max(at.y + line_height - visible).max(px(0.));
        }
        let origin = point(bounds.origin.x, bounds.origin.y - layout.scroll);
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.deck.clone()), cx);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if !selection.is_empty() && !empty {
                for quad in selection_rects(&layout, selection.clone(), text_len) {
                    window.paint_quad(fill(quad + origin, tint(Tokens::accent(), 0.28)));
                }
            }
            for row in &layout.rows {
                let at = point(origin.x, origin.y + row.top);
                let _ = row.line.paint(at, line_height, gpui::TextAlign::Left, None, window, cx);
            }
            if let Some(at) = caret.filter(|_| selection.is_empty()) {
                let quad = Bounds::new(point(at.x, at.y + px(2.)), size(px(2.), line_height - px(4.)));
                window.paint_quad(fill(quad + origin, self.caret));
            }
        });
        self.deck.update(cx, |d, _| d.composer = layout);
    }
}

/// The selection as one rectangle per visual row it covers.
fn selection_rects(layout: &Layout, range: Range<usize>, text_len: usize) -> Vec<Bounds<Pixels>> {
    let (Some(a), Some(b)) = (layout.position(range.start), layout.position(range.end.min(text_len))) else {
        return vec![];
    };
    let lh = layout.line_height;
    let right = layout.bounds.size.width;
    if a.y == b.y {
        return vec![Bounds::from_corners(a, point(b.x, b.y + lh))];
    }
    let mut rects = vec![Bounds::from_corners(a, point(right, a.y + lh))];
    if b.y > a.y + lh {
        rects.push(Bounds::from_corners(point(px(0.), a.y + lh), point(right, b.y)));
    }
    rects.push(Bounds::from_corners(point(px(0.), b.y), point(b.x, b.y + lh)));
    rects
}
