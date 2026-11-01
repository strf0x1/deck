//! Render helpers for the Deck window's regions: header, lane board, and the
//! session panel (transcript, permission question, prompt line).
//!
//! The design choice: free functions over `&Deck`, not separate entities.
//! Nothing here owns state; every interaction calls back into `Deck`.

use super::board::{CARD_H, Card, LaneState};
use super::root::Deck;
use super::session::{Entry, Status};
use super::shape::{Cuts, Skin, backdrop};
use super::theme::Tokens;
use crate::acp::ToolStatus;
use gpui::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, div, px,
};

fn state_color(s: LaneState) -> Hsla {
    match s {
        LaneState::Idle => Tokens::text_faint().into(),
        LaneState::Working => Tokens::cyan(),
        LaneState::Waiting => Tokens::amber(),
        LaneState::Review => Tokens::lime(),
    }
}

fn state_label(s: LaneState) -> &'static str {
    match s {
        LaneState::Idle => "idle",
        LaneState::Working => "working",
        LaneState::Waiting => "needs you",
        LaneState::Review => "review",
    }
}

fn label(text: &'static str) -> impl IntoElement {
    div().text_size(px(10.)).text_color(Tokens::text_dim()).font_weight(FontWeight::MEDIUM).child(text)
}

pub fn header(deck: &Deck) -> impl IntoElement {
    div()
        .flex()
        .items_end()
        .gap(px(10.))
        .px(px(4.))
        .child(div().text_size(px(18.)).font_weight(FontWeight::BOLD).child("DECK"))
        .child(div().pb(px(3.)).text_size(px(10.)).text_color(Tokens::text_faint()).child("sprawlcommons"))
        .child(div().flex_1().min_w_0().pb(px(3.)).flex().justify_end().child(
            div().truncate().text_size(px(10.)).text_color(Tokens::text_faint()).child(agent_label(&deck.agent_cmd)),
        ))
}

/// The agent command without leading `env` assignments or directories.
fn agent_label(cmd: &str) -> String {
    let words: Vec<&str> = cmd.split_whitespace().skip_while(|w| *w == "env" || w.contains('=')).collect();
    let Some((prog, args)) = words.split_first() else { return String::new() };
    let prog = prog.rsplit('/').next().unwrap_or(prog);
    std::iter::once(prog).chain(args.iter().copied()).collect::<Vec<_>>().join(" ")
}

pub fn lanes(deck: &Deck, cx: &mut Context<Deck>) -> impl IntoElement {
    let held = deck.board.dragging();
    let (mut rest, mut top): (Vec<&Card>, Vec<&Card>) = (vec![], vec![]);
    for c in deck.board.cards() {
        if Some(c.lane.id) == held { top.push(c) } else { rest.push(c) }
    }
    // The held card renders last so it passes over the others.
    rest.append(&mut top);
    let cards: Vec<AnyElement> = rest.into_iter().map(|c| card(c, held == Some(c.lane.id), cx)).collect();
    div()
        .relative()
        .w(px(300.))
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(12.))
        .p(px(14.))
        .pt(px(18.))
        .child(backdrop(
            Cuts { top_left: 22., top_right: 0., bottom_right: 0., bottom_left: 8. },
            Skin { fill: Tokens::panel().into(), edge: Tokens::edge().into(), tick: None },
        ))
        .child(div().pl(px(14.)).child(label("LANES")))
        .child(div().relative().h(px(deck.board.height())).children(cards))
}

fn card(c: &Card, held: bool, cx: &mut Context<Deck>) -> AnyElement {
    let lift = c.lift.pos.clamp(0., 1.2);
    let color = state_color(c.lane.state);
    let edge = Hsla { a: 0.18 + 0.5 * lift.min(1.), ..Tokens::magenta() };
    let edge = if lift > 0.02 { edge } else { Tokens::edge().into() };
    let id = c.lane.id;
    div()
        .id(("card", id))
        .absolute()
        .top(px(c.y.pos))
        .left(px(lift * 10.))
        .w_full()
        .h(px(CARD_H))
        .when_cursor(held)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, ev: &MouseDownEvent, _, cx| this.grab(id, f32::from(ev.position.y), cx)),
        )
        .child(backdrop(Cuts::deck(14., 5.), Skin { fill: Tokens::panel_raised().into(), edge, tick: Some(color) }))
        .child(
            div()
                .relative()
                .size_full()
                .flex()
                .flex_col()
                .justify_center()
                .gap(px(4.))
                .pl(px(18.))
                .pr(px(12.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(div().flex_1().truncate().font_weight(FontWeight::MEDIUM).child(c.lane.title.clone()))
                        .child(div().text_size(px(10.)).text_color(color).child(state_label(c.lane.state))),
                )
                .child(div().truncate().text_size(px(11.)).text_color(Tokens::text_dim()).child(c.lane.detail.clone())),
        )
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

pub fn main(deck: &Deck, cx: &mut Context<Deck>) -> impl IntoElement {
    let s = &deck.session;
    let (status, color) = match &s.status {
        Status::Connecting => ("connecting".to_string(), Tokens::text_dim().into()),
        Status::Ready => ("ready".to_string(), Tokens::cyan()),
        Status::Working => ("working".to_string(), Tokens::cyan()),
        Status::Down(_) => ("down".to_string(), Tokens::red()),
    };
    let status = if s.ask.is_some() { ("needs you".to_string(), Tokens::amber()) } else { (status, color) };
    div()
        .relative()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(10.))
        .p(px(16.))
        .child(backdrop(
            Cuts { top_left: 0., top_right: 26., bottom_right: 0., bottom_left: 12. },
            Skin { fill: Tokens::panel().into(), edge: Tokens::edge().into(), tick: None },
        ))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(label("SESSION"))
                .child(div().text_size(px(10.)).text_color(Tokens::text_faint()).child(s.agent.clone()))
                .child(div().flex_1())
                .child(div().size(px(6.)).bg(status.1))
                .child(div().text_size(px(10.)).text_color(status.1).child(status.0)),
        )
        .child(transcript(deck))
        .children(s.ask.as_ref().map(|_| ask(deck, cx)))
        .child(prompt(deck))
}

fn transcript(deck: &Deck) -> impl IntoElement {
    let rows = deck.session.entries.iter().map(|e| match e {
        Entry::You(t) => div()
            .flex()
            .gap(px(8.))
            .child(div().text_color(Tokens::magenta()).child("›"))
            .child(div().flex_1().child(t.clone())),
        Entry::Agent(t) => div().pl(px(16.)).child(t.clone()),
        Entry::Thought(t) => div().pl(px(16.)).italic().text_color(Tokens::text_faint()).child(t.clone()),
        Entry::Tool { title, status, .. } => {
            let (glyph, color) = match status {
                ToolStatus::Running => ("◌", Tokens::cyan()),
                ToolStatus::Done => ("✓", Tokens::lime()),
                ToolStatus::Failed => ("✕", Tokens::red()),
            };
            div()
                .pl(px(16.))
                .flex()
                .gap(px(8.))
                .text_size(px(12.))
                .child(div().text_color(color).child(glyph))
                .child(div().text_color(Tokens::text_dim()).child(title.clone()))
        }
        Entry::Note(t) => div().pl(px(16.)).text_color(Tokens::red()).child(t.clone()),
    });
    div()
        .id("transcript")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .track_scroll(&deck.scroll)
        .flex()
        .flex_col()
        .gap(px(8.))
        .children(rows)
}

fn ask(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let Some(ask) = &deck.session.ask else { return div().into_any_element() };
    let buttons = ask.options.iter().enumerate().map(|(i, o)| {
        let color = if o.allows() { Tokens::cyan() } else { Tokens::red() };
        div()
            .id(("opt", i))
            .relative()
            .h(px(28.))
            .px(px(14.))
            .flex()
            .items_center()
            .gap(px(6.))
            .cursor_pointer()
            .child(backdrop(
                Cuts::deck(8., 3.),
                Skin { fill: Tokens::panel().into(), edge: Hsla { a: 0.55, ..color }, tick: None },
            ))
            .child(div().relative().text_size(px(10.)).text_color(Tokens::text_faint()).child(format!("{}", i + 1)))
            .child(div().relative().text_size(px(12.)).text_color(color).child(o.name.clone()))
            .on_click(cx.listener(move |this, _, _, cx| this.choose(Some(i), cx)))
    });
    div()
        .relative()
        .flex()
        .flex_col()
        .gap(px(10.))
        .p(px(14.))
        .pl(px(18.))
        .child(backdrop(
            Cuts::deck(14., 5.),
            Skin {
                fill: Tokens::panel_raised().into(),
                edge: Hsla { a: 0.5, ..Tokens::amber() },
                tick: Some(Tokens::amber()),
            },
        ))
        .child(
            div()
                .relative()
                .flex()
                .gap(px(8.))
                .child(div().text_color(Tokens::amber()).child(format!("{} wants to run", deck.session.agent)))
                .child(div().text_color(Tokens::text()).child(ask.title.clone())),
        )
        .child(div().relative().flex().gap(px(8.)).children(buttons))
        .into_any_element()
}

fn prompt(deck: &Deck) -> impl IntoElement {
    let ready = deck.session.can_prompt();
    let body = if deck.draft.is_empty() {
        let hint = if ready { "tell the agent what to do — enter sends, esc cancels a turn" } else { "…" };
        div().text_color(Tokens::text_faint()).child(hint)
    } else {
        div().child(deck.draft.clone())
    };
    let accent = if ready { Tokens::cyan() } else { Tokens::text_faint().into() };
    div()
        .relative()
        .h(px(40.))
        .px(px(16.))
        .flex()
        .items_center()
        .gap(px(10.))
        .child(backdrop(
            Cuts { top_left: 10., top_right: 0., bottom_right: 10., bottom_left: 0. },
            Skin { fill: Tokens::panel_raised().into(), edge: Hsla { a: 0.35, ..accent }, tick: None },
        ))
        .child(div().relative().text_color(accent).child("›"))
        .child(div().relative().flex_1().truncate().child(body))
        .child(div().relative().w(px(7.)).h(px(15.)).bg(Hsla { a: if ready { 0.8 } else { 0.2 }, ..accent }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn agent_label_drops_env_and_paths() {
        assert_eq!(super::agent_label("env A=1 B=2 /x/y/construct acp"), "construct acp");
        assert_eq!(super::agent_label("construct acp"), "construct acp");
        assert_eq!(super::agent_label(""), "");
    }
}
