//! Rows folded to one line until opened: a reasoning trace, and a run of
//! reads and searches shown as file chips.
//!
//! The design choice: what the agent looked at and thought matters less
//! than what it changed, so it takes one quiet line, and the full trace
//! or list is a click away.

use super::cards::{frame, mono, status, toggle};
use super::panels::{glyph, icon_tile};
use super::root::Deck;
use super::session::{Entry, Tool};
use super::theme::{Tokens, icon, radius, size, space, tint};
use super::transcript;
use deck_acp::ToolStatus;
use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, StatefulInteractiveElement as _,
    Styled as _, div, prelude::FluentBuilder as _, px, relative,
};

/// A reasoning trace: one faint line until opened.
pub fn thought(text: &str, key: String, open: bool, selected: bool, cx: &mut Context<Deck>) -> AnyElement {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().to_string();
    div()
        .id(key.clone())
        .flex()
        .gap(px(space::LINE))
        .px(px(space::TIGHT))
        .mx(px(-space::TIGHT))
        .rounded(px(radius::CHIP))
        .when(selected, |d| d.bg(tint(Tokens::accent(), 0.1)))
        .cursor_pointer()
        .text_color(Tokens::text_faint())
        .italic()
        .hover(|s| s.text_color(Tokens::text_dim()))
        .on_click(toggle(key, cx))
        .child(glyph(icon::BULB, 10.).mt(px(space::DETAIL)).text_color(tint(Tokens::amber(), 0.7)))
        .child(if open {
            div().flex_1().min_w_0().line_height(relative(1.5)).child(text.to_string())
        } else {
            div().flex_1().min_w_0().truncate().child(first)
        })
        .into_any_element()
}

/// A run of reads and searches: one row of file chips, a list when opened.
pub fn reads(entries: &[Entry], key: String, open: bool, selected: bool, cx: &mut Context<Deck>) -> AnyElement {
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
            .py(px(space::HAIR))
            .rounded(px(radius::CHIP))
            .bg(tint(Tokens::sky(), 0.1))
            .text_color(tint(Tokens::sky(), 0.9))
            .child(transcript::file_name(p).to_string())
    };
    let shown = 4;
    let header = div()
        .id(key.clone())
        .min_h(px(space::HEADER))
        .px(px(space::INSET))
        .pl(px(space::TIGHT))
        .flex()
        .items_center()
        .gap(px(9.))
        .cursor_pointer()
        .hover(|s| s.bg(Tokens::raised()))
        .on_click(toggle(key, cx))
        .child(icon_tile(icon::EYE, Tokens::sky(), 22.))
        .child(
            div()
                .flex_none()
                .text_size(size::scaled(size::LABEL))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Tokens::sky())
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
        )
        .child(glyph(g, 11.).flex_none().text_color(color));
    let list = open.then(|| {
        mono()
            .flex()
            .flex_col()
            .px(px(space::INSET))
            .py(px(space::TIGHT))
            .gap(px(3.))
            .border_t_1()
            .border_color(Tokens::edge())
            .bg(Tokens::sunken())
            .children(tools.iter().map(|t| {
                let (g, color) = status(t.call.status);
                div()
                    .flex()
                    .gap(px(space::LINE))
                    .child(glyph(g, 9.).text_color(color))
                    .child(div().text_color(Tokens::text_dim()).child(t.call.title.clone()))
            }))
    });
    frame(selected).child(header).children(list).into_any_element()
}
