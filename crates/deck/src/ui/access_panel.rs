//! Renders the access panel from [`Access`](super::access::Access). It takes
//! the conversation pane's body while open. No state of its own.
//!
//! The design choice: red appears only on what revokes (the button, the
//! confirmation) and amber only on the reminder that follows, so the panel
//! stays quiet until a decision is on it.

use super::access::{NOT_CANCELLED, Revoke};
use super::panels::glyph;
use super::root::Deck;
use super::theme::{MONO, Tokens, icon, tint};
use gpui::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

pub fn panel(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let access = &deck.access;
    let rows = access
        .credentials()
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let name = item.name.clone();
            let can_revoke = item.fingerprint.is_some() && !item.revoked;
            let (label, color): (&str, Hsla) = if item.revoked {
                ("revoked", Tokens::red())
            } else if item.fingerprint.is_some() {
                ("active", Tokens::lime())
            } else {
                ("no value", Tokens::text_faint().into())
            };
            let detail = item.fingerprint.clone().or_else(|| item.error.clone()).unwrap_or_default();
            div()
                .debug_selector(move || format!("access-credential-row-{i}"))
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(6.))
                .px(px(12.))
                .py(px(9.))
                .rounded(px(10.))
                .bg(Tokens::raised())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .child(
                            div()
                                .debug_selector(move || format!("access-credential-name-{i}"))
                                .flex_1()
                                .min_w_0()
                                .font_family(MONO)
                                .text_size(px(12.5))
                                .child(item.name.clone()),
                        )
                        .child(
                            div()
                                .debug_selector(move || format!("access-credential-status-{i}"))
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .text_size(px(11.5))
                                .text_color(Tokens::text_dim())
                                .child(div().size(px(6.)).rounded_full().bg(color))
                                .child(label),
                        )
                        .children(can_revoke.then(|| {
                            button(("access-revoke", i), "Revoke", Tokens::red())
                                .debug_selector(move || format!("access-revoke-{i}"))
                                .flex_none()
                                .on_click(cx.listener(move |this, _, _, cx| this.ask_revoke(&name, cx)))
                        })),
                )
                .child(
                    div()
                        .debug_selector(move || format!("access-credential-detail-{i}"))
                        .w_full()
                        .whitespace_normal()
                        .font_family(MONO)
                        .text_size(px(11.5))
                        .text_color(Tokens::text_faint())
                        .child(detail),
                )
        })
        .collect::<Vec<_>>();
    let empty = (access.credentials().is_empty() && access.credentials_error().is_none()).then(|| {
        div().text_color(Tokens::text_faint()).child(if access.credentials_loading() {
            "Loading credentials…"
        } else {
            "This task has no brokered credentials."
        })
    });
    let contents = div()
        .id("access-body")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(14.))
        .child(super::access_grants::sections(deck, cx))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(div().font_weight(FontWeight::MEDIUM).child("Credentials"))
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(Tokens::text_faint())
                        .child("Values stay on the host. This lists names and fingerprints only."),
                )
                .children(empty)
                .children(rows)
                .children(access.credentials_error().map(error)),
        );
    div()
        .id("access")
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .gap(px(14.))
        .px(px(22.))
        .py(px(18.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(glyph(icon::SHIELD, 13.).text_color(Tokens::text_dim()))
                .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(15.)).child("Access"))
                .child(div().flex_1())
                .child(
                    button("access-refresh", "Refresh", Tokens::text_dim())
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_access(cx))),
                )
                .child(
                    button("access-close", "Close", Tokens::text_dim())
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_access(cx))),
                ),
        )
        .children(deck.task.as_ref().map(|task| {
            div().flex_none().text_size(px(11.5)).text_color(Tokens::text_faint()).child(if task.boundary_enforced() {
                "VM · external access is enforced by host gateways."
            } else {
                "Local process · host network access is not restricted by these grants."
            })
        }))
        .children(access.confirming().map(|target| confirm(target, access.busy(), cx)))
        .children(access.revoked().map(notice))
        .children(access.revoke_error().map(error))
        .child(contents)
        .into_any_element()
}

fn confirm(target: &Revoke, busy: bool, cx: &mut Context<Deck>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(10.))
        .p(px(14.))
        .rounded(px(14.))
        .border_1()
        .border_color(tint(Tokens::red(), 0.35))
        .bg(tint(Tokens::red(), 0.06))
        .flex_none()
        .child(div().font_weight(FontWeight::MEDIUM).child(format!("Revoke {}?", target.label())))
        .child(div().text_size(px(12.5)).text_color(Tokens::text_dim()).child(target.explanation()))
        .child(
            div()
                .flex()
                .gap(px(8.))
                .child(
                    button("access-revoke-confirm", if busy { "Revoking…" } else { "Revoke" }, Tokens::red())
                        .debug_selector(|| "access-revoke-confirm".into())
                        .on_click(cx.listener(|this, _, _, cx| this.confirm_revoke(cx))),
                )
                .child(
                    button("access-revoke-cancel", "Cancel", Tokens::text_dim())
                        .debug_selector(|| "access-revoke-cancel".into())
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_revoke(cx))),
                ),
        )
        .into_any_element()
}

/// After a revoke: the part that is still the user's to do.
fn notice(target: &Revoke) -> AnyElement {
    let text = match target {
        Revoke::Credential(name) => format!(
            "{NOT_CANCELLED} Rotate it at the provider, then set the new value with `deckd credential set {name}`."
        ),
        Revoke::Grant(_) => "New matching requests need approval. Completed operations cannot be undone.".into(),
    };
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .p(px(14.))
        .rounded(px(14.))
        .border_1()
        .border_color(tint(Tokens::amber(), 0.35))
        .bg(tint(Tokens::amber(), 0.06))
        .flex_none()
        .child(div().font_weight(FontWeight::MEDIUM).child(format!("{} revoked", target.label())))
        .child(div().text_size(px(12.5)).text_color(Tokens::text_dim()).child(text))
        .into_any_element()
}

pub(super) fn button(id: impl Into<gpui::ElementId>, label: &str, fg: impl Into<Hsla>) -> gpui::Stateful<gpui::Div> {
    let fg = fg.into();
    div()
        .id(id.into())
        .h(px(28.))
        .px(px(12.))
        .flex()
        .items_center()
        .rounded(px(9.))
        .border_1()
        .border_color(tint(fg, 0.3))
        .bg(Tokens::raised())
        .text_color(fg)
        .text_size(px(12.5))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child(label.to_owned())
}

pub(super) fn error(why: &str) -> AnyElement {
    div().text_color(Tokens::red()).child(why.to_owned()).into_any_element()
}
