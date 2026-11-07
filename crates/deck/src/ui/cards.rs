//! Transcript rows: the user's turns, agent prose, folded thoughts, and the
//! tool cards, each card a header (status, kind, what it touched, `+n −m`,
//! how long it ran) over a body chosen by the call's kind.
//!
//! The design choice: counts first, content on request. An edit shows a few
//! diff lines around each change, a command the tail of its output, a run
//! of reads one row of file chips; clicking the header expands the rest.
//! Colour stays in the status glyph and the diff's added and removed lines.

use super::copy;
use super::panels::glyph;
use super::prose;
use super::root::Deck;
use super::session::{Entry, Tool};
use super::theme::{MONO, Tokens, icon, radius, size, space, tint};
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
    let entries = &deck.session.entries;
    let key = row.key(entries);
    let open = deck.expanded.contains(&key);
    let selected = deck.selected.as_deref() == Some(key.as_str());
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
                        .border_1()
                        .border_color(tint(Tokens::accent(), 0.22))
                        .bg(tint(Tokens::accent(), 0.12))
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
            Entry::Note(t) => div().text_color(Tokens::red()).child(t.clone()).into_any_element(),
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
        ToolStatus::Running => (icon::SPIN, Tokens::mint()),
        ToolStatus::Done => (icon::CHECK, Tokens::lime()),
        ToolStatus::Failed => (icon::CROSS, Tokens::red()),
    }
}

fn kind(k: ToolKind) -> (&'static str, &'static str) {
    match k {
        ToolKind::Read => (icon::FILES, "Read"),
        ToolKind::Edit => (icon::EDIT, "Edit"),
        ToolKind::Delete => (icon::TRASH, "Delete"),
        ToolKind::Move => (icon::MOVE, "Move"),
        ToolKind::Search => (icon::SEARCH, "Search"),
        ToolKind::Execute => (icon::TERMINAL, "Run"),
        ToolKind::Think => (icon::BULB, "Think"),
        ToolKind::Fetch => (icon::GLOBE, "Fetch"),
        ToolKind::Other => (icon::WRENCH, "Tool"),
    }
}

/// A card's box; the keyboard-selected one has an accent edge.
pub(super) fn frame(selected: bool) -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(radius::CARD))
        .border_1()
        .border_color(if selected { tint(Tokens::accent(), 0.6) } else { Tokens::edge().into() })
        .bg(Tokens::raised())
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
        .text_color(Tokens::text_faint())
        .child(if open { "show less".to_string() } else { format!("{n} more {what} ⌄") })
}

fn card(deck: &Deck, tool: &Tool, key: String, open: bool, cx: &mut Context<Deck>) -> AnyElement {
    let call = &tool.call;
    let (g, color) = status(call.status);
    let (kg, label) = kind(call.kind);
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
        .gap(px(9.))
        .cursor_pointer()
        .on_click(toggle(key, cx))
        .child(glyph(g, 10.).text_color(color))
        .child(glyph(kg, 10.).text_color(Tokens::text_faint()))
        .child(div().flex_none().text_size(px(size::LABEL)).text_color(Tokens::text_dim()).child(label))
        .child(mono().flex_1().min_w_0().truncate().text_color(Tokens::text()).child(subject))
        .when(added + removed > 0, |d| {
            d.child(mono().flex_none().text_color(Tokens::lime()).child(format!("+{added}")))
                .child(mono().flex_none().text_color(Tokens::red()).child(format!("−{removed}")))
        })
        .children(copy)
        .children(transcript::took(tool).map(|t| mono().flex_none().text_color(Tokens::text_faint()).child(t)));
    let body = match call.kind {
        ToolKind::Execute => super::card_body::output(&transcript::output(call), open),
        _ if !changes.is_empty() => Some(super::card_body::diff(&changes, open)),
        _ if open => super::card_body::output(&transcript::output(call), true),
        _ => None,
    };
    let fade = deck.motion.reveal(&header_key);
    frame(deck.selected.as_deref() == Some(header_key.as_str()))
        .child(header)
        .children(body.map(|b| div().opacity(fade).child(b)))
        .into_any_element()
}
