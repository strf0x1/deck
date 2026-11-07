//! Grant and pending-request rows in the Access panel. No state of their own.
//!
//! The design choice: a revoke names one daemon-issued grant id. Review
//! returns to the existing boundary card, so the panel does not introduce a
//! second approval flow or let hidden keyboard shortcuts decide a request.

use super::access::grant_scope;
use super::access_panel::{button, error};
use super::root::Deck;
use super::theme::{MONO, Tokens, radius, size, space};
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

pub(super) fn sections(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |time| time.as_millis() as u64);
    let grants = deck.access.grants().iter().enumerate().map(|(i, grant)| {
        let id = grant.id.clone();
        div()
            .flex()
            .items_center()
            .gap(px(space::SECTION))
            .p(px(space::INSET))
            .rounded(px(radius::CONTROL))
            .bg(Tokens::raised())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .gap(px(space::DETAIL))
                    .child(div().font_family(MONO).text_size(px(size::LABEL)).child(grant.destination.clone()))
                    .child(div().text_size(px(size::SMALL)).text_color(Tokens::text_dim()).child(format!(
                        "{} · {} · {}",
                        grant.gateway,
                        grant.operation,
                        grant_scope(grant, now)
                    ))),
            )
            .child(
                button(("access-grant-revoke", i), "Revoke", Tokens::red())
                    .debug_selector(move || format!("access-grant-revoke-{i}"))
                    .on_click(cx.listener(move |this, _, _, cx| this.ask_revoke_grant(&id, cx))),
            )
    });
    let pending = deck.boundary.items().iter().enumerate().map(|(i, item)| {
        let id = item.id.clone();
        div()
            .flex()
            .items_center()
            .gap(px(space::SECTION))
            .p(px(space::INSET))
            .rounded(px(radius::CONTROL))
            .bg(Tokens::raised())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .gap(px(space::DETAIL))
                    .child(div().font_family(MONO).text_size(px(size::LABEL)).child(item.destination.clone()))
                    .child(div().text_size(px(size::SMALL)).text_color(Tokens::text_dim()).child(format!(
                        "{} · {} · {}",
                        item.gateway,
                        item.operation,
                        if item.waiters > 0 { "Agent is waiting" } else { "Agent moved on" }
                    ))),
            )
            .child(
                button(("access-review", i), "Review", Tokens::text_dim())
                    .debug_selector(move || format!("access-review-{i}"))
                    .on_click(cx.listener(move |this, _, _, cx| this.review_approval(&id, cx))),
            )
    });
    div()
        .flex()
        .flex_col()
        .gap(px(18.))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(space::LINE))
                .child(div().font_weight(FontWeight::MEDIUM).child("Pending requests"))
                .children(
                    deck.boundary
                        .items()
                        .is_empty()
                        .then(|| div().text_color(Tokens::text_faint()).child("No pending requests.")),
                )
                .children(pending),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(space::LINE))
                .child(div().font_weight(FontWeight::MEDIUM).child("Additional access"))
                .child(
                    div()
                        .text_size(px(size::SMALL))
                        .text_color(Tokens::text_faint())
                        .child("Grants for this task and saved rules for its project."),
                )
                .children((deck.access.grants().is_empty() && deck.access.grants_error().is_none()).then(|| {
                    div().text_color(Tokens::text_faint()).child(if deck.access.grants_loading() {
                        "Loading access…"
                    } else {
                        "No additional grants."
                    })
                }))
                .children(grants)
                .children(deck.access.grants_error().map(error)),
        )
        .into_any_element()
}
