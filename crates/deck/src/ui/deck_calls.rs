//! A coordinator's `deck` calls drawn by operation (ADR 0011 §9): `create`
//! and `prompt` as one quiet line naming the task, `stop` as one that stands
//! out (it ended a session the user may have been relying on; after a cap
//! refusal it says it made room, #202), `tasks` and `task` folded like a
//! read, and `wait` as a faint line while it polls.
//!
//! The design choice: a `deck` call is the coordinator acting on other tasks,
//! so what matters is which task, not the call; every task it names is a
//! chip that selects that task, across projects if needed. The operation
//! wears the agent's teal; nothing here looks like a stuck tool card, since a
//! `wait` may run for five minutes by design.

use super::cards::{mono, toggle};
use super::deck_tool::{self, Call, Op};
use super::glyph::glyph;
use super::root::Deck;
use super::selectable::{RowRuns, Text};
use super::session::{Entry, Tool};
use super::theme::{Tokens, icon, radius, size, space, tint};
use deck_acp::ToolStatus;
use gpui::{
    AnyElement, Context, Div, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px, relative,
};

/// The ids of every task the window knows, for finding them in a reply.
pub(super) fn known(deck: &Deck) -> Vec<&str> {
    deck.tasks().iter().map(|task| task.id.as_str()).collect()
}

/// One `deck` call as its line; a folding read opens to its reply.
/// `made_room` marks a `stop` that freed a slot after a cap refusal.
pub(super) fn line(
    deck: &Deck,
    tool: &Tool,
    call: &Call,
    made_room: bool,
    key: String,
    runs: &RowRuns,
    cx: &mut Context<Deck>,
) -> AnyElement {
    let open = deck.is_expanded(&key);
    let selected = deck.selected_card() == Some(key.as_str());
    let running = tool.call.status == ToolStatus::Running;
    let chips: Vec<AnyElement> = call.ids.iter().map(|id| chip(deck, id, call.title.as_deref(), cx)).collect();
    let selector = format!("deck-call-{key}");
    let quiet = call.op == Op::Wait;
    let head = row(selected)
        .id(key.clone())
        .debug_selector(move || selector)
        .when(call.op.folds(), |d| d.cursor_pointer().hover(|s| s.bg(Tokens::raised())).on_click(toggle(key, cx)))
        .child(match call.op {
            // The header's own Stop mark: a session ended.
            Op::Stop => glyph(icon::STOP, 11.).text_color(Tokens::red()),
            _ => glyph(icon::AGENTS, 12.).text_color(tint(Tokens::cyan(), if quiet { 0.5 } else { 0.85 })),
        })
        .children(match call.op {
            // A poll says what it waits for, not that a tool runs.
            Op::Wait if running => vec![words("waiting on", true)],
            Op::Wait => vec![words("waited on", true)],
            Op::Stop => vec![words("coordinator", true), stopped(if running { "stopping" } else { "stopped" })],
            op => vec![words("deck", true), verb(op.name())],
        })
        .when(chips.is_empty() && call.op == Op::Create, |d| d.child(words("a new session…", true)))
        .children(chips)
        .children(match call.op {
            Op::Wait if !running => Some(words(&format!("· {}", deck_tool::woke(&tool.call)), true)),
            Op::Tasks if !running => Some(words(&listed(&tool.call), true)),
            Op::Stop if made_room => Some(
                div().debug_selector(|| "deck-made-room".into()).child(words("to make room", false)).into_any_element(),
            ),
            _ => None,
        })
        .when(running && !quiet, |d| d.child(glyph(icon::SPIN, 11.).text_color(tint(Tokens::cyan(), 0.6))))
        .when(call.op.folds(), |d| d.child(glyph(if open { icon::CHEVRON_DOWN } else { icon::CHEVRON_RIGHT }, 10.)));
    let body = (open && call.op.folds())
        .then(|| super::runs::output_tail(&super::transcript::output(&tool.call), true))
        .flatten()
        .map(|(text, _)| {
            mono()
                .font_family(deck.typography.code.family())
                .ml(px(20.))
                .px(px(space::INSET))
                .py(px(space::LINE))
                .rounded(px(radius::CONTROL))
                .border_1()
                .border_color(Tokens::edge())
                .bg(Tokens::sunken())
                .text_color(Tokens::text_dim())
                .line_height(relative(1.45))
                .child(runs.text(Text::Plain(text)))
        });
    div().flex().flex_col().gap(px(space::DETAIL)).child(head).children(body).into_any_element()
}

/// A run of `wait` polls on one line: what it waits on, and how many polls.
pub(super) fn waits(deck: &Deck, entries: &[Entry], key: String, cx: &mut Context<Deck>) -> AnyElement {
    let known = known(deck);
    let calls: Vec<(&Tool, Call)> = entries
        .iter()
        .filter_map(|e| if let Entry::Tool(t) = e { Some(t) } else { None })
        .filter_map(|t| deck_tool::parse(&t.call, &known).map(|c| (t, c)))
        .collect();
    let running = calls.iter().any(|(t, _)| t.call.status == ToolStatus::Running);
    let mut ids: Vec<&str> = Vec::new();
    for id in calls.iter().flat_map(|(_, c)| c.ids.iter()) {
        if !ids.contains(&id.as_str()) {
            ids.push(id);
        }
    }
    let chips: Vec<AnyElement> = ids.iter().map(|id| chip(deck, id, None, cx)).collect();
    let selector = format!("deck-call-{key}");
    row(false)
        .id(key)
        .debug_selector(move || selector)
        .child(glyph(icon::AGENTS, 12.).text_color(tint(Tokens::cyan(), 0.5)))
        .child(words(if running { "waiting on" } else { "waited on" }, true))
        .children(chips)
        .child(words(&format!("· {} polls", calls.len()), true))
        .into_any_element()
}

/// A task the call names: its title, selecting it on click.
fn chip(deck: &Deck, id: &str, asked: Option<&str>, cx: &mut Context<Deck>) -> AnyElement {
    let record = deck.tasks().iter().find(|task| task.id == id);
    let title = match (record, asked) {
        (Some(task), _) => super::tasks::title(task),
        (None, Some(title)) => title.to_string(),
        (None, None) => deck_tool::short(id),
    };
    let target = id.to_string();
    let selector = format!("deck-chip-{id}");
    div()
        .id(gpui::ElementId::Name(format!("deck-chip-{id}").into()))
        .debug_selector(move || selector)
        .flex_none()
        .max_w(px(260.))
        .min_h(px(20.))
        .px(px(space::LINE))
        .flex()
        .items_center()
        .rounded_full()
        .border_1()
        .border_color(Tokens::edge_strong())
        .bg(Tokens::well())
        .text_size(size::scaled(size::SMALL))
        .text_color(Tokens::text())
        .cursor_pointer()
        .hover(|s| s.border_color(tint(Tokens::accent(), 0.6)).bg(Tokens::raised_hover()))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.select_task_id(&target, cx);
        }))
        .child(div().min_w_0().truncate().child(title))
        .into_any_element()
}

/// A `deck` line's frame: one row, the keyboard selection's background.
fn row(selected: bool) -> Div {
    div()
        .min_h(px(26.))
        .px(px(space::TIGHT))
        .mx(px(-space::TIGHT))
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(space::LINE))
        .rounded(px(radius::CHIP))
        .when(selected, |d| d.bg(Tokens::raised()))
        .text_size(size::scaled(size::LABEL))
}

fn words(text: &str, faint: bool) -> AnyElement {
    div()
        .flex_none()
        .text_color(if faint { Tokens::text_faint() } else { Tokens::text_dim() })
        .child(text.to_string())
        .into_any_element()
}

/// The operation, in the agent's colour.
fn verb(name: &'static str) -> AnyElement {
    div().flex_none().font_weight(gpui::FontWeight::SEMIBOLD).text_color(Tokens::cyan()).child(name).into_any_element()
}

/// A `stop`'s verb: in plain text, not the agent's teal, so it reads as a
/// session ending rather than routine work.
fn stopped(word: &'static str) -> AnyElement {
    div()
        .flex_none()
        .debug_selector(|| "deck-stopped".into())
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Tokens::text())
        .child(word)
        .into_any_element()
}

/// How many tasks a listing returned, from its reply's first line.
fn listed(call: &deck_acp::ToolCall) -> String {
    let output = super::transcript::output(call);
    let first = output.lines().next().unwrap_or_default().trim_end_matches(':');
    format!("· {first}")
}
