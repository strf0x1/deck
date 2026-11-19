//! The project view: the coordinator's conversation in the centre and the
//! project's unsettled tasks as cards beside it, stacked below it at narrow
//! widths (ADR 0010 §1, ADR 0011 §9).
//!
//! The design choice: the cards are the board's own, so the project view and
//! the sidebar's task rows never disagree about order, titles or detail; a
//! click selects the card's lane exactly as a sidebar row does. The centre
//! is the ordinary transcript list of the selected coordinator, so nothing
//! here draws a conversation of its own.

use super::board::Card;
use super::glyph::glyph;
use super::panels::{eyebrow, icon_mark, state_color, state_mark};
use super::root::Deck;
use super::theme::{MONO, Tokens, icon, radius, size, space, tint};
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

/// The cards' column beside the conversation.
const SIDE: f32 = 236.;
/// Below this main-area width the cards stack under the conversation.
pub const WIDE: f32 = 780.;
/// The stacked cards' strip height at narrow widths.
const STRIP: f32 = 168.;

/// The selected project's view: what the main area shows when no task is
/// selected or its coordinator is (ADR 0010 §1, §5). `width` is the main
/// area's, which decides where the cards go.
pub fn project_view(deck: &Deck, width: f32, cx: &mut Context<Deck>) -> AnyElement {
    let Some(project) = deck.projects.selected() else { return div().into_any_element() };
    let centre = if deck.coordinator_selected() {
        super::transcript_list::transcript(deck, cx).into_any_element()
    } else {
        empty(&project.name)
    };
    let centre = div()
        .debug_selector(|| "project-conversation".into())
        .flex_1()
        .min_w_0()
        .min_h_0()
        .flex()
        .flex_col()
        .children(deck.coordinator_refusal().map(refusal))
        .children(super::worktree_view::source(deck, cx))
        .child(centre)
        .children(super::surfaced_view::rows(deck, cx));
    let wide = width >= WIDE;
    let side = sessions(deck, &project.argv.join(" "), wide, cx);
    let view = div().debug_selector(|| "project-view".into()).flex_1().min_h_0().flex().overflow_hidden();
    if wide { view.child(centre).child(side) } else { view.flex_col().child(centre).child(side) }.into_any_element()
}

/// The centre before there is a coordinator: what one is for, and that the
/// composer below starts it.
fn empty(name: &str) -> AnyElement {
    div()
        .debug_selector(|| "coordinator-empty".into())
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(space::STACK))
        .px(px(space::PANE))
        .child(icon_mark(icon::AGENTS, Tokens::cyan(), 30.))
        .child(
            div()
                .text_size(size::scaled(size::TITLE))
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!("Brief a coordinator for {name}")),
        )
        .child(div().max_w(px(520.)).text_size(size::scaled(size::LABEL)).text_color(Tokens::text_dim()).child(
            "Tell it what you want done. It starts sessions in this project, follows them, and brings you \
             what they need you to decide; you answer those yourself.",
        ))
        .into_any_element()
}

/// The agent's refusal of the coordinator's session, such as a model below
/// `optchat`'s window: said here rather than left as a dead task.
fn refusal(why: &str) -> AnyElement {
    div()
        .debug_selector(|| "coordinator-refusal".into())
        .mx(px(space::PAD))
        .mt(px(space::PAD))
        .px(px(space::INSET))
        .py(px(space::LINE))
        .flex()
        .gap(px(space::LINE))
        .rounded(px(radius::CONTROL))
        .border_1()
        .border_color(tint(Tokens::red(), 0.35))
        .bg(Tokens::well())
        .text_size(size::scaled(size::LABEL))
        .text_color(Tokens::red())
        .child(glyph(icon::WARN, 13.).mt(px(space::HAIR)))
        .child(div().flex_1().min_w_0().child(format!("The coordinator could not start: {why}")))
        .into_any_element()
}

/// The project's unsettled tasks as cards, then the agent command and the
/// project's rules: a column beside the conversation, or a strip below it.
fn sessions(deck: &Deck, agent: &str, wide: bool, cx: &mut Context<Deck>) -> AnyElement {
    let cards: Vec<Card> = deck.board().cards().to_vec();
    let count = cards.len();
    let list = if cards.is_empty() {
        div()
            .debug_selector(|| "project-empty-state".into())
            .p(px(space::INSET))
            .rounded(px(radius::CARD))
            .border_1()
            .border_color(Tokens::edge())
            .bg(Tokens::well())
            .text_size(size::scaled(size::SMALL))
            .text_color(Tokens::text_faint())
            .child("No sessions yet. The coordinator starts them, or New session does.")
    } else {
        div().flex().flex_col().gap(px(space::LINE)).children(cards.iter().map(|card| card_row(deck, card, cx)))
    };
    let column = div()
        .debug_selector(|| "project-view-side".into())
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(space::STACK))
        .p(px(space::PAD))
        .overflow_hidden();
    let column = if wide {
        column.w(px(SIDE)).border_l_1().border_color(Tokens::edge())
    } else {
        column.h(px(STRIP)).border_t_1().border_color(Tokens::edge())
    };
    column
        .child(eyebrow(&format!("Sessions · {count}")).debug_selector(|| "project-view-sessions".into()))
        .child(div().flex_1().min_h_0().overflow_hidden().child(list))
        .child(footer(agent, cx))
        .into_any_element()
}

/// The workers' agent command and the project's rules and grants.
fn footer(agent: &str, cx: &mut Context<Deck>) -> AnyElement {
    div()
        .min_w_0()
        .flex()
        .items_center()
        .gap(px(space::LINE))
        .child(glyph(icon::AGENTS, 12.).text_color(Tokens::text_faint()))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(MONO)
                .text_size(size::scaled(size::MICRO))
                .text_color(Tokens::text_faint())
                .child(agent.to_string()),
        )
        .child(
            div()
                .id("project-view-rules")
                .debug_selector(|| "project-view-rules".into())
                .flex_none()
                .size(px(26.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(radius::CONTROL))
                .border_1()
                .border_color(tint(Tokens::violet(), 0.3))
                .bg(Tokens::well())
                .text_color(Tokens::violet())
                .cursor_pointer()
                .hover(|style| style.bg(Tokens::raised_hover()))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_access(cx)))
                .child(glyph(icon::SHIELD, 12.)),
        )
        .into_any_element()
}

/// One unsettled task: its state dot and title, its detail line (or the
/// reason it needs you, in amber), who started it, and its publication chip.
fn card_row(deck: &Deck, card: &Card, cx: &mut Context<Deck>) -> AnyElement {
    let lane = &card.lane;
    let id = lane.id;
    // A card that waits on you says so with its whole edge.
    let color = if lane.attention.is_some() { Tokens::amber() } else { state_color(lane.state) };
    let line = match lane.attention {
        Some(word) => div().text_color(Tokens::amber()).child(word),
        None => div().text_color(Tokens::text_faint()).child(lane.detail.clone()),
    };
    let chip = lane.chip.as_ref().map(|(dot, label)| super::publication_panel::chip(*dot, label));
    let worker = deck.lanes.task(id, deck.task_id());
    let local = worker
        .as_ref()
        .and_then(|task| super::worktree_view::task_button(deck, task, format!("project-worktree-{id}"), cx));
    let branch = worker.as_ref().filter(|_| local.is_some()).map(|task| {
        div()
            .truncate()
            .font_family(MONO)
            .text_size(size::scaled(size::MICRO))
            .text_color(Tokens::text_faint())
            .child(deck_runtime::github::task_branch(task))
    });
    let provenance = deck.lane_provenance(id).map(|text| {
        div()
            .debug_selector(move || format!("project-view-provenance-{id}"))
            .truncate()
            .text_size(size::scaled(size::MICRO))
            .text_color(Tokens::text_faint())
            .child(text)
    });
    div()
        .id(("project-view-card", id))
        .debug_selector(move || format!("project-view-card-{id}"))
        .min_w_0()
        .px(px(space::INSET))
        .py(px(space::LINE))
        .flex()
        .items_start()
        .gap(px(space::LINE))
        .rounded(px(radius::CARD))
        .border_1()
        .border_color(if lane.attention.is_some() { tint(color, 0.4) } else { Tokens::edge().into() })
        .bg(Tokens::raised())
        .cursor_pointer()
        .hover(move |style| style.bg(Tokens::raised_hover()).border_color(tint(color, 0.5)))
        .on_click(cx.listener(move |this, _, _, cx| this.select(id, cx)))
        .child(state_mark(if lane.attention.is_some() { super::board::LaneState::Waiting } else { lane.state }, 12.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(space::HAIR))
                .child(
                    div()
                        .truncate()
                        .text_size(size::scaled(size::LABEL))
                        .font_weight(FontWeight::MEDIUM)
                        .child(lane.title.clone()),
                )
                .child(div().truncate().text_size(size::scaled(size::SMALL)).child(line))
                .children(provenance)
                .children(branch)
                .children(local.map(|chip| div().flex().child(chip)))
                .children(chip.map(|chip| div().flex().child(chip))),
        )
        .into_any_element()
}
