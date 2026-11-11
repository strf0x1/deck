//! Transcript rows: the user's turns, agent prose, folded thoughts, and the
//! tool cards, each card a header (status, kind, what it touched, `+n −m`,
//! how long it ran) over a body chosen by the call's kind.
//!
//! The design choice: counts first, content on request. An edit shows a few
//! diff lines around each change, a command the tail of its output, a run
//! of reads one row of file chips; clicking the header expands the rest.
//! A card's tile and verb take its kind's hue (`kind`), its status sits at
//! the right edge, and a running call is lit in the agent's colour.

use super::copy;
use super::panels::{glyph, icon_tile};
use super::prose;
use super::root::Deck;
use super::session::{Entry, Tool};
use super::theme::{MONO, Tokens, glow, icon, radius, size, space, tint};
use super::transcript::{self, Row};
use deck_acp::{ToolKind, ToolStatus};
use gpui::{
    AnyElement, ClickEvent, Context, Div, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px, relative,
};

/// Diff and output rows shown before a card is expanded.
pub(super) const PREVIEW: usize = 8;
/// Output lines an execute card shows before it is expanded.
pub(super) const TAIL: usize = 5;
/// Never draw more than this many lines of one body, even expanded.
pub(super) const CAP: usize = 400;

pub fn row(deck: &Deck, row: &Row, cx: &mut Context<Deck>) -> AnyElement {
    let entries = &deck.session().entries;
    let key = row.key(entries);
    let open = deck.expanded.contains(&key);
    let selected = deck.focused.as_deref() == Some(key.as_str());
    match row {
        Row::Reads(range) => super::folds::reads(&entries[range.clone()], key, open, selected, cx),
        Row::Summary(turn) => super::turn_footer::footer(deck, turn.clone(), cx),
        Row::One(i) => match &entries[*i] {
            Entry::You(t) => div()
                .flex()
                .justify_end()
                .items_center()
                .gap(px(space::TIGHT))
                .child(copy::button(deck, key, t.clone(), cx))
                .child(
                    div()
                        .max_w(relative(0.72))
                        .px(px(space::PAD))
                        .py(px(9.))
                        .rounded(px(radius::BUBBLE))
                        .rounded_br(px(radius::CHIP))
                        .border_1()
                        .border_color(tint(Tokens::accent(), 0.4))
                        .bg(Tokens::sweep(0.16))
                        .line_height(relative(1.5))
                        .child(t.clone()),
                )
                .into_any_element(),
            Entry::Agent(t) => div()
                .flex()
                .gap(px(space::TIGHT))
                .child(div().flex_1().min_w_0().max_w(relative(0.9)).child(prose::prose(t)))
                .child(copy::button(deck, key, t.clone(), cx))
                .into_any_element(),
            Entry::Thought(t) => super::folds::thought(t, key, open, selected, cx),
            Entry::Tool(tool) => card(deck, tool, key, open, cx),
            Entry::Note(t) => div()
                .flex()
                .gap(px(space::LINE))
                .px(px(space::INSET))
                .py(px(space::LINE))
                .rounded(px(radius::CONTROL))
                .border_1()
                .border_color(tint(Tokens::red(), 0.3))
                .bg(tint(Tokens::red(), 0.07))
                .text_color(Tokens::red())
                .child(glyph(icon::WARN, 11.).mt(px(space::HAIR)))
                .child(div().flex_1().min_w_0().child(t.clone()))
                .into_any_element(),
        },
    }
}

pub(super) fn toggle(
    key: String,
    cx: &mut Context<Deck>,
) -> impl Fn(&ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static {
    cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle(&key, cx))
}

pub(super) fn status(s: ToolStatus) -> (&'static str, Hsla) {
    match s {
        ToolStatus::Running => (icon::SPIN, Tokens::cyan()),
        ToolStatus::Done => (icon::CHECK, Tokens::lime()),
        ToolStatus::Failed => (icon::CROSS, Tokens::red()),
    }
}

/// A kind's icon, verb and hue. The hue runs cool to warm by reach:
/// looking, changing files, running code, leaving the machine, deleting.
pub(super) fn kind(k: ToolKind) -> (&'static str, &'static str, Hsla) {
    match k {
        ToolKind::Read => (icon::FILES, "Read", Tokens::sky()),
        ToolKind::Search => (icon::SEARCH, "Search", Tokens::sky()),
        ToolKind::Edit => (icon::EDIT, "Edit", Tokens::violet()),
        ToolKind::Move => (icon::MOVE, "Move", Tokens::violet()),
        ToolKind::Execute => (icon::TERMINAL, "Run", Tokens::cyan()),
        ToolKind::Fetch => (icon::GLOBE, "Fetch", Tokens::orange()),
        ToolKind::Delete => (icon::TRASH, "Delete", Tokens::red()),
        ToolKind::Think => (icon::BULB, "Think", Tokens::text_dim().into()),
        ToolKind::Other => (icon::WRENCH, "Tool", Tokens::text_dim().into()),
    }
}

/// A card's box; the keyboard-selected one has an accent edge.
pub(super) fn frame(selected: bool) -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(radius::CARD))
        .border_1()
        .border_color(if selected { tint(Tokens::accent(), 0.7) } else { Tokens::edge().into() })
        .bg(Tokens::raised())
        .when(selected, |d| d.shadow(glow(Tokens::accent(), 0.4)))
        .overflow_hidden()
}

pub(super) fn mono() -> Div {
    div().font_family(MONO).text_size(px(size::SMALL))
}

pub(super) fn more(n: usize, what: &str, open: bool) -> Div {
    div()
        .px(px(space::INSET))
        .py(px(5.))
        .border_t_1()
        .border_color(Tokens::edge())
        .text_size(px(size::SMALL))
        .bg(Tokens::sunken())
        .text_color(Tokens::text_dim())
        .child(if open { "show less".to_string() } else { format!("{n} more {what} ⌄") })
}

fn card(deck: &Deck, tool: &Tool, key: String, open: bool, cx: &mut Context<Deck>) -> AnyElement {
    let call = &tool.call;
    let (g, color) = status(call.status);
    let (kg, label, hue) = kind(call.kind);
    let changes = transcript::changes(call);
    let (added, removed) = changes.iter().fold((0, 0), |(a, r), c| (a + c.diff.added, r + c.diff.removed));
    let subject = match call.kind {
        ToolKind::Execute => transcript::command(call).unwrap_or_else(|| call.title.clone()),
        ToolKind::Read | ToolKind::Search => transcript::paths(call).join(", "),
        _ => changes.first().map(|c| c.path.clone()).filter(|p| !p.is_empty()).unwrap_or_else(|| call.title.clone()),
    };
    // What a copy button on the header copies: the command, or the path.
    let copyable = match call.kind {
        ToolKind::Execute => transcript::command(call),
        _ => changes.first().map(|c| c.path.clone()).filter(|p| !p.is_empty()),
    };
    let copy = copyable.map(|text| copy::button(deck, format!("{key}:subject"), text, cx));
    let header_key = key.clone();
    let selector = format!("card-{key}");
    let header = div()
        .id(key.clone())
        .debug_selector(move || selector)
        .h(px(space::HEADER))
        .px(px(space::INSET))
        .flex()
        .items_center()
        .pl(px(space::TIGHT))
        .gap(px(9.))
        .cursor_pointer()
        .hover(|s| s.bg(Tokens::raised()))
        .on_click(toggle(key, cx))
        .child(icon_tile(kg, hue, 22.))
        .child(
            div()
                .flex_none()
                .text_size(px(size::LABEL))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(hue)
                .child(label),
        )
        .child(mono().flex_1().min_w_0().truncate().text_color(Tokens::text()).child(subject))
        .when(added + removed > 0, |d| {
            d.child(mono().flex_none().text_color(Tokens::lime()).child(format!("+{added}")))
                .child(mono().flex_none().text_color(Tokens::red()).child(format!("−{removed}")))
        })
        .children(copy)
        .children(transcript::took(tool).map(|t| mono().flex_none().text_color(Tokens::text_faint()).child(t)))
        .child(glyph(g, 11.).flex_none().text_color(color));
    let body = match call.kind {
        ToolKind::Execute => super::card_body::output(&transcript::output(call), open),
        _ if !changes.is_empty() => Some(super::card_body::diff(&changes, open)),
        _ if open => super::card_body::output(&transcript::output(call), true),
        _ => None,
    };
    let fade = deck.motion.reveal(&header_key);
    let selected = deck.focused.as_deref() == Some(header_key.as_str());
    // A running call is lit in the agent's colour; a failed one keeps a red edge.
    let edge = match call.status {
        _ if selected => None,
        ToolStatus::Running => Some(Tokens::cyan()),
        ToolStatus::Failed => Some(Tokens::red()),
        ToolStatus::Done => None,
    };
    frame(selected)
        .when_some(edge, |d, edge| d.border_color(tint(edge, 0.4)))
        .when(!selected && call.status == ToolStatus::Running, |d| d.shadow(glow(Tokens::cyan(), 0.3)))
        .child(header)
        .children(body.map(|b| div().opacity(fade).child(b)))
        .into_any_element()
}
