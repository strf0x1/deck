//! The rows under the coordinator's conversation (ADR 0011 §9): each open
//! decision of its project, linking to the worker where the user answers
//! it, and the cap row when a `create` met a limit.
//!
//! The design choice: a row waits on a human, so it is the amber-edged
//! surface on a dark body; a network destination wears the network's orange.
//! Opening one selects the worker, whose own card takes the answer as for any
//! task: nothing here answers anything.

use super::glyph::glyph;
use super::root::Deck;
use super::surfaced::{self, Decision};
use super::theme::{MONO, Tokens, icon, radius, size, space, tint};
use deck_runtime::control::CapNotice;
use gpui::{
    AnyElement, Context, Div, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};
use std::collections::HashMap;

impl Deck {
    /// The selected project's open decisions, as its coordinator's
    /// conversation surfaces them. None without a coordinator.
    pub fn surfaced(&self) -> Vec<Decision> {
        let Some(coordinator) = self.coordinator() else { return Vec::new() };
        let mut gateways: HashMap<String, String> = HashMap::new();
        for (task, approvals) in &self.approvals {
            if let Some(first) = approvals.first() {
                gateways.insert(task.clone(), first.gateway.clone());
            }
        }
        for (task, attached) in &self.attached {
            if let Some(pending) = attached.boundary.pending() {
                gateways.insert(task.clone(), pending.gateway.clone());
            }
        }
        surfaced::decisions(&self.needs_you, &self.tasks, coordinator, &gateways)
    }

    /// The cap the selected project's coordinator met, while it stands.
    pub fn limit(&self) -> Option<&CapNotice> {
        let coordinator = self.coordinator()?;
        self.limit.as_ref().filter(|cap| cap.coordinator == coordinator.id)
    }

    /// A coordinator's `create` was refused at a cap (deck#152 R4).
    pub(super) fn on_cap(&mut self, cap: CapNotice, cx: &mut Context<Self>) {
        self.limit = Some(cap);
        cx.notify();
    }

    /// Drop the cap row once it no longer stands; `raised_to` is a value the
    /// user just saved for that cap.
    pub(super) fn settle_limit(&mut self, raised_to: Option<u32>) {
        if let Some(cap) = &self.limit
            && !surfaced::limit_stands(cap, &self.tasks, raised_to)
        {
            self.limit = None;
        }
    }
}

/// The rows, or nothing when no decision is open and no cap was met.
pub fn rows(deck: &Deck, cx: &mut Context<Deck>) -> Option<AnyElement> {
    let decisions = deck.surfaced();
    let limit = deck.limit().cloned();
    if decisions.is_empty() && limit.is_none() {
        return None;
    }
    Some(
        div()
            .debug_selector(|| "surfaced".into())
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(space::LINE))
            .px(px(space::PAD))
            .pt(px(space::LINE))
            .children(decisions.iter().enumerate().map(|(index, decision)| decision_row(index, decision, cx)))
            .children(limit.map(|cap| limit_row(&cap, cx)))
            .into_any_element(),
    )
}

/// The amber-edged row a waiting decision is.
fn frame() -> Div {
    div()
        .min_w_0()
        .min_h(px(36.))
        .px(px(space::INSET))
        .py(px(space::LINE))
        .flex()
        .items_center()
        .gap(px(space::LINE))
        .rounded(px(radius::CONTROL))
        .border_1()
        .border_color(tint(Tokens::amber(), 0.45))
        .bg(Tokens::well())
        .text_size(size::scaled(size::LABEL))
}

fn decision_row(index: usize, decision: &Decision, cx: &mut Context<Deck>) -> AnyElement {
    let task = decision.task.clone();
    let subject = div()
        .min_w_0()
        .truncate()
        .font_family(MONO)
        .text_size(size::scaled(size::SMALL))
        .text_color(if decision.network { Tokens::orange() } else { Tokens::text().into() })
        .child(decision.subject.clone());
    frame()
        .id(("surfaced", index))
        .debug_selector(move || format!("surfaced-{index}"))
        .cursor_pointer()
        .hover(|s| s.bg(Tokens::raised_hover()).border_color(tint(Tokens::amber(), 0.7)))
        .on_click(cx.listener(move |this, _, _, cx| this.select_task_id(&task, cx)))
        .child(div().flex_none().size(px(7.)).rounded_full().bg(Tokens::amber()))
        .child(div().flex_none().font_weight(gpui::FontWeight::MEDIUM).child(decision.title.clone()))
        .child(div().flex_none().text_color(Tokens::text_faint()).child("·"))
        .child(div().flex_none().text_color(Tokens::text_dim()).child(decision.asks.clone()))
        .child(subject)
        .child(div().flex_1())
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(space::DETAIL))
                .text_size(size::scaled(size::SMALL))
                .text_color(Tokens::amber())
                .child("Open")
                .child(glyph(icon::CHEVRON_RIGHT, 11.)),
        )
        .into_any_element()
}

fn limit_row(cap: &CapNotice, cx: &mut Context<Deck>) -> AnyElement {
    let field = match cap.cap.as_str() {
        "creates_per_hour" => super::limits::Field::PerHour,
        _ => super::limits::Field::Running,
    };
    frame()
        .debug_selector(|| "surfaced-limit".into())
        .child(glyph(icon::GAUGE, 13.).text_color(Tokens::amber()))
        .child(div().flex_1().min_w_0().text_color(Tokens::text_dim()).child(surfaced::limit_text(cap)))
        .child(
            div()
                .id("surfaced-raise")
                .debug_selector(|| "surfaced-raise".into())
                .flex_none()
                .h(px(24.))
                .px(px(space::LINE))
                .flex()
                .items_center()
                .rounded(px(radius::CHIP))
                .border_1()
                .border_color(tint(Tokens::amber(), 0.5))
                .text_size(size::scaled(size::SMALL))
                .text_color(Tokens::amber())
                .cursor_pointer()
                .hover(|s| s.bg(Tokens::raised_hover()))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_limits(Some(field), cx)))
                .child("Raise"),
        )
        .into_any_element()
}
