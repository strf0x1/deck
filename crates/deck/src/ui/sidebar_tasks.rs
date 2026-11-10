//! The sidebar's compact task rows and the settled fold: the selected
//! project's tasks on the board's pitch, and the rows that settled
//! (ADR 0010 §6, §9).
//!
//! The design choice: the rows are a snapshot of the board's cards, so the
//! drag's springs and order stay the board's and nothing here owns state.

use super::board::{CARD_H, Card, LaneState};
use super::panels::{glyph, state_mark};
use super::root::{Deck, LIVE};
use super::sidebar_rows::TASK_INDENT;
use super::theme::{Tokens, icon, radius, size, space, tint};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, BoxShadow, Context, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, div, point, px,
};

/// The selected project's compact task rows, drawn on the board's pitch so drag
/// and reorder keep working.
pub(super) fn tasks_view(deck: &mut Deck, cx: &mut Context<Deck>) -> AnyElement {
    let held = deck.board.dragging();
    let cards: Vec<Card> = deck.board.cards().to_vec();
    let (mut rest, mut top): (Vec<Card>, Vec<Card>) = (vec![], vec![]);
    for c in cards {
        if Some(c.lane.id) == held { top.push(c) } else { rest.push(c) }
    }
    rest.append(&mut top);
    let cards: Vec<AnyElement> = rest.into_iter().map(|c| task_row(&c, held == Some(c.lane.id), deck, cx)).collect();
    div()
        .debug_selector(|| "sidebar-tasks".into())
        .relative()
        .ml(px(TASK_INDENT))
        .h(px(deck.board.height()))
        .children(cards)
        .into_any_element()
}

fn task_row(c: &Card, held: bool, deck: &mut Deck, cx: &mut Context<Deck>) -> AnyElement {
    let lift = c.lift.pos.clamp(0., 1.2);
    let live = c.lane.id == LIVE;
    let edge: gpui::Hsla = if live { tint(Tokens::accent(), 0.55) } else { Tokens::edge().into() };
    let edge = if lift > 0.02 { tint(Tokens::text().into(), 0.07 + 0.12 * lift.min(1.)) } else { edge };
    let bg: gpui::Hsla = if live {
        tint(Tokens::accent(), 0.1)
    } else if lift > 0.02 {
        Tokens::raised_hover().into()
    } else {
        Tokens::raised().into()
    };
    let id = c.lane.id;
    let task = deck.lanes.task(id, deck.task_id());
    let done = task
        .as_deref()
        .and_then(|task| deck.tasks.iter().find(|record| record.id == task))
        .is_some_and(|record| record.done_ms.is_some());
    // The check appears only while the row is hovered, so the list carries
    // no permanent chrome.
    let hovered = deck.hovered_task == Some(id);
    let mark = task.filter(|_| hovered).map(|task| {
        div()
            .id(("card-done", id))
            .debug_selector(move || format!("card-done-{id}"))
            .flex_none()
            .size(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(radius::CHIP))
            .text_color(if done { Tokens::lime() } else { Tokens::text_faint().into() })
            .cursor_pointer()
            .hover(|style| style.bg(Tokens::raised_hover()).text_color(Tokens::text_dim()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.mark_task(&task, !done, cx);
                }),
            )
            .child(glyph(icon::CHECK, 10.))
    });
    let chip = c.lane.chip.as_ref().map(|(dot, label)| {
        super::publication_panel::chip(*dot, label)
            .debug_selector(move || format!("card-chip-{id}"))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_publication(id, cx);
                }),
            )
    });
    let right: AnyElement = match (&c.lane.attention, chip) {
        (Some(word), _) => div()
            .flex_none()
            .h(px(18.))
            .px(px(space::TIGHT))
            .flex()
            .items_center()
            .rounded_full()
            .bg(tint(Tokens::amber(), 0.14))
            .text_size(px(size::MICRO))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Tokens::amber())
            .child(*word)
            .into_any_element(),
        (None, Some(chip)) => chip.into_any_element(),
        (None, None) => div().flex_none().into_any_element(),
    };
    div()
        .id(("card", id))
        .debug_selector(move || format!("card-row-{id}"))
        .absolute()
        .top(px(c.y.pos))
        .left(px(lift * 8.))
        .w_full()
        .h(px(CARD_H))
        .px(px(space::TIGHT))
        .flex()
        .items_center()
        .gap(px(space::LINE))
        .rounded(px(radius::CONTROL))
        .border_1()
        .border_color(edge)
        .bg(bg)
        .shadow(vec![BoxShadow {
            color: tint(Tokens::shadow(), 0.6 * lift.min(1.)),
            offset: point(px(0.), px(4. + 10. * lift)),
            blur_radius: px(8. + 24. * lift),
            spread_radius: px(0.),
        }])
        .when(held, |s| s.cursor_grabbing())
        .when(!held, |s| s.cursor_grab())
        .hover(|s| s.bg(Tokens::raised_hover()))
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            let hovered = if *hovered {
                Some(id)
            } else if this.hovered_task == Some(id) {
                None
            } else {
                return;
            };
            if this.hovered_task != hovered {
                this.hovered_task = hovered;
                cx.notify();
            }
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, ev: &MouseDownEvent, _, cx| this.grab(id, f32::from(ev.position.y), cx)),
        )
        // A row that asks wears the waiting mark, whatever its process is doing.
        .child(state_mark(if c.lane.attention.is_some() { LaneState::Waiting } else { c.lane.state }, 8.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(size::SMALL))
                .text_color(if live { Tokens::text() } else { Tokens::text_dim() })
                .child(c.lane.title.clone()),
        )
        .children(mark)
        .child(right)
        .into_any_element()
}

/// The settled tasks' fold: a quiet header, and the rows once opened.
pub(super) fn settled_fold(deck: &mut Deck, count: usize, cx: &mut Context<Deck>) -> AnyElement {
    let open = deck.settled_open();
    let rows: Vec<AnyElement> = if open {
        let settled: Vec<super::tasks::Settled> = deck.settled().to_vec();
        settled.iter().enumerate().map(|(index, settled)| settled_row(settled, index, deck, cx)).collect()
    } else {
        Vec::new()
    };
    div()
        .flex()
        .flex_col()
        .gap(px(space::HAIR))
        .child(
            div()
                .id("settled-fold")
                .debug_selector(|| "settled-fold".into())
                .h(px(28.))
                .px(px(space::TIGHT))
                .flex()
                .items_center()
                .rounded(px(radius::CONTROL))
                .cursor_pointer()
                .hover(|style| style.bg(Tokens::raised()))
                .gap(px(space::LINE))
                .text_size(px(size::SMALL))
                .text_color(Tokens::text_faint())
                .on_click(cx.listener(|this, _, _, cx| this.toggle_settled(cx)))
                .child(glyph(if open { icon::CHEVRON_DOWN } else { icon::CHEVRON_RIGHT }, 8.))
                .child(glyph(icon::ARCHIVE, 10.))
                .child(format!("Settled ({count})")),
        )
        .children(rows)
        .into_any_element()
}

/// One settled task: what it was, when it settled, and the mark's toggle.
fn settled_row(settled: &super::tasks::Settled, index: usize, deck: &mut Deck, cx: &mut Context<Deck>) -> AnyElement {
    let title = super::tasks::title(&settled.task);
    let destroyed = settled.task.state == deck_runtime::State::Destroyed;
    let done = settled.task.done_ms.is_some();
    let label = if destroyed {
        "destroyed"
    } else if done {
        "done"
    } else {
        "merged"
    };
    let id = settled.task.id.clone();
    let lane = deck.lanes.lane_of(&id, deck.task_id());
    let chip = lane.and_then(|lane| deck.publication_of(lane).map(|view| (lane, view)));
    let toggle = (!destroyed).then(|| {
        div()
            .id(("settled-done", index))
            .debug_selector(move || format!("settled-done-{index}"))
            .h(px(20.))
            .px(px(space::TIGHT))
            .flex()
            .items_center()
            .rounded(px(radius::CHIP))
            .border_1()
            .border_color(Tokens::edge())
            .text_size(px(size::SMALL))
            .text_color(Tokens::text_dim())
            .cursor_pointer()
            .hover(|style| style.bg(Tokens::raised_hover()))
            .on_click(cx.listener(move |this, _, _, cx| this.mark_task(&id, !done, cx)))
            .child(if done { "Not done" } else { "Mark done" })
    });
    div()
        .h(px(26.))
        .px(px(space::TIGHT))
        .flex()
        .items_center()
        .gap(px(space::TIGHT))
        .rounded(px(radius::CONTROL))
        .bg(Tokens::raised())
        .text_size(px(size::SMALL))
        .text_color(Tokens::text_dim())
        .child(div().flex_1().min_w_0().truncate().child(title))
        .child(div().flex_none().text_color(Tokens::text_faint()).child(label))
        .children(chip.map(|(lane, view)| {
            super::publication_panel::chip(view.dot, &view.label)
                .id(("settled-chip", index))
                .debug_selector(move || format!("settled-chip-{index}"))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_publication(lane, cx)))
        }))
        .children(toggle)
        .into_any_element()
}
