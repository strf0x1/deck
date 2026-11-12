//! Native typography specimens used by Appearance. All candidates receive
//! identical synthetic text; masking changes glyphs, never the source string.

use super::glyph::glyph;
use super::theme::{Tokens, icon, size, space};
use super::typography::Typography;
use gpui::{Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _, div, px, relative};

pub const DUMMY_KEY: &str = "sk-proj-DEMO0123456789abcdefghijklmnopqrstuvwxyz";
pub const CODE: &str = "let ready = tasks.iter().filter(|t| t.is_ready());";
pub const OUTPUT: &str = "$ cargo test -p deck-acp\ntest result: ok. 48 passed; 0 failed";

pub fn label(text: &'static str) -> Div {
    div().text_size(size::scaled(size::SMALL)).text_color(Tokens::text_dim()).child(text)
}

pub fn surface() -> Div {
    div()
        .min_w_0()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(space::STACK))
        .p(px(space::PAD))
        .rounded(px(super::theme::radius::CARD))
        .border_1()
        .border_color(Tokens::edge())
        .bg(Tokens::raised())
}

pub fn code(family: &'static str, text: impl Into<gpui::SharedString>) -> Div {
    div()
        .min_w_0()
        .font_family(family)
        .text_size(size::scaled(size::SMALL))
        .line_height(relative(1.5))
        .whitespace_normal()
        .child(text.into())
}

pub fn hierarchy(typeface: &Typography) -> impl IntoElement {
    surface()
        .debug_selector(|| "type-hierarchy".into())
        .child(label("Live hierarchy"))
        .child(
            div()
                .text_size(size::scaled(size::DISPLAY))
                .font_weight(FontWeight::SEMIBOLD)
                .child("A quieter place to build."),
        )
        .child(
            div()
                .debug_selector(|| "type-prose".into())
                .text_size(size::scaled(size::BODY))
                .line_height(relative(1.6))
                .child(
                    "The agent has finished its work. Review the changes, run the tests, and decide what ships next.",
                ),
        )
        .child(label("Code and command output samples"))
        .child(code(typeface.code.family(), CODE))
        .child(code(typeface.code.family(), OUTPUT))
        .child(code(typeface.code.family(), format!("OPENAI_API_KEY={DUMMY_KEY}")))
        .child(label("Editable values use the regular build"))
        .child(code(typeface.code.value_family(), DUMMY_KEY).debug_selector(|| "type-value".into()))
        .child(label("Icons are SVG, tinted by the text colour around them"))
        .child(
            div().flex().items_center().gap(px(space::PANE_V)).children(
                [icon::SESSIONS, icon::TERMINAL, icon::SHIELD, icon::CHECK, icon::GEAR, icon::SEND]
                    .map(|icon| glyph(icon, 16.)),
            ),
        )
}
