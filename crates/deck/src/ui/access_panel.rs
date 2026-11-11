//! Renders the access panel from [`Access`](super::access::Access). It takes
//! the conversation pane's body while open. No state of its own.
//!
//! The design choice: red appears only on what revokes (the button, the
//! confirmation) and amber on what is unguarded or still the user's to do
//! (a local process, the reminder after a revoke); a sealed VM is green.

use super::access::{NOT_CANCELLED, Revoke};
use super::panels::glyph;
use super::root::Deck;
use super::theme::{MONO, Tokens, icon, radius, size, space, tint};
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
            let retry_name = item.name.clone();
            let can_revoke = item.fingerprint.is_some() && !item.revoked;
            let retrying = access.retrying() == Some(item.name.as_str());
            let (label, color): (&str, Hsla) = if item.revoked {
                ("revoked", Tokens::red())
            } else if item.fingerprint.is_some() {
                ("active", Tokens::lime())
            } else if item.paused {
                ("paused", Tokens::text_dim().into())
            } else {
                ("no value", Tokens::text_faint().into())
            };
            let detail = item.fingerprint.clone().or_else(|| item.error.clone()).unwrap_or_default();
            div()
                .debug_selector(move || format!("access-credential-row-{i}"))
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(space::TIGHT))
                .px(px(space::INSET))
                .py(px(9.))
                .rounded(px(radius::CARD))
                .border_1()
                .border_color(Tokens::edge())
                .bg(Tokens::raised())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(space::SECTION))
                        .child(glyph(icon::KEY, 11.).text_color(color))
                        .child(
                            div()
                                .debug_selector(move || format!("access-credential-name-{i}"))
                                .flex_1()
                                .min_w_0()
                                .font_family(MONO)
                                .text_size(px(size::LABEL))
                                .child(item.name.clone()),
                        )
                        .child(
                            div()
                                .debug_selector(move || format!("access-credential-status-{i}"))
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(space::TIGHT))
                                .text_size(px(size::SMALL))
                                .text_color(color)
                                .child(div().size(px(6.)).rounded_full().bg(color))
                                .child(label),
                        )
                        .children(can_revoke.then(|| {
                            button(("access-revoke", i), "Revoke", Tokens::red())
                                .debug_selector(move || format!("access-revoke-{i}"))
                                .flex_none()
                                .on_click(cx.listener(move |this, _, _, cx| this.ask_revoke(&name, cx)))
                        }))
                        .children(item.paused.then(|| {
                            button(
                                ("access-retry", i),
                                if retrying { "Retrying…" } else { "Retry" },
                                Tokens::text_dim(),
                            )
                            .debug_selector(move || format!("access-retry-{i}"))
                            .flex_none()
                            .on_click(cx.listener(move |this, _, _, cx| this.retry_credential(&retry_name, cx)))
                        })),
                )
                .child(
                    div()
                        .debug_selector(move || format!("access-credential-detail-{i}"))
                        .w_full()
                        .whitespace_normal()
                        .font_family(MONO)
                        .text_size(px(size::SMALL))
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
        .gap(px(space::ROW))
        .child(super::access_grants::sections(deck, cx))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(space::LINE))
                .child(super::panels::eyebrow("Credentials"))
                .child(
                    div()
                        .text_size(px(size::SMALL))
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
        .gap(px(space::ROW))
        .px(px(space::PANE))
        .py(px(space::PANE_V))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(space::STACK))
                .child(super::panels::icon_tile(icon::SHIELD, Tokens::violet(), 26.))
                .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(size::TITLE)).child("Access"))
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
        .children(deck.current().task.as_ref().map(|task| {
            // Sealed is green; an unrestricted host process is a caution.
            let (g, color, text) = if task.boundary_enforced() {
                (icon::LOCK, Tokens::lime(), "VM · external access is enforced by host gateways.")
            } else {
                (icon::WARN, Tokens::amber(), "Local process · host network access is not restricted by these grants.")
            };
            div()
                .flex_none()
                .px(px(space::INSET))
                .py(px(space::LINE))
                .flex()
                .items_center()
                .gap(px(space::LINE))
                .rounded(px(radius::CONTROL))
                .border_1()
                .border_color(tint(color, 0.25))
                .bg(tint(color, 0.06))
                .text_size(px(size::SMALL))
                .text_color(Tokens::text_dim())
                .child(glyph(g, 10.).text_color(color))
                .child(text)
        }))
        .children(access.approvals().map(approval_mode))
        .children(access.approvals_error().map(error))
        .children(access.confirming().map(|target| confirm(target, access.busy(), cx)))
        .children(access.revoked().map(notice))
        .children(access.revoke_error().map(error))
        .children(access.retry_message().map(|message| {
            div().flex_none().text_size(px(size::SMALL)).text_color(Tokens::text_dim()).child(message.to_owned())
        }))
        .child(contents)
        .into_any_element()
}

/// The task's GitHub approval mode and what it means for the next write.
fn approval_mode(mode: deck_runtime::github::Approvals) -> AnyElement {
    use deck_runtime::github::Approvals;
    let (label, detail) = match mode {
        Approvals::OwnBranch => (
            "own-branch",
            "This task's branch, its draft pull request, and updates to it need no card. \
             Ready for review, shared branches and public repositories still ask.",
        ),
        Approvals::Ask => ("ask", "Every GitHub write asks for a card."),
        Approvals::OwnBranchPublic => (
            "own-branch-public",
            "As own-branch, and a public repository does not ask. \
             Pushing there discloses the branch to everyone.",
        ),
    };
    div()
        .debug_selector(|| "access-approvals".into())
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(3.))
        .child(
            div().text_size(px(size::LABEL)).text_color(Tokens::text_dim()).child(format!("GitHub approvals: {label}")),
        )
        .child(div().text_size(px(size::SMALL)).text_color(Tokens::text_faint()).child(detail))
        .into_any_element()
}

fn confirm(target: &Revoke, busy: bool, cx: &mut Context<Deck>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::STACK))
        .p(px(space::PAD))
        .rounded(px(radius::BUBBLE))
        .border_1()
        .border_color(tint(Tokens::red(), 0.35))
        .bg(tint(Tokens::red(), 0.06))
        .flex_none()
        .child(div().font_weight(FontWeight::MEDIUM).child(format!("Revoke {}?", target.label())))
        .child(div().text_size(px(size::LABEL)).text_color(Tokens::text_dim()).child(target.explanation()))
        .child(
            div()
                .flex()
                .gap(px(space::LINE))
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
        .gap(px(space::TIGHT))
        .p(px(space::PAD))
        .rounded(px(radius::BUBBLE))
        .border_1()
        .border_color(tint(Tokens::amber(), 0.35))
        .bg(tint(Tokens::amber(), 0.06))
        .flex_none()
        .child(div().font_weight(FontWeight::MEDIUM).child(format!("{} revoked", target.label())))
        .child(div().text_size(px(size::LABEL)).text_color(Tokens::text_dim()).child(text))
        .into_any_element()
}

pub(super) fn button(id: impl Into<gpui::ElementId>, label: &str, fg: impl Into<Hsla>) -> gpui::Stateful<gpui::Div> {
    let fg = fg.into();
    div()
        .id(id.into())
        .h(px(28.))
        .px(px(space::INSET))
        .flex()
        .items_center()
        .rounded(px(radius::CONTROL))
        .border_1()
        .border_color(tint(fg, 0.3))
        .bg(tint(fg, 0.06))
        .hover(move |style| style.bg(tint(fg, 0.14)))
        .text_color(fg)
        .text_size(px(size::LABEL))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child(label.to_owned())
}

pub(super) fn error(why: &str) -> AnyElement {
    div()
        .flex_none()
        .flex()
        .gap(px(space::LINE))
        .text_color(Tokens::red())
        .child(glyph(icon::WARN, 11.).mt(px(space::HAIR)))
        .child(div().flex_1().min_w_0().child(why.to_owned()))
        .into_any_element()
}
