//! The conversation pane: the live session's title bar, transcript, the
//! permission question when one is open, and the prompt field.
//!
//! The design choice: the transcript is mostly uncoloured. The user's turns
//! carry the accent as a faint wash, tool cards spend colour only on their
//! status glyph and diff lines, and a permission question is the one place
//! amber appears, so a glance at the pane finds what needs attention.

use super::chrome::kbd;
use super::panels::{glyph, pane};
use super::root::Deck;
use super::session::Status;
use super::theme::{MONO, Tokens, icon, tint};
use deck_runtime::approvals::Decision;
use gpui::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

pub fn main(deck: &Deck, cx: &mut Context<Deck>) -> impl IntoElement {
    let s = &deck.session;
    let (status, color): (&str, Hsla) = match &s.status {
        _ if s.ask.is_some() => ("Needs you", Tokens::amber()),
        Status::Connecting => ("Connecting", Tokens::text_faint().into()),
        Status::Ready => ("Ready", Tokens::lime()),
        Status::Waiting => ("Waiting", Tokens::amber()),
        Status::Working => ("Working", Tokens::mint()),
        Status::Detached => ("Running, detached", Tokens::mint()),
        Status::Down(_) => ("Down", Tokens::red()),
    };
    pane()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(56.))
                .px(px(20.))
                .flex()
                .items_center()
                .gap(px(10.))
                .border_b_1()
                .border_color(Tokens::edge())
                .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(s.agent.clone()))
                .child(div().text_size(px(12.)).text_color(Tokens::text_faint()).child("live session"))
                .child(div().flex_1())
                .child(
                    div()
                        .h(px(26.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .gap(px(7.))
                        .rounded_full()
                        .bg(Tokens::raised())
                        .text_size(px(11.5))
                        .text_color(Tokens::text_dim())
                        .child(div().size(px(6.)).rounded_full().bg(color))
                        .child(status),
                ),
        )
        .child(if deck.access.is_open() {
            super::access_panel::panel(deck, cx)
        } else if deck.open_publication().is_some() {
            super::publication_panel::panel(deck, cx)
        } else {
            super::transcript_list::transcript(deck, cx)
        })
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .p(px(14.))
                .pt(px(4.))
                .children(deck.boundary().pending().map(|_| boundary(deck, cx)))
                .children(s.ask.as_ref().map(|_| ask(deck, cx)))
                // Access takes the keyboard, so the field it would block is not shown.
                .children((!deck.access.is_open()).then(|| {
                    if deck.can_restart() {
                        restart(deck, cx)
                    } else {
                        super::prompt::prompt(deck, cx).into_any_element()
                    }
                })),
        )
}

/// The host boundary card: an external operation that needs a human
/// decision. It is not an ACP permission; the daemon holds the request and
/// the agent learns the outcome through its own gateway error.
fn boundary(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let Some(item) = deck.boundary().pending() else { return div().into_any_element() };
    let options = [
        (Decision::Once, "Allow once"),
        (Decision::Timed, "Allow for 5 minutes"),
        (Decision::Task, "Allow for this task"),
        (Decision::Project, "Always for this project"),
        (Decision::Deny, "Deny"),
    ];
    let buttons = options.iter().enumerate().map(|(i, (decision, name))| {
        let primary = i == 0;
        let deny = *decision == Decision::Deny;
        let (bg, fg, edge): (Hsla, Hsla, Hsla) = if primary {
            (Tokens::text().into(), Tokens::ink().into(), Tokens::text().into())
        } else if deny {
            (Tokens::raised().into(), Tokens::red(), tint(Tokens::red(), 0.3))
        } else {
            (Tokens::raised().into(), Tokens::text_dim().into(), Tokens::edge().into())
        };
        let decision = *decision;
        div()
            .id(("boundary-option", i))
            .h(px(30.))
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(8.))
            .rounded(px(9.))
            .border_1()
            .border_color(edge)
            .bg(bg)
            .text_color(fg)
            .text_size(px(12.5))
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .child(*name)
            .child(div().font_family(MONO).text_size(px(10.)).opacity(0.55).child(format!("{}", i + 1)))
            .on_click(cx.listener(move |this, _, _, cx| this.decide_boundary(decision, cx)))
    });
    let detail = format!(
        "{}{}",
        item.details.method.as_deref().unwrap_or(""),
        item.details.path.as_deref().map(|path| format!(" {path}")).unwrap_or_default()
    );
    div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .p(px(14.))
        .rounded(px(14.))
        .border_1()
        .border_color(tint(Tokens::amber(), 0.35))
        .bg(tint(Tokens::amber(), 0.06))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(glyph(icon::SHIELD, 13.).text_color(Tokens::amber()))
                .child(div().font_weight(FontWeight::MEDIUM).child("External access needs you"))
                .child(div().flex_1())
                .child(kbd("1–5")),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(10.))
                .py(px(7.))
                .rounded(px(8.))
                .bg(Tokens::raised())
                .font_family(MONO)
                .text_size(px(12.))
                .child(format!("{} · {}", item.gateway, item.destination))
                .children(item.credential.as_ref().map(|credential| {
                    div().text_color(Tokens::text_faint()).child(format!("credential {credential}"))
                })),
        )
        .children(item.details.summary.as_ref().map(|summary| {
            div()
                .debug_selector(|| "boundary-summary".into())
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(div().text_size(px(10.5)).text_color(Tokens::text_faint()).child("the agent's description"))
                .child(div().text_size(px(12.)).text_color(Tokens::text_dim()).child(format!(
                    "{} · {} commit{}",
                    summary.subject,
                    summary.count,
                    if summary.count == 1 { "" } else { "s" }
                )))
        }))
        .children(
            (!detail.trim().is_empty())
                .then(|| div().text_size(px(11.5)).text_color(Tokens::text_faint()).font_family(MONO).child(detail)),
        )
        .children(
            item.reason
                .as_ref()
                .map(|reason| div().text_size(px(12.)).text_color(Tokens::text_dim()).child(reason.clone())),
        )
        .child(div().text_size(px(11.5)).text_color(Tokens::text_faint()).child(if item.waiters > 0 {
            "the agent is waiting for this decision"
        } else {
            "the agent has moved on"
        }))
        .child(div().flex().flex_wrap().gap(px(8.)).children(buttons))
        .into_any_element()
}

fn ask(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let Some(ask) = &deck.session.ask else { return div().into_any_element() };
    let first_allow = ask.options.iter().position(|o| o.allows());
    let buttons = ask.options.iter().enumerate().map(|(i, o)| {
        let primary = Some(i) == first_allow;
        let (bg, fg, edge): (Hsla, Hsla, Hsla) = if primary {
            (Tokens::text().into(), Tokens::ink().into(), Tokens::text().into())
        } else {
            (Tokens::raised().into(), Tokens::text_dim().into(), Tokens::edge().into())
        };
        div()
            .id(("opt", i))
            .h(px(30.))
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(8.))
            .rounded(px(9.))
            .border_1()
            .border_color(edge)
            .bg(bg)
            .text_color(fg)
            .text_size(px(12.5))
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .child(o.name.clone())
            .child(div().font_family(MONO).text_size(px(10.)).opacity(0.55).child(format!("{}", i + 1)))
            .on_click(cx.listener(move |this, _, _, cx| this.choose(Some(i), cx)))
    });
    div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .p(px(14.))
        .rounded(px(14.))
        .border_1()
        .border_color(tint(Tokens::amber(), 0.35))
        .bg(tint(Tokens::amber(), 0.06))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(glyph(icon::SHIELD, 13.).text_color(Tokens::amber()))
                .child(div().font_weight(FontWeight::MEDIUM).child(format!("{} wants to run", deck.session.agent)))
                .child(div().flex_1())
                .child(kbd("esc")),
        )
        .child(
            div()
                .px(px(10.))
                .py(px(7.))
                .rounded(px(8.))
                .bg(Tokens::raised())
                .font_family(MONO)
                .text_size(px(12.))
                .child(ask.title.clone()),
        )
        .child(div().flex().gap(px(8.)).children(buttons))
        .into_any_element()
}

fn restart(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let title = if deck.session.status == Status::Detached {
        "Your task is still running."
    } else {
        "The task connection needs to be restored."
    };
    div()
        .flex()
        .flex_col()
        .gap(px(10.))
        .p(px(14.))
        .rounded(px(14.))
        .bg(Tokens::raised())
        .child(title)
        .child(
            div()
                .text_color(Tokens::text_dim())
                .child("Restart to restore the conversation. Any unfinished turn will be interrupted."),
        )
        .child(
            div()
                .id("restart-task")
                .flex()
                .items_center()
                .gap(px(10.))
                .cursor_pointer()
                .text_color(Tokens::accent())
                .child("Stop and restart")
                .child(kbd(crate::app::RESTART_HINT))
                .on_click(cx.listener(|this, _, _, cx| this.restart(cx))),
        )
        .into_any_element()
}
