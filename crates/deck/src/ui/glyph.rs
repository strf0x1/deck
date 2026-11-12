//! The icon element: one SVG from the embedded asset table, sized and tinted.
//!
//! The design choice: GPUI's own `svg()` paints only with a colour set on the
//! element itself, while Deck's icons take the colour of the row they sit in —
//! a dimmed row dims its icons, and a card header sets one hue for its text and
//! its mark together. So this reads the inherited text style the way a text run
//! does, which is what the Nerd Font glyphs it replaced did.

use gpui::prelude::Refineable as _;
use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId, Pixels, Style,
    StyleRefinement, Styled, TransformationMatrix, Window, px,
};

/// One icon, drawn in a `side`-pixel square. The shape keeps its own 1:1 aspect
/// inside that square; the caller's `text_color` wins over the row's.
pub struct Glyph {
    path: &'static str,
    side: Pixels,
    color: Option<Hsla>,
    style: StyleRefinement,
}

/// The icon at `path` (a name from [`crate::ui::theme::icon`]) at `side` pixels.
pub fn glyph(path: &'static str, side: f32) -> Glyph {
    let mut glyph = Glyph { path, side: px(side), color: None, style: StyleRefinement::default() };
    glyph.style.size.width = Some(glyph.side.into());
    glyph.style.size.height = Some(glyph.side.into());
    glyph
}

impl Element for Glyph {
    type RequestLayoutState = ();
    type PrepaintState = Hsla;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.refine(&self.style);
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        self.color.unwrap_or_else(|| window.text_style().color)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        color: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // A path the asset table does not carry paints nothing;
        // `crate::assets::tests` is what stops that happening.
        let _ = window.paint_svg(bounds, self.path.into(), None, TransformationMatrix::default(), *color, cx);
    }
}

impl IntoElement for Glyph {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Styled for Glyph {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl Glyph {
    /// The colour to draw in, instead of the row's text colour.
    pub fn text_color(mut self, color: impl Into<Hsla>) -> Self {
        self.color = Some(color.into());
        self
    }
}
