//! Transcript rows: the user's turns, agent prose, folded thoughts, and the
//! tool cards, each card a header (status, kind, what it touched, `+n −m`,
//! how long it ran) over a body chosen by the call's kind.
//!
//! The design choice: counts first, content on request. An edit shows a few
//! diff lines around each change, a command the tail of its output, a run
//! of reads one row of file chips; clicking the header expands the rest.
//! Colour stays in the status glyph and the diff's added and removed lines.

use super::diff::{self, Row as DiffRow};
use super::panels::glyph;
use super::prose;
use super::root::Deck;
use super::session::{Entry, Tool};
use super::theme::{MONO, Tokens, icon, tint};
use super::transcript::{self, Row};
use deck_acp::{ToolKind, ToolStatus};
use gpui::{
    AnyElement, ClickEvent, Context, Div, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px, relative,
};

/// Diff and output rows shown before a card is expanded.
const PREVIEW: usize = 8;
/// Output lines an execute card shows before it is expanded.
const TAIL: usize = 5;
/// Never draw more than this many lines of one body, even expanded.
const CAP: usize = 400;

pub fn row(deck: &Deck, row: &Row, cx: &mut Context<Deck>) -> AnyElement {
    let entries = &deck.session.entries;
    let key = row.key(entries);
    let open = deck.expanded.contains(&key);
    match row {
        Row::Reads(range) => reads(&entries[range.clone()], key, open, cx),
        Row::One(i) => match &entries[*i] {
            Entry::You(t) => div()
                .flex()
                .justify_end()
                .child(
                    div()
                        .max_w(relative(0.72))
                        .px(px(14.))
                        .py(px(9.))
                        .rounded(px(14.))
                        .border_1()
                        .border_color(tint(Tokens::accent(), 0.22))
                        .bg(tint(Tokens::accent(), 0.12))
                        .line_height(relative(1.5))
                        .child(t.clone()),
                )
                .into_any_element(),
            Entry::Agent(t) => div().max_w(relative(0.9)).child(prose::prose(t)).into_any_element(),
            Entry::Thought(t) => thought(t, key, open, cx),
            Entry::Tool(tool) => card(tool, key, open, cx),
            Entry::Note(t) => div().text_color(Tokens::red()).child(t.clone()).into_any_element(),
        },
    }
}

/// A reasoning trace: one faint line until opened.
fn thought(text: &str, key: String, open: bool, cx: &mut Context<Deck>) -> AnyElement {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().to_string();
    div()
        .id(key.clone())
        .flex()
        .gap(px(8.))
        .cursor_pointer()
        .text_color(Tokens::text_faint())
        .italic()
        .on_click(toggle(key, cx))
        .child(glyph(icon::BULB, 10.).mt(px(4.)))
        .child(if open {
            div().flex_1().min_w_0().line_height(relative(1.5)).child(text.to_string())
        } else {
            div().flex_1().min_w_0().truncate().child(first)
        })
        .into_any_element()
}

fn toggle(key: String, cx: &mut Context<Deck>) -> impl Fn(&ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static {
    cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle(&key, cx))
}

fn status(s: ToolStatus) -> (&'static str, Hsla) {
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

fn frame() -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(12.))
        .border_1()
        .border_color(Tokens::edge())
        .bg(Tokens::raised())
        .overflow_hidden()
}

fn mono() -> Div {
    div().font_family(MONO).text_size(px(11.5))
}

fn more(n: usize, what: &str, open: bool) -> Div {
    div()
        .px(px(12.))
        .py(px(5.))
        .border_t_1()
        .border_color(Tokens::edge())
        .text_size(px(11.))
        .text_color(Tokens::text_faint())
        .child(if open { "show less".to_string() } else { format!("{n} more {what} ⌄") })
}

fn card(tool: &Tool, key: String, open: bool, cx: &mut Context<Deck>) -> AnyElement {
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
    let selector = format!("card-{key}");
    let header = div()
        .id(key.clone())
        .debug_selector(move || selector)
        .h(px(34.))
        .px(px(12.))
        .flex()
        .items_center()
        .gap(px(9.))
        .cursor_pointer()
        .on_click(toggle(key, cx))
        .child(glyph(g, 10.).text_color(color))
        .child(glyph(kg, 10.).text_color(Tokens::text_faint()))
        .child(div().flex_none().text_size(px(12.)).text_color(Tokens::text_dim()).child(label))
        .child(mono().flex_1().min_w_0().truncate().text_color(Tokens::text()).child(subject))
        .when(added + removed > 0, |d| {
            d.child(mono().flex_none().text_color(Tokens::lime()).child(format!("+{added}")))
                .child(mono().flex_none().text_color(Tokens::red()).child(format!("−{removed}")))
        })
        .children(transcript::took(tool).map(|t| mono().flex_none().text_color(Tokens::text_faint()).child(t)));
    let body = match call.kind {
        ToolKind::Execute => output_body(&transcript::output(call), open),
        _ if !changes.is_empty() => Some(diff_body(&changes, open)),
        _ if open => output_body(&transcript::output(call), true),
        _ => None,
    };
    frame().child(header).children(body).into_any_element()
}

fn diff_body(changes: &[transcript::Change], open: bool) -> AnyElement {
    let mut cut_total = 0;
    let mut out = div().flex().flex_col().border_t_1().border_color(Tokens::edge()).py(px(4.));
    for (n, change) in changes.iter().enumerate() {
        let (rows, cut) = change.diff.preview(2, if open { CAP } else { PREVIEW });
        cut_total += cut;
        if changes.len() > 1 {
            out = out.child(
                mono()
                    .px(px(12.))
                    .pt(px(if n == 0 { 2. } else { 8. }))
                    .pb(px(2.))
                    .text_color(Tokens::text_dim())
                    .child(change.path.clone()),
            );
        }
        out = out.children(rows.into_iter().map(|r| diff_line(r, change.numbered)));
    }
    let toggle = (cut_total > 0 || open).then(|| more(cut_total, "lines", open));
    div().flex().flex_col().child(out).children(toggle).into_any_element()
}

fn diff_line(row: DiffRow<'_>, numbered: bool) -> Div {
    let line = match row {
        DiffRow::Gap(n) => {
            return mono()
                .px(px(12.))
                .py(px(1.))
                .text_color(Tokens::text_faint())
                .child(format!("⋯ {n} unchanged line{}", if n == 1 { "" } else { "s" }));
        }
        DiffRow::Line(l) => l,
    };
    let (sign, fg, bg): (&str, Hsla, Option<Hsla>) = match line.kind {
        diff::Kind::Same => (" ", Tokens::text_dim().into(), None),
        diff::Kind::Added => ("+", Tokens::lime(), Some(tint(Tokens::lime(), 0.09))),
        diff::Kind::Removed => ("−", Tokens::red(), Some(tint(Tokens::red(), 0.09))),
    };
    let number =
        if numbered { line.new.or(line.old).map(|n| n.to_string()).unwrap_or_default() } else { String::new() };
    mono()
        .flex()
        .px(px(12.))
        .line_height(px(18.))
        .when_some(bg, |d, bg| d.bg(bg))
        // A diff built from an edit's arguments has no file line numbers to show.
        .when(numbered, |d| d.child(div().flex_none().w(px(34.)).text_color(Tokens::text_faint()).child(number)))
        .child(div().flex_none().w(px(14.)).text_color(fg).child(sign))
        .child(div().flex_1().min_w_0().text_color(fg).child(line.text.clone()))
}

fn output_body(text: &str, open: bool) -> Option<AnyElement> {
    if text.trim().is_empty() {
        return None;
    }
    let (lines, hidden) = transcript::tail(text, if open { CAP } else { TAIL });
    let body = mono()
        .flex()
        .flex_col()
        .px(px(12.))
        .py(px(7.))
        .border_t_1()
        .border_color(Tokens::edge())
        .line_height(px(17.))
        .text_color(Tokens::text_dim())
        .children(lines.into_iter().map(|l| div().child(l.to_string())));
    let toggle = (hidden > 0 || open).then(|| more(hidden, "earlier lines", open));
    Some(div().flex().flex_col().child(body).children(toggle).into_any_element())
}

/// A run of reads and searches: one row of file chips, a list when opened.
fn reads(entries: &[Entry], key: String, open: bool, cx: &mut Context<Deck>) -> AnyElement {
    let tools: Vec<&Tool> =
        entries.iter().filter_map(|e| if let Entry::Tool(t) = e { Some(t) } else { None }).collect();
    let failed = tools.iter().any(|t| t.call.status == ToolStatus::Failed);
    let running = tools.iter().any(|t| t.call.status == ToolStatus::Running);
    let (g, color) = status(if running {
        ToolStatus::Running
    } else if failed {
        ToolStatus::Failed
    } else {
        ToolStatus::Done
    });
    let paths: Vec<String> = tools.iter().flat_map(|t| transcript::paths(&t.call)).collect();
    let chip = |p: &str| {
        mono()
            .flex_none()
            .px(px(7.))
            .py(px(2.))
            .rounded(px(6.))
            .bg(Tokens::raised_hover())
            .text_color(Tokens::text_dim())
            .child(transcript::file_name(p).to_string())
    };
    let shown = 4;
    let header = div()
        .id(key.clone())
        .min_h(px(34.))
        .px(px(12.))
        .flex()
        .items_center()
        .gap(px(9.))
        .cursor_pointer()
        .on_click(toggle(key, cx))
        .child(glyph(g, 10.).text_color(color))
        .child(glyph(icon::FILES, 10.).text_color(Tokens::text_faint()))
        .child(
            div()
                .flex_none()
                .text_size(px(12.))
                .text_color(Tokens::text_dim())
                .child(format!("Looked at {}", tools.len())),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .gap(px(5.))
                .overflow_hidden()
                .children(paths.iter().take(shown).map(|p| chip(p)))
                .when(paths.len() > shown, |d| {
                    d.child(
                        mono().flex_none().text_color(Tokens::text_faint()).child(format!("+{}", paths.len() - shown)),
                    )
                }),
        );
    let list = open.then(|| {
        mono().flex().flex_col().px(px(12.)).py(px(6.)).gap(px(3.)).border_t_1().border_color(Tokens::edge()).children(
            tools.iter().map(|t| {
                let (g, color) = status(t.call.status);
                div()
                    .flex()
                    .gap(px(8.))
                    .child(glyph(g, 9.).text_color(color))
                    .child(div().text_color(Tokens::text_dim()).child(t.call.title.clone()))
            }),
        )
    });
    frame().child(header).children(list).into_any_element()
}
