//! Renders the coordinator's Limits panel from [`Limits`](super::limits):
//! its caps, budgets and command, with Save, a refusal inline and the note
//! that a command change applies to the next coordinator. It takes the
//! conversation pane's body while open. No state of its own.
//!
//! The design choice: caps wait on a human when met, so the panel's mark is
//! amber, like the cap row that opens it; the focused field is the composer's
//! own element, so typing, IME and moves work as in the prompt. The fields
//! scroll and Save does not: it stays under them at any window height.

use super::access_panel::{button, error};
use super::composer;
use super::glyph::glyph;
use super::limits::Field;
use super::panels::{eyebrow, icon_mark};
use super::root::Deck;
use super::theme::{MONO, Tokens, icon, radius, size, space, tint};
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px,
};

pub fn panel(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let limits = deck.limits();
    let command_only = limits.command_only();
    let title = if command_only { "Coordinator command" } else { "Coordinator limits" };
    let description = if command_only {
        "Choose the coordinator's command and model before its first prompt. Workers keep the project's agent command."
    } else {
        "Only you change these; the coordinator has no way to. A refused create is reported, never queued."
    };
    let command_note = if command_only {
        "Saving configures this project without starting a task. Leave the command empty to use construct acp."
    } else {
        "A command change applies to the next coordinator; this one keeps the command it started with."
    };
    let row = |field: Field, cx: &mut Context<Deck>| field_row(deck, field, cx);
    let fields = div()
        .id("limits")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(space::STACK))
        .p(px(space::PAD))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(space::STACK))
                .child(icon_mark(icon::GAUGE, Tokens::amber(), 26.))
                .child(
                    div().flex_1().flex().flex_col().child(div().font_weight(FontWeight::SEMIBOLD).child(title)).child(
                        div().text_size(size::scaled(size::SMALL)).text_color(Tokens::text_faint()).child(description),
                    ),
                )
                .child(
                    button("limits-close", "Close", Tokens::text_dim())
                        .debug_selector(|| "limits-close".into())
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_limits(None, cx))),
                ),
        )
        .when(!command_only, |fields| {
            fields
                .child(eyebrow("Caps"))
                .child(row(Field::Running, cx))
                .child(row(Field::PerHour, cx))
                .child(eyebrow("Budgets · empty is unlimited"))
                .child(row(Field::WorkerRequests, cx))
                .child(row(Field::WorkerTokens, cx))
                .child(row(Field::ModelRequests, cx))
                .child(row(Field::ModelTokens, cx))
        })
        .child(eyebrow("Command"))
        .child(row(Field::Command, cx))
        .child(div().text_size(size::scaled(size::SMALL)).text_color(Tokens::text_faint()).child(command_note));
    let footer = div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(space::STACK))
        .px(px(space::PAD))
        .pt(px(space::STACK))
        .pb(px(space::PAD))
        .border_t_1()
        .border_color(Tokens::edge())
        .children(limits.error().map(error))
        .children(limits.notice().map(|notice| {
            div()
                .debug_selector(|| "limits-notice".into())
                .flex()
                .gap(px(space::LINE))
                .text_size(size::scaled(size::LABEL))
                .text_color(Tokens::text_dim())
                .child(glyph(icon::CHECK, 13.).text_color(Tokens::lime()))
                .child(notice.to_string())
        }))
        .child(
            div().flex().gap(px(space::LINE)).child(
                button("limits-save", if limits.busy() { "Saving…" } else { "Save" }, Tokens::accent())
                    .debug_selector(|| "limits-save".into())
                    .on_click(cx.listener(|this, _, _, cx| this.save_limits(cx))),
            ),
        );
    div()
        .debug_selector(|| "limits-panel".into())
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(fields)
        .child(footer)
        .into_any_element()
}

/// One field: its label, then its value (the composer's element while it
/// has the caret).
fn field_row(deck: &Deck, field: Field, cx: &mut Context<Deck>) -> AnyElement {
    let limits = deck.limits();
    let focused = limits.focused() == Some(field);
    let text = limits.value(field).to_string();
    let body: AnyElement = if focused {
        composer::Field::new(cx.entity(), Tokens::accent(), field.empty()).into_any_element()
    } else if text.is_empty() {
        div().text_color(Tokens::text_faint()).child(field.empty()).into_any_element()
    } else {
        div().child(text).into_any_element()
    };
    let selector = format!("limits-field-{}", field.name());
    div()
        .flex()
        .items_center()
        .gap(px(space::STACK))
        .child(
            div()
                .flex_none()
                .w(px(230.))
                .text_size(size::scaled(size::LABEL))
                .text_color(Tokens::text_dim())
                .child(field.label()),
        )
        .child(
            div()
                .id(gpui::SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .flex_1()
                .min_w_0()
                .max_w(px(360.))
                .px(px(space::INSET))
                .py(px(4.))
                .rounded(px(radius::CONTROL))
                .border_1()
                .border_color(if focused { tint(Tokens::accent(), 0.55) } else { Tokens::edge().into() })
                .bg(Tokens::sunken())
                .font_family(MONO)
                .text_size(size::scaled(size::LABEL))
                .cursor_text()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.limits.focus(field);
                    cx.notify();
                }))
                .child(body),
        )
        .into_any_element()
}
