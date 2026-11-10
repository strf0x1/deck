//! Render helpers for the small shared pieces other regions use: icon glyphs,
//! state colours and panes.
//!
//! The design choice: free functions, not separate entities. Nothing here owns
//! state; every interaction calls back into `Deck`.

use super::board::LaneState;
use super::theme::{MONO, Tokens, radius};
use gpui::{Div, Hsla, ParentElement as _, Styled as _, div, px};

pub fn state_color(s: LaneState) -> Hsla {
    match s {
        LaneState::Idle => Tokens::text_faint().into(),
        LaneState::Working => Tokens::mint(),
        LaneState::Waiting => Tokens::amber(),
        LaneState::Review => Tokens::lime(),
    }
}

/// A Nerd Font icon at `size` pixels.
pub fn glyph(g: &'static str, size: f32) -> Div {
    div().font_family(MONO).text_size(px(size)).line_height(px(size + 2.)).child(g)
}

/// One pane of glass: the conversation sits on this.
pub fn pane() -> Div {
    div().relative().rounded(px(radius::PANE)).border_1().border_color(Tokens::edge()).bg(Tokens::panel())
}
