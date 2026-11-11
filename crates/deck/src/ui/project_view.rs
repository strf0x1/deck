//! The project view: a project's header, its unsettled tasks as cards, and a
//! footer naming the agent and opening its rules (ADR 0010 §1).
//!
//! The design choice: the cards are the board's own, so the project view and
//! the sidebar's task rows never disagree about order, titles or detail; a
//! click selects the card's lane exactly as a sidebar row does.

use super::board::Card;
use super::panels::{eyebrow, state_color, state_mark};
use super::root::Deck;
use super::theme::{MONO, Tokens, icon, radius, size, space, tint};
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

/// The selected project with no selected task: what the main area shows
/// (ADR 0010 §1, §5).
pub fn project_view(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let Some(project) = deck.projects.selected() else { return div().into_any_element() };
    let name = project.name.clone();
    let agent = project.argv.join(" ");
    let checkouts = project.checkouts.len();
    let checkout = deck.project_checkout().map(|dir| super::prompt::tilde(&dir));
    let cards: Vec<Card> = deck.board().cards().to_vec();
    let count = cards.len();
    let list = if cards.is_empty() {
        div()
            .debug_selector(|| "project-empty-state".into())
            .p(px(space::PAD))
            .flex()
            .items_center()
            .gap(px(space::STACK))
            .rounded(px(radius::CARD))
            .border_1()
            .border_color(tint(Tokens::violet(), 0.25))
            .bg(super::theme::Tokens::well())
            .text_size(size::scaled(size::LABEL))
            .text_color(Tokens::text_dim())
            .child(super::panels::icon_tile(icon::SHIELD, Tokens::violet(), 26.))
            .child("A task sees the project's tracked and untracked files, but not ignored ones.")
    } else {
        div().flex().flex_col().gap(px(space::LINE)).children(cards.iter().map(|card| card_row(card, cx)))
    };
    div()
        .debug_selector(|| "project-view".into())
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .gap(px(space::SECTION))
        .px(px(space::PANE))
        .py(px(space::PANE_V))
        .overflow_hidden()
        .child(header(&name, checkout.as_deref(), checkouts))
        .child(
            eyebrow(&format!("Sessions · {count}"))
                .debug_selector(|| "project-view-sessions".into())
                .pt(px(space::LINE)),
        )
        .child(list)
        .child(div().flex_1())
        .child(
            div()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(space::STACK))
                .child(super::panels::glyph(icon::AGENTS, 10.).text_color(Tokens::text_faint()))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(MONO)
                        .text_size(size::scaled(size::SMALL))
                        .text_color(Tokens::text_faint())
                        .child(agent),
                )
                .child(
                    div()
                        .id("project-view-rules")
                        .debug_selector(|| "project-view-rules".into())
                        .flex_none()
                        .h(px(28.))
                        .px(px(space::INSET))
                        .flex()
                        .items_center()
                        .gap(px(space::TIGHT))
                        .rounded(px(radius::CONTROL))
                        .border_1()
                        .border_color(tint(Tokens::violet(), 0.3))
                        .bg(super::theme::Tokens::well())
                        .text_size(size::scaled(size::SMALL))
                        .text_color(Tokens::text_dim())
                        .cursor_pointer()
                        .hover(|style| style.bg(Tokens::raised_hover()).text_color(Tokens::text()))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_access(cx)))
                        .child(super::panels::glyph(icon::SHIELD, 10.).text_color(Tokens::violet()))
                        .child("Rules and grants"),
                ),
        )
        .into_any_element()
}

/// The project's letter tile, its name, and the selected checkout with the
/// number of checkouts when it has more than one.
fn header(name: &str, checkout: Option<&str>, checkouts: usize) -> AnyElement {
    let line =
        checkout.map(|dir| if checkouts > 1 { format!("{dir} · {checkouts} checkouts") } else { dir.to_string() });
    div()
        .flex()
        .items_center()
        .gap(px(space::STACK))
        .child(super::sidebar_narrow::tile(super::sidebar::initial(name), Tokens::hue(name), true, 40.))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(space::HAIR))
                .child(
                    div()
                        .debug_selector(|| "project-view-name".into())
                        .text_size(size::scaled(size::DISPLAY))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(name.to_string()),
                )
                .children(line.map(|line| {
                    div()
                        .debug_selector(|| "project-view-checkout".into())
                        .font_family(MONO)
                        .text_size(size::scaled(size::SMALL))
                        .text_color(Tokens::text_faint())
                        .child(line)
                })),
        )
        .into_any_element()
}

/// One unsettled task: its state dot and title, its detail line (or the
/// reason it needs you, in amber), and its publication chip.
fn card_row(card: &Card, cx: &mut Context<Deck>) -> AnyElement {
    let lane = &card.lane;
    let id = lane.id;
    // A card that waits on you says so with its whole edge.
    let color = if lane.attention.is_some() { Tokens::amber() } else { state_color(lane.state) };
    let line = match lane.attention {
        Some(word) => div().text_color(Tokens::amber()).child(word),
        None => div().text_color(Tokens::text_faint()).child(lane.detail.clone()),
    };
    let chip = lane.chip.as_ref().map(|(dot, label)| super::publication_panel::chip(*dot, label));
    div()
        .id(("project-view-card", id))
        .debug_selector(move || format!("project-view-card-{id}"))
        .min_w_0()
        .px(px(space::INSET))
        .py(px(space::STACK))
        .flex()
        .items_center()
        .gap(px(space::STACK))
        .rounded(px(radius::CARD))
        .border_1()
        .border_color(if lane.attention.is_some() { tint(color, 0.4) } else { Tokens::edge().into() })
        .bg(Tokens::raised())
        .cursor_pointer()
        .hover(move |style| style.bg(Tokens::raised_hover()).border_color(tint(color, 0.5)))
        .on_click(cx.listener(move |this, _, _, cx| this.select(id, cx)))
        .child(state_mark(if lane.attention.is_some() { super::board::LaneState::Waiting } else { lane.state }, 11.))
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
                        .text_size(size::scaled(size::BODY))
                        .font_weight(FontWeight::MEDIUM)
                        .child(lane.title.clone()),
                )
                .child(div().truncate().text_size(size::scaled(size::SMALL)).child(line)),
        )
        .children(chip)
        .child(super::panels::glyph(icon::CHEVRON_RIGHT, 9.).text_color(Tokens::text_faint()))
        .into_any_element()
}
