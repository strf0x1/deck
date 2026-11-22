//! The project view's header controls (ADR 0011 §9): the coordinator's
//! status chip where a task's agent pill sits, Restart for a stopped one, and
//! Limits. Stop is the header's own, as for any task.
//!
//! The design choice: the chip reads the coordinator's record and, when the
//! window holds it, its live session, through `coordinator::status`, so the
//! header and the control socket's `state` say the same words.

use super::glyph::glyph;
use super::panels::state_color;
use super::root::Deck;
use super::theme::{Tokens, icon, radius, size, space, tint};
use gpui::{
    AnyElement, Context, Div, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px,
};

impl Deck {
    /// The coordinator's status as the header says it, and its state.
    pub fn coordinator_status(&self) -> Option<(super::board::LaneState, String)> {
        let record = self.coordinator()?;
        let attached = self.attached.get(&record.id);
        let asks = attached.is_some_and(|a| a.session.ask.is_some() || a.boundary.pending().is_some());
        Some(super::coordinator::status(record, attached.map(|a| &a.session.status), asks))
    }
}

/// The coordinator's status: a dot and its words, in its state's colour.
pub fn status_chip(deck: &Deck) -> Option<Div> {
    let (state, text) = deck.coordinator_status()?;
    let color = state_color(state);
    Some(
        div()
            .debug_selector(|| "hdr-coordinator".into())
            .min_h(px(30.))
            .px(px(space::INSET))
            .flex()
            .items_center()
            .gap(px(space::LINE))
            .rounded_full()
            .border_1()
            .border_color(tint(color, 0.35))
            .bg(Tokens::well())
            .text_size(size::scaled(size::SMALL))
            .text_color(Tokens::text_dim())
            .child(div().size(px(7.)).rounded_full().bg(color))
            .child(text),
    )
}

/// Restart and Destroy for a stopped or failed coordinator (Destroy asks
/// for a second click), and Limits for any.
pub fn actions(deck: &Deck, cx: &mut Context<Deck>) -> Vec<AnyElement> {
    let mut out = Vec::new();
    if deck.coordinator_selected() && deck.can_restart() {
        out.push(
            button("hdr-restart", icon::REFRESH, "Restart", Tokens::cyan())
                .on_click(cx.listener(|this, _, _, cx| this.restart(cx)))
                .into_any_element(),
        );
    }
    let stopped = deck.coordinator().is_some_and(|task| task.state != deck_runtime::State::Running);
    if stopped {
        let label = if deck.coordinator_destroy_armed() { "Destroy coordinator?" } else { "Destroy" };
        out.push(
            button("hdr-destroy", icon::TRASH, label, Tokens::red())
                .on_click(cx.listener(|this, _, _, cx| this.destroy_coordinator(cx)))
                .into_any_element(),
        );
    }
    if deck.coordinator().is_some() {
        let open = deck.limits_open();
        out.push(
            button("hdr-limits", icon::GAUGE, "Limits", Tokens::amber())
                .when(open, |d| d.bg(Tokens::raised_hover()).text_color(Tokens::text()))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_limits(None, cx)))
                .into_any_element(),
        );
    } else if deck.project().is_some() {
        out.push(
            button("hdr-command", icon::TERMINAL, "Coordinator command", Tokens::amber())
                .when(deck.limits_open(), |d| d.bg(Tokens::raised_hover()).text_color(Tokens::text()))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_limits(None, cx)))
                .into_any_element(),
        );
    }
    out
}

/// A header button: its glyph in the colour of what it touches.
fn button(id: &'static str, g: &'static str, label: &'static str, hue: gpui::Hsla) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .debug_selector(move || id.into())
        .min_h(px(32.))
        .px(px(space::INSET))
        .flex()
        .items_center()
        .gap(px(space::LINE))
        .rounded(px(radius::CONTROL))
        .border_1()
        .border_color(Tokens::edge())
        .text_size(size::scaled(size::BODY))
        .text_color(Tokens::text_dim())
        .cursor_pointer()
        .hover(move |s| s.border_color(tint(hue, 0.5)).bg(Tokens::well()).text_color(Tokens::text()))
        .child(glyph(g, 12.).text_color(hue))
        .child(label)
}
