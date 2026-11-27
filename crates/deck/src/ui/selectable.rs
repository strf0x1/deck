//! Drawing selectable transcript text: each run (`runs.rs`) as styled text
//! that wears the selection as its background and records, while it
//! prepaints, where it landed, so a mouse point can become a position.
//!
//! The design choice: pixels live for one frame only (ADR 0013). The hit
//! map is cleared every render and refilled by the runs GPUI actually lays
//! out, so a row the virtualized list skipped is simply absent, and the
//! selection itself, held as text positions, never depends on it. The
//! highlight is the selected bytes' run background rather than painted
//! quads, so it follows wrapping and fonts exactly as the text does.

use super::selection::Span;
use super::theme::Tokens;
use gpui::{
    App, Bounds, ElementId, GlobalElementId, HighlightStyle, InspectorElementId, IntoElement, LayoutId, Pixels, Point,
    SharedString, StyledText, TextLayout, TextRun, Window,
};
use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;

/// One drawn run this frame: which text it is and where it landed.
#[derive(Clone)]
pub struct Hit {
    pub row: String,
    pub run: usize,
    pub text: SharedString,
    bounds: Bounds<Pixels>,
    layout: TextLayout,
}

/// A point turned into a run and a byte offset; `inside` when the point
/// was on the run itself rather than nearest to it.
#[derive(Debug, Clone, PartialEq)]
pub struct Located {
    pub row: String,
    pub run: usize,
    pub text: SharedString,
    pub offset: usize,
    pub inside: bool,
}

/// The runs drawn this frame, in drawing (reading) order.
#[derive(Clone, Default)]
pub struct Hits(Rc<RefCell<Vec<Hit>>>);

impl Hits {
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    /// `(row, run, text)` of each drawn run, for checking against `runs.rs`.
    pub fn drawn(&self) -> Vec<(String, usize, String)> {
        self.0.borrow().iter().map(|h| (h.row.clone(), h.run, h.text.to_string())).collect()
    }

    /// Where run `run` of row `row` was drawn, if it was.
    pub fn bounds(&self, row: &str, run: usize) -> Option<Bounds<Pixels>> {
        self.0.borrow().iter().find(|h| h.row == row && h.run == run).map(|h| h.bounds)
    }

    /// The run and offset under `p`. Off any run, the nearest on the same
    /// line; between lines, the end of the run above (the start of the
    /// first, above them all). None when nothing is drawn.
    pub fn locate(&self, p: Point<Pixels>) -> Option<Located> {
        let hits = self.0.borrow();
        let on_line = |h: &&Hit| h.bounds.top() <= p.y && p.y < h.bounds.bottom();
        let left = |h: &Hit| f32::from(h.bounds.left());
        let right = |h: &Hit| f32::from(h.bounds.right());
        let x = f32::from(p.x);
        let distance = |h: &&Hit| (left(h) - x).max(x - right(h)).max(0.);
        if let Some(h) = hits.iter().filter(on_line).min_by(|a, b| distance(a).total_cmp(&distance(b))) {
            let x = gpui::px(x.clamp(left(h), right(h)));
            let offset = h.layout.index_for_position(gpui::point(x, p.y)).unwrap_or_else(|i| i);
            return Some(located(h, offset, h.bounds.contains(&p)));
        }
        match hits.iter().rev().find(|h| h.bounds.top() < p.y) {
            Some(above) => Some(located(above, above.text.len(), false)),
            None => hits.first().map(|h| located(h, 0, false)),
        }
    }

    fn push(&self, hit: Hit) {
        self.0.borrow_mut().push(hit);
    }
}

fn located(h: &Hit, offset: usize, inside: bool) -> Located {
    Located { row: h.row.clone(), run: h.run, text: h.text.clone(), offset: offset.min(h.text.len()), inside }
}

/// Text to draw as a run: plain (styled by its container) or with runs.
pub enum Text {
    Plain(String),
    Styled(String, Vec<TextRun>),
}

/// What one row needs to draw its runs: its key and index, the selection
/// resolved for this frame, and the hit map. Runs are numbered in the
/// order [`RowRuns::text`] is called, which must be the order `runs.rs`
/// lists them.
pub struct RowRuns {
    row: String,
    ix: usize,
    next: Cell<usize>,
    span: Option<Span>,
    hits: Hits,
}

impl RowRuns {
    /// A stable element id for a container starting at the next run.
    pub fn element_id(&self, kind: &str) -> ElementId {
        format!("{}:{kind}:{}", self.row, self.next.get()).into()
    }

    pub fn new(row: String, ix: usize, span: Option<Span>, hits: Hits) -> Self {
        RowRuns { row, ix, next: Cell::new(0), span, hits }
    }

    /// The next run, highlighted where the selection covers it.
    pub fn text(&self, text: Text) -> Selectable {
        let run = self.next.replace(self.next.get() + 1);
        let content = SharedString::from(match &text {
            Text::Plain(s) | Text::Styled(s, _) => s.clone(),
        });
        let covered = self.span.and_then(|s| s.covers(self.ix, run, &content));
        let styled = match (text, covered) {
            (Text::Plain(_), None) => StyledText::new(content.clone()),
            (Text::Plain(_), Some(range)) => StyledText::new(content.clone()).with_highlights([(
                range,
                HighlightStyle { background_color: Some(Tokens::selection()), ..HighlightStyle::default() },
            )]),
            (Text::Styled(_, runs), covered) => {
                let runs = match covered {
                    Some(range) => highlight(runs, range),
                    None => runs,
                };
                StyledText::new(content.clone()).with_runs(runs)
            }
        };
        let record = (self.hits.clone(), self.row.clone(), run, content);
        Selectable { styled, record }
    }
}

/// `runs` with the bytes in `range` on the selection background, split at
/// its ends.
fn highlight(runs: Vec<TextRun>, range: Range<usize>) -> Vec<TextRun> {
    let mut out = Vec::with_capacity(runs.len() + 2);
    let mut at = 0;
    for run in runs {
        let (start, end) = (at, at + run.len);
        at = end;
        let cuts = [start, range.start.clamp(start, end), range.end.clamp(start, end), end];
        for w in cuts.windows(2) {
            if w[1] > w[0] {
                let selected = w[0] >= range.start && w[1] <= range.end;
                out.push(TextRun {
                    len: w[1] - w[0],
                    background_color: if selected { Some(Tokens::selection()) } else { run.background_color },
                    ..run.clone()
                });
            }
        }
    }
    out
}

/// A run's styled text that adds itself to the hit map once laid out.
pub struct Selectable {
    styled: StyledText,
    record: (Hits, String, usize, SharedString),
}

impl IntoElement for Selectable {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for Selectable {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.styled.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.styled.prepaint(id, inspector_id, bounds, state, window, cx);
        let (hits, row, run, text) = &self.record;
        let layout = self.styled.layout().clone();
        hits.push(Hit { row: row.clone(), run: *run, text: text.clone(), bounds, layout });
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request: &mut (),
        prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.styled.paint(id, inspector_id, bounds, request, prepaint, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(len: usize) -> TextRun {
        TextRun { len, font: gpui::font("x"), color: gpui::black(), ..TextRun::default() }
    }

    #[test]
    fn highlighting_splits_runs_at_the_selection_ends() {
        let out = highlight(vec![run(4), run(6)], 2..7);
        let lens: Vec<_> = out.iter().map(|r| (r.len, r.background_color.is_some())).collect();
        assert_eq!(lens, [(2, false), (2, true), (3, true), (3, false)]);
    }
}
