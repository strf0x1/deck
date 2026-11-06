//! The conversation pane: the live session's title bar, transcript, the
//! permission question when one is open, and the prompt field.
//!
//! The design choice: the transcript is mostly uncoloured. The user's turns
//! carry the accent as a faint wash, tool calls are quiet mono rows whose
//! only colour is their status glyph, and a permission question is the one
//! place amber appears, so a glance at the pane finds what needs attention.

use super::chrome::kbd;
use super::panels::{glyph, pane};
use super::root::Deck;
use super::session::{Entry, Status};
use super::theme::{MONO, Tokens, icon, tint};
use deck_acp::ToolStatus;
use deck_runtime::approvals::Decision;
use gpui::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px, relative,
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
        } else {
            transcript(deck).into_any_element()
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
                .child(if deck.can_restart() { restart(deck, cx) } else { prompt(deck).into_any_element() }),
        )
}

fn transcript(deck: &Deck) -> impl IntoElement {
    let rows = deck.session.entries.iter().map(|e| match e {
        Entry::You(t) => div().flex().justify_end().child(
            div()
                .max_w(relative(0.72))
                .px(px(14.))
                .py(px(9.))
                .rounded(px(14.))
                .border_1()
                .border_color(tint(Tokens::accent(), 0.22))
                .bg(tint(Tokens::accent(), 0.12))
                .child(t.clone()),
        ),
        Entry::Agent(t) => div().max_w(relative(0.86)).line_height(relative(1.5)).child(t.clone()),
        Entry::Thought(t) => div().italic().text_color(Tokens::text_faint()).child(t.clone()),
        Entry::Tool { title, status, .. } => {
            let (g, color) = match status {
                ToolStatus::Running => (icon::SPIN, Tokens::mint()),
                ToolStatus::Done => (icon::CHECK, Tokens::lime()),
                ToolStatus::Failed => (icon::CROSS, Tokens::red()),
            };
            div().flex().child(
                div()
                    .px(px(10.))
                    .py(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(9.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(Tokens::edge())
                    .bg(Tokens::raised())
                    .child(glyph(g, 10.).text_color(color))
                    .child(
                        div().font_family(MONO).text_size(px(11.5)).text_color(Tokens::text_dim()).child(title.clone()),
                    ),
            )
        }
        Entry::Note(t) => div().text_color(Tokens::red()).child(t.clone()),
    });
    let empty = (deck.session.entries.is_empty() && deck.session.status != Status::Detached).then(|| {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .child(div().text_size(px(17.)).font_weight(FontWeight::SEMIBOLD).child("What should we work on?"))
            .child(
                div().text_color(Tokens::text_faint()).child("The agent's work, tool calls and questions land here."),
            )
    });
    div()
        .id("transcript")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .track_scroll(&deck.scroll)
        .flex()
        .flex_col()
        .gap(px(14.))
        .px(px(22.))
        .py(px(18.))
        .children(empty)
        .children(rows)
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

fn prompt(deck: &Deck) -> impl IntoElement {
    let ready = deck.session.can_prompt();
    let has_draft = !deck.draft.is_empty();
    let caret = div()
        .flex_none()
        .w(px(2.))
        .h(px(17.))
        .rounded(px(1.))
        .bg(tint(Tokens::accent(), if ready { 0.9 } else { 0.25 }));
    // A draft is followed by the caret; the hint sits just after it.
    let (first, second) = if has_draft {
        (div().min_w_0().truncate().child(deck.draft.clone()).into_any_element(), caret.into_any_element())
    } else {
        let hint = if ready { "Tell the agent what to do…" } else { "Waiting for the agent…" };
        (caret.into_any_element(), div().pl(px(6.)).text_color(Tokens::text_faint()).child(hint).into_any_element())
    };
    let (send_bg, send_fg): (Hsla, Hsla) = if ready && has_draft {
        (Tokens::accent(), Tokens::ink().into())
    } else {
        (Tokens::raised().into(), Tokens::text_faint().into())
    };
    div()
        .h(px(52.))
        .pl(px(16.))
        .pr(px(8.))
        .flex()
        .items_center()
        .gap(px(1.))
        .rounded(px(16.))
        .border_1()
        .border_color(if ready { Tokens::edge_strong() } else { Tokens::edge() })
        .bg(Tokens::raised())
        .child(first)
        .child(second)
        .child(div().flex_1())
        .child(div().pr(px(10.)).text_size(px(11.)).text_color(Tokens::text_faint()).child("enter to send"))
        .child(
            div()
                .size(px(36.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(11.))
                .bg(send_bg)
                .text_color(send_fg)
                .child(glyph(icon::SEND, 13.)),
        )
}
