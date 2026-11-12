//! Render helpers for the small shared pieces other regions use: icon marks,
//! state colours, pills, the two button weights and panes.
//!
//! The design choice: free functions, not separate entities. Nothing here owns
//! state; every interaction calls back into `Deck`. A colour arrives as an
//! argument, so each caller says what the hue means where it is used.

use super::board::LaneState;
use super::glyph::glyph;
use super::theme::{MONO, Tokens, glow, icon, radius, size, space, tint};
use gpui::{Div, FontWeight, Hsla, ParentElement as _, Styled as _, div, px};

pub fn state_color(s: LaneState) -> Hsla {
    match s {
        LaneState::Idle => Tokens::text_faint().into(),
        LaneState::Working => Tokens::cyan(),
        LaneState::Waiting => Tokens::amber(),
        LaneState::Review => Tokens::lime(),
    }
}

/// A task's state as a mark: a shape as well as a hue, so it reads without
/// colour. It does not glow, and nothing frames it — the shape is the whole
/// mark. A task the window is not showing reports only that its agent is
/// running, not that a turn is.
pub fn state_mark(s: LaneState, size: f32) -> Div {
    let (g, lit) = match s {
        LaneState::Idle => (icon::RING, false),
        LaneState::Working => (icon::BOLT, true),
        LaneState::Waiting => (icon::BELL, true),
        LaneState::Review => (icon::CHECK, false),
    };
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
