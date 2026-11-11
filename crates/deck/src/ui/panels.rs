//! Render helpers for the small shared pieces other regions use: icon glyphs,
//! state colours and marks, tinted tiles and pills, the two button weights
//! and panes.
//!
//! The design choice: free functions, not separate entities. Nothing here owns
//! state; every interaction calls back into `Deck`. A colour arrives as an
//! argument, so each caller says what the hue means where it is used.

use super::board::LaneState;
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
/// colour. It does not glow: a task the window is not showing reports only
/// that its agent is running, not that a turn is.
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
        .size(px(size + 8.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(tint(color, if lit { 0.16 } else { 0.08 }))
        .text_color(color)
        .child(glyph(g, size))
}

/// A Nerd Font icon at `size` pixels.
pub fn glyph(g: &'static str, size: f32) -> Div {
    div().font_family(super::theme::ICONS).text_size(px(size)).line_height(px(size + 2.)).child(g)
}

/// An icon on a square wash of its own colour: what kind of thing a row is.
pub fn icon_tile(g: &'static str, color: Hsla, side: f32) -> Div {
    div()
        .flex_none()
        .size(px(side))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::CHIP))
        .bg(tint(color, 0.14))
        .text_color(color)
        .child(glyph(g, (side * 0.5).round()))
}

/// A status pill: a dot and a word in the state's colour, on a wash of it.
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
        .border_color(tint(color, 0.28))
        .bg(tint(color, 0.1))
        .text_size(size::scaled(size::SMALL))
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .child(div().size(px(6.)).rounded_full().bg(color).shadow(glow(color, 0.8)))
        .child(label.into())
}

/// The one loud button in a region: the user's own colour, lit.
pub fn primary() -> Div {
    div()
        .min_h(px(32.))
        .px(px(space::PAD))
        .flex()
        .items_center()
        .gap(px(space::LINE))
        .rounded(px(radius::CONTROL))
        .bg(Tokens::sweep(1.))
        .shadow(glow(Tokens::accent(), 0.7))
        .text_color(Tokens::ink())
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
