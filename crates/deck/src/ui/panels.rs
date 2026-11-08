//! Render helpers for the sessions column (the draggable lane cards) and the
//! small shared pieces other regions use: icon glyphs, state colours, panes.
//!
//! The design choice: free functions over `&Deck`, not separate entities.
//! Nothing here owns state; every interaction calls back into `Deck`.

use super::board::{CARD_H, Card, LaneState};
use super::chrome::icon_button;
use super::root::{Deck, LIVE};
use super::theme::{MONO, Tokens, icon, radius, size, space, tint};
use gpui::{
    AnyElement, BoxShadow, Context, Div, FontWeight, Hsla, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, ParentElement as _, StatefulInteractiveElement as _, Styled as _, div, point, px,
};

pub fn state_color(s: LaneState) -> Hsla {
    match s {
        LaneState::Idle => Tokens::text_faint().into(),
        LaneState::Working => Tokens::mint(),
        LaneState::Waiting => Tokens::amber(),
        LaneState::Review => Tokens::lime(),
    }
}

fn state_label(s: LaneState) -> &'static str {
    match s {
        LaneState::Idle => "Idle",
        LaneState::Working => "Working",
        LaneState::Waiting => "Needs you",
        LaneState::Review => "Review",
    }
}

/// A Nerd Font icon at `size` pixels.
pub fn glyph(g: &'static str, size: f32) -> Div {
    div().font_family(MONO).text_size(px(size)).line_height(px(size + 2.)).child(g)
}

/// One pane of glass: the sessions column and the conversation both sit on this.
pub fn pane() -> Div {
    div().relative().rounded(px(radius::PANE)).border_1().border_color(Tokens::edge()).bg(Tokens::panel())
}

pub fn lanes(deck: &Deck, cx: &mut Context<Deck>) -> impl IntoElement {
    let held = deck.board.dragging();
    let (mut rest, mut top): (Vec<&Card>, Vec<&Card>) = (vec![], vec![]);
    for c in deck.board.cards() {
        if Some(c.lane.id) == held { top.push(c) } else { rest.push(c) }
    }
    // The held card renders last so it passes over the others.
    rest.append(&mut top);
    let cards: Vec<AnyElement> = rest.into_iter().map(|c| card(c, held == Some(c.lane.id), deck, cx)).collect();
    pane()
        .w(px(296.))
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(space::ROW))
        .p(px(space::INSET))
        .child(
            div()
                .pl(px(space::TIGHT))
                .flex()
                .items_center()
                .gap(px(space::LINE))
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Sessions"))
                .child(
                    div()
                        .px(px(7.))
                        .rounded_full()
                        .bg(Tokens::raised())
                        .text_size(px(size::SMALL))
                        .text_color(Tokens::text_dim())
                        .child(deck.board.cards().len().to_string()),
                )
                .child(div().flex_1())
                .child(
                    icon_button("lanes-new", icon::PLUS)
                        .debug_selector(|| "lanes-new".into())
                        .on_click(cx.listener(|this, _, _, cx| this.new_session(cx))),
                ),
        )
        .child(div().relative().h(px(deck.board.height())).children(cards))
        .children((!deck.settled().is_empty()).then(|| fold(deck, cx)))
}

/// The settled tasks' fold: a quiet header, and the rows once opened.
fn fold(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let open = deck.settled_open();
    let rows: Vec<AnyElement> = if open {
        deck.settled().iter().enumerate().map(|(index, settled)| settled_row(settled, index, deck, cx)).collect()
    } else {
        Vec::new()
    };
    div()
        .flex()
        .flex_none()
        .flex_col()
        .gap(px(space::TIGHT))
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
                .text_size(px(size::SMALL))
                .text_color(Tokens::text_faint())
                .on_click(cx.listener(|this, _, _, cx| this.toggle_settled(cx)))
                .child(format!("Settled ({})", deck.settled().len())),
        )
        .children(rows)
        .into_any_element()
}

/// One settled task: what it was, when it settled, and the mark's toggle.
fn settled_row(settled: &super::tasks::Settled, index: usize, deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let title = settled
        .task
        .spec
        .cwd
        .file_name()
        .map_or_else(|| settled.task.spec.cwd.display().to_string(), |name| name.to_string_lossy().into_owned());
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
    // The chip is how a settled task still reaches its publication panel and
    // the destroy-after-merge flow (ADR 0008 §8).
    let lane = settled.task.id.as_str();
    let lane = deck.lanes.lane_of(lane, deck.task_id());
    let chip = lane.and_then(|lane| deck.publication_of(lane).map(|view| (lane, view)));
    let toggle = (!destroyed).then(|| {
        div()
            .id(("settled-done", index))
            .debug_selector(move || format!("settled-done-{index}"))
            .h(px(22.))
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
        .h(px(30.))
        .px(px(space::INSET))
        .flex()
        .items_center()
        .gap(px(space::SECTION))
        .rounded(px(radius::CONTROL))
        .bg(Tokens::raised())
        .text_size(px(size::SMALL))
        .text_color(Tokens::text_dim())
        .child(div().truncate().child(title))
        .child(div().flex_none().text_color(Tokens::text_faint()).child(label))
        .child(div().flex_1())
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

fn card(c: &Card, held: bool, deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let lift = c.lift.pos.clamp(0., 1.2);
    let live = c.lane.id == LIVE;
    let color = state_color(c.lane.state);
    let edge: Hsla = if live { tint(Tokens::accent(), 0.4) } else { Tokens::edge().into() };
    let edge = if lift > 0.02 { tint(Tokens::text().into(), 0.07 + 0.12 * lift.min(1.)) } else { edge };
    let bg = if lift > 0.02 || live { Tokens::raised_hover() } else { Tokens::raised() };
    let tile_bg: Hsla = if live { tint(Tokens::accent(), 0.18) } else { Tokens::raised().into() };
    let tile_fg: Hsla = if live { Tokens::accent() } else { Tokens::text_dim().into() };
    let id = c.lane.id;
    let note = c.lane.attention.clone().unwrap_or_else(|| c.lane.detail.clone());
    let note_color: Hsla = if c.lane.attention.is_some() { Tokens::amber() } else { Tokens::text_faint().into() };
    let task = deck.lanes.task(id, deck.task_id());
    let done = task
        .as_deref()
        .and_then(|task| deck.tasks.iter().find(|record| record.id == task))
        .is_some_and(|record| record.done_ms.is_some());
    let mark = task.map(|task| {
        div()
            .id(("card-done", id))
            .debug_selector(move || format!("card-done-{id}"))
            .flex_none()
            .h(px(22.))
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.mark_task(&task, !done, cx);
                }),
            )
            .child(if done { "Not done" } else { "Mark done" })
    });
    div()
        .id(("card", id))
        .absolute()
        .top(px(c.y.pos))
        .left(px(lift * 8.))
        .w_full()
        .h(px(CARD_H))
        .px(px(space::INSET))
        .flex()
        .items_center()
        .gap(px(space::SECTION))
        .rounded(px(radius::BUBBLE))
        .border_1()
        .border_color(edge)
        .bg(bg)
        .shadow(vec![BoxShadow {
            color: tint(Tokens::shadow(), 0.6 * lift.min(1.)),
            offset: point(px(0.), px(4. + 10. * lift)),
            blur_radius: px(8. + 24. * lift),
            spread_radius: px(0.),
        }])
        .when_cursor(held)
        .hover(|s| s.bg(Tokens::raised_hover()))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, ev: &MouseDownEvent, _, cx| this.grab(id, f32::from(ev.position.y), cx)),
        )
        .child(
            div()
                .flex_none()
                .size(px(36.))
                .rounded(px(radius::CONTROL))
                .bg(tile_bg)
                .text_color(tile_fg)
                .flex()
                .items_center()
                .justify_center()
                .child(glyph(icon::TERMINAL, 13.)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(div().truncate().font_weight(FontWeight::MEDIUM).child(c.lane.title.clone()))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(space::TIGHT))
                        .text_size(px(size::SMALL))
                        .text_color(Tokens::text_dim())
                        .child(div().flex_none().size(px(6.)).rounded_full().bg(color))
                        .child(div().flex_none().child(state_label(c.lane.state)))
                        .child(div().flex_none().text_color(Tokens::text_faint()).child("·"))
                        .child(div().truncate().text_color(note_color).child(note)),
                ),
        )
        .children(mark)
        .children(c.lane.chip.as_ref().map(|(dot, label)| {
            // Its own press: the card under it must not take it as a grab.
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
        }))
        .into_any_element()
}

trait CursorExt {
    fn when_cursor(self, held: bool) -> Self;
}

impl CursorExt for gpui::Stateful<gpui::Div> {
    fn when_cursor(self, held: bool) -> Self {
        if held { self.cursor_grabbing() } else { self.cursor_grab() }
    }
}
