//! The "Needs you" strip across the top of the window (ADR 0010 §7).
//!
//! The design choice: one row that never disappears. With nothing pending it
//! shrinks to a quiet line, so its absence never reads as broken; items scroll
//! sideways rather than wrapping, oldest first.

use super::root::Deck;
use super::theme::{Tokens, radius, size, space, tint};
use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, StatefulInteractiveElement as _,
    Styled as _, div, px,
};

/// The whole strip: every project's attention items, oldest first.
pub fn strip(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let items = deck.needs_you();
    if items.is_empty() {
        return div()
            .id("needs-you-empty")
            .debug_selector(|| "needs-you-empty".into())
            .h(px(30.))
            .px(px(space::INSET))
            .flex()
            .items_center()
            .text_size(px(size::SMALL))
            .text_color(Tokens::text_faint())
            .child("Nothing needs you")
            .into_any_element();
    }
    let pills = items.iter().enumerate().map(|(index, item)| {
        let project = deck
            .projects()
            .iter()
            .find(|project| project.id == item.project)
            .map_or_else(|| item.project.clone(), |project| project.name.clone());
        let title = deck.task_title(&item.task);
        let reason = item.reason.text();
        div()
            .id(("needs", index))
            .debug_selector(move || format!("needs-{index}"))
            .flex_none()
            .h(px(30.))
            .px(px(space::INSET))
            .flex()
            .items_center()
            .gap(px(space::LINE))
            .rounded(px(radius::CONTROL))
            .border_1()
            .border_color(tint(Tokens::amber(), 0.3))
            .bg(Tokens::raised())
            .hover(|style| style.bg(Tokens::raised_hover()))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.open_item(index, cx)))
            .child(div().size(px(6.)).rounded_full().bg(Tokens::amber()))
            .child(div().text_size(px(size::SMALL)).text_color(Tokens::text_dim()).child(project))
            .child(div().text_size(px(size::SMALL)).text_color(Tokens::text()).child(title))
            .child(div().text_size(px(size::SMALL)).text_color(Tokens::amber()).child(reason))
            .into_any_element()
    });
    div()
        .id("needs-you")
        .debug_selector(|| "needs-you".into())
        .h(px(36.))
        .flex()
        .items_center()
        .gap(px(space::TIGHT))
        .overflow_x_scroll()
        .children(pills)
        .into_any_element()
}
