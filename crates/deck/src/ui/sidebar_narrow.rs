//! The narrow sidebar: the needs count badge and one glyph per project
//! (ADR 0010 §1, §7).
//!
//! The design choice: the glyphs come whole from `sidebar::narrow`, so this
//! file only draws them; the open view and the shell are `sidebar_view.rs`.

use super::rail::{COLLAPSED, ITEM_H};
use super::root::Deck;
use super::sidebar::Glyph;
use super::theme::{Tokens, glow, radius, size, space, tint};
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

/// The narrow sidebar: the needs count badge, then one glyph per project.
pub(super) fn narrow_view(glyphs: &[Glyph], marker: f32, cx: &mut Context<Deck>) -> AnyElement {
    let items: Vec<AnyElement> = glyphs.iter().map(|g| glyph_row(g, cx)).collect();
    div()
        .relative()
        .flex()
        .flex_col()
        .child(
            div()
                .absolute()
                .top(px(marker))
                .left_0()
                .right_0()
                .h(px(ITEM_H))
                .rounded(px(radius::CONTROL))
                .bg(Tokens::raised())
                .child(
                    div()
                        .absolute()
                        .left(px(-6.))
                        .top(px(12.))
                        .w(px(3.))
                        .h(px(16.))
                        .rounded(px(radius::MARK))
                        .bg(Tokens::accent())
                        .shadow(glow(Tokens::accent(), 1.)),
                ),
        )
        .children(items)
        .into_any_element()
}

fn glyph_row(g: &Glyph, cx: &mut Context<Deck>) -> AnyElement {
    let fg = if g.selected { Tokens::text() } else { Tokens::text_dim() };
    let index = g.index;
    div()
        .id(("sidebar-glyph", index))
        .debug_selector(move || format!("sidebar-glyph-{index}"))
        .relative()
        .w(px(COLLAPSED - 12.))
        .h(px(ITEM_H))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::CONTROL))
        .cursor_pointer()
        .text_color(fg)
        .on_click(cx.listener(move |this, _, _, cx| this.select_project(index, cx)))
        .child(div().relative().child(tile(g.letter, Tokens::hue(&g.name), g.selected, 32.)).children(
            g.attention.then(|| {
                div()
                    .absolute()
                    .top(px(-2.))
                    .right(px(-2.))
                    .size(px(7.))
                    .rounded_full()
                    .bg(Tokens::amber())
                    .shadow(glow(Tokens::amber(), 1.))
            }),
        ))
        .into_any_element()
}

/// A project's letter tile in the project's own hue: the narrow sidebar's,
/// and the open project header's at a smaller size (ADR 0010 §9).
pub(super) fn tile(letter: char, hue: gpui::Hsla, selected: bool, size: f32) -> gpui::Div {
    let font = if size >= 32. { size::TITLE } else { size::SMALL };
    div()
        .flex_none()
        .size(px(size))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(if size >= 32. { radius::CONTROL } else { radius::CHIP }))
        .border_1()
        .border_color(tint(hue, if selected { 0.55 } else { 0.22 }))
        .bg(super::theme::Tokens::well())
        .text_color(tint(hue, if selected { 1. } else { 0.8 }))
        .child(div().font_weight(FontWeight::BOLD).text_size(size::scaled(font)).child(letter.to_string()))
}

pub(super) fn count_badge(count: usize) -> AnyElement {
    div()
        .debug_selector(|| "sidebar-count".into())
        .h(px(ITEM_H))
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .px(px(space::TIGHT))
                .h(px(22.))
                .flex()
                .items_center()
                .rounded_full()
                .bg(Tokens::amber())
                .shadow(glow(Tokens::amber(), 0.8))
                .text_size(size::scaled(size::SMALL))
                .text_color(Tokens::ink())
                .font_weight(FontWeight::SEMIBOLD)
                .child(count.to_string()),
        )
        .into_any_element()
}
