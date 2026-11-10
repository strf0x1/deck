//! The project view: a project's header, its unsettled tasks as cards, and a
//! footer naming the agent and opening its rules (ADR 0010 §1).
//!
//! The design choice: the cards are the board's own, so the project view and
//! the sidebar's task rows never disagree about order, titles or detail; a
//! click selects the card's lane exactly as a sidebar row does.

use super::board::Card;
use super::panels::state_color;
use super::root::Deck;
use super::theme::{MONO, Tokens, icon, radius, size, space};
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

/// The selected project with no selected task: what the main area shows
/// (ADR 0010 §1, §5).
pub fn project_view(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let Some(project) = deck.projects.selected() else { return div().into_any_element() };
    let name = project.name.clone();
    let agent = format!("Agent: {}", project.argv.join(" "));
    let checkouts = project.checkouts.len();
    let checkout = deck.project_checkout().map(|dir| super::prompt::tilde(&dir));
    let cards: Vec<Card> = deck.board().cards().to_vec();
    let count = cards.len();
    let list = if cards.is_empty() {
        div()
            .debug_selector(|| "project-empty-state".into())
            .py(px(space::PANE_V))
            .text_size(px(size::LABEL))
            .text_color(Tokens::text_dim())
            .child("A task sees the project's tracked and untracked files, but not ignored ones.")
    } else {
        div().flex().flex_col().gap(px(space::TIGHT)).children(cards.iter().map(|card| card_row(card, cx)))
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
            div()
                .debug_selector(|| "project-view-sessions".into())
                .text_size(px(size::SMALL))
                .text_color(Tokens::text_faint())
                .child(format!("Sessions · {count}")),
        )
        .child(list)
        .child(div().flex_1())
        .child(
            div()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(space::STACK))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(MONO)
                        .text_size(px(size::SMALL))
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
                        .border_color(Tokens::edge())
                        .text_size(px(size::SMALL))
                        .text_color(Tokens::text_dim())
                        .cursor_pointer()
                        .hover(|style| style.bg(Tokens::raised_hover()).text_color(Tokens::text()))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_access(cx)))
                        .child(super::panels::glyph(icon::SHIELD, 10.))
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
        .child(super::sidebar_narrow::tile(super::sidebar::initial(name), true, 32.))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(space::HAIR))
                .child(
                    div()
                        .debug_selector(|| "project-view-name".into())
                        .text_size(px(size::DISPLAY))
                        .font_weight(FontWeight::MEDIUM)
                        .child(name.to_string()),
                )
                .children(line.map(|line| {
                    div()
                        .debug_selector(|| "project-view-checkout".into())
                        .font_family(MONO)
                        .text_size(px(size::SMALL))
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
    let color = state_color(lane.state);
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
        .rounded(px(radius::CONTROL))
        .bg(Tokens::raised())
        .cursor_pointer()
        .hover(|style| style.bg(Tokens::raised_hover()))
        .on_click(cx.listener(move |this, _, _, cx| this.select(id, cx)))
        .child(div().flex_none().size(px(7.)).rounded_full().bg(color))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(space::HAIR))
                .child(div().truncate().text_size(px(size::BODY)).child(lane.title.clone()))
                .child(div().truncate().text_size(px(size::SMALL)).child(line)),
        )
        .children(chip)
        .into_any_element()
}
