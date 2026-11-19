//! Render helpers for the small shared pieces other regions use: icon marks,
//! state colours, pills, the two button weights, the activity sheen and panes.
//!
//! The design choice: free functions, not separate entities. Nothing here owns
//! state; every interaction calls back into `Deck`. A colour arrives as an
//! argument, so each caller says what the hue means where it is used.

use super::band::Piece;
use super::board::{Lane, LaneState};
use super::glyph::glyph;
use super::theme::{MONO, Tokens, glow, icon, radius, size, space, tint};
use gpui::{
    Bounds, Div, FontWeight, Hsla, IntoElement, ParentElement as _, Path, Styled as _, canvas, div, fill,
    linear_color_stop, linear_gradient, point, px,
};

pub fn state_color(s: LaneState) -> Hsla {
    match s {
        LaneState::Idle => Tokens::text_faint().into(),
        LaneState::Working => Tokens::cyan(),
        LaneState::Waiting => Tokens::amber(),
        LaneState::Review => Tokens::lime(),
    }
}

/// The mark a row wears: a row that asks wears the waiting mark, whatever
/// its process is doing.
pub fn mark_state(lane: &Lane) -> LaneState {
    if lane.attention.is_some() { LaneState::Waiting } else { lane.state }
}

/// A state's glyph and whether it is lit: a shape as well as a hue, so the
/// mark reads without colour.
fn mark_glyph(s: LaneState) -> (&'static str, bool) {
    match s {
        LaneState::Idle => (icon::RING, false),
        LaneState::Working => (icon::BOLT, true),
        LaneState::Waiting => (icon::BELL, true),
        LaneState::Review => (icon::CHECK, false),
    }
}

/// A task's state as a mark: a shape as well as a hue, so it reads without
/// colour. It does not glow, and nothing frames it — the shape is the whole
/// mark. A task the window is not showing reports only that its agent is
/// running, not that a turn is.
pub fn state_mark(s: LaneState, size: f32) -> Div {
    let (g, lit) = mark_glyph(s);
    let color = state_color(s);
    div()
        .flex_none()
        .size(px(size + 4.))
        .flex()
        .items_center()
        .justify_center()
        .text_color(if lit { color } else { tint(color, 0.75) })
        .child(glyph(g, size))
}

/// Two state marks crossfading: the old glyph fades out as the new one fades
/// in (`marks.rs`). The box is `state_mark`'s fixed size and only opacity
/// moves, so nothing re-measures.
pub fn state_mark_faded(from: LaneState, to: LaneState, t: f32, size: f32) -> Div {
    let layer = |s: LaneState, alpha: f32| {
        let (g, lit) = mark_glyph(s);
        let color = state_color(s);
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(if lit { color } else { tint(color, 0.75) })
            .opacity(alpha)
            .child(glyph(g, size))
    };
    div().flex_none().size(px(size + 4.)).relative().child(layer(from, 1. - t)).child(layer(to, t))
}

/// The mark that says what kind of row this is: an icon in a `side`-pixel
/// column, in the row's own hue. The column is what keeps a card's headers and
/// a panel's rows aligned; the icon fills most of it, since nothing else frames it.
pub fn icon_mark(g: &'static str, color: Hsla, side: f32) -> Div {
    div()
        .flex_none()
        .size(px(side))
        .flex()
        .items_center()
        .justify_center()
        .text_color(color)
        .child(glyph(g, (side * 0.72).round()))
}

/// A status pill: a dot and a word in the state's colour, edged in it.
pub fn pill(color: Hsla, label: impl Into<String>) -> Div {
    div()
        .flex_none()
        .min_h(px(24.))
        .px(px(10.))
        .flex()
        .items_center()
        .gap(px(7.))
        .rounded_full()
        .border_1()
        .border_color(tint(color, 0.4))
        .bg(Tokens::well())
        .text_size(size::scaled(size::SMALL))
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .child(div().size(px(6.)).rounded_full().bg(color))
        .child(label.into())
}

/// A dark body that wears `color` in its text and border instead of a fill:
/// how a coloured mark stands off the glass without a wash of colour.
pub fn outlined(color: Hsla) -> Div {
    div().border_1().border_color(tint(color, 0.5)).bg(Tokens::well()).text_color(color)
}

/// A gradient band riding a surface's outline, never its body, following
/// the rounded contour through its corners (`band.rs`): linear ramps on the
/// straight edges, annular sectors through the arcs, solid runs down the
/// sides. The canvas reads the size the surface laid out and paints the
/// pieces at the band's centre; paint-only, nothing re-measures. `peak` is
/// the band's colour at full strength — its alpha is the band's own weight —
/// and `alpha` scales the whole band, for a fade.
pub fn band_border(center: f32, corners: [f32; 4], peak: Hsla, alpha: f32) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds: Bounds<gpui::Pixels>, _, window, _| {
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            let at = |x: f32, y: f32| bounds.origin + point(px(x), px(y));
            let color = |a: f32| tint(peak, peak.a * a * alpha);
            for piece in super::band::pieces(w, h, corners, center) {
                match piece {
                    Piece::Ramp { x, y, w, h, a0, a1 } => {
                        let bounds = Bounds { origin: at(x, y), size: gpui::size(px(w), px(h)) };
                        window.paint_quad(fill(
                            bounds,
                            linear_gradient(90., linear_color_stop(color(a0), 0.), linear_color_stop(color(a1), 1.)),
                        ));
                    }
                    Piece::Solid { x, y, w, h, a } => {
                        let bounds = Bounds { origin: at(x, y), size: gpui::size(px(w), px(h)) };
                        window.paint_quad(fill(bounds, color(a)));
                    }
                    Piece::Sector { points, a } => {
                        let mut path = Path::new(at(points[0].0, points[0].1));
                        for (x, y) in &points[1..] {
                            path.line_to(at(*x, *y));
                        }
                        window.paint_path(path, color(a));
                    }
                }
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// The activity sheen as a shared mark: the neutral white band (`sheen.rs`)
/// on the window sheen's centre.
pub fn sheen_border(center: f32, corner: f32) -> impl IntoElement {
    band_border(center, [corner; 4], Tokens::sheen_peak(), 1.)
}

/// The done pulse as a shared mark: the same band in the done hue, crossing
/// once when a turn ends (`exhale.rs`).
pub fn pulse_border(center: f32, corner: f32) -> impl IntoElement {
    band_border(center, [corner; 4], Tokens::done_peak(), 1.)
}

/// The one loud button in a region: the user's own colour, in its border
/// and text on a dark body, with a faint halo.
pub fn primary() -> Div {
    outlined(Tokens::accent())
        .min_h(px(32.))
        .px(px(space::PAD))
        .flex()
        .items_center()
        .gap(px(space::LINE))
        .rounded(px(radius::CONTROL))
        .border_color(tint(Tokens::accent(), 0.7))
        .shadow(glow(Tokens::accent(), 0.6))
        .text_size(size::scaled(size::BODY))
        .font_weight(FontWeight::SEMIBOLD)
}

/// A quiet section heading: small, mono, upper case.
pub fn eyebrow(label: &str) -> Div {
    div()
        .font_family(MONO)
        .text_size(size::scaled(size::MICRO))
        .font_weight(FontWeight::MEDIUM)
        .text_color(Tokens::text_faint())
        .child(label.to_uppercase())
}

/// One pane of glass: the conversation sits on this.
pub fn pane() -> Div {
    div().relative().rounded(px(radius::PANE)).border_1().border_color(Tokens::edge()).bg(Tokens::panel())
}
