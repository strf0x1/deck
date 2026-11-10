//! The sidebar's open rows: the Needs-you section and each project's header,
//! plus the task list and settled fold they open (ADR 0010 §7, §9).
//!
//! The design choice: the row builders are free functions over the rows
//! `sidebar::rows` returns, so the shell (`sidebar_view.rs`) stays a layout.

use super::root::Deck;
use super::sidebar::Row;
use super::sidebar_tasks::{settled_fold, tasks_view};
use super::theme::{Tokens, radius, size, space};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, AppContext as _, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

/// The open rows, top to bottom: the needs section, each project, the selected
/// project's tasks, then the settled fold.
pub(super) fn rows_view(deck: &mut Deck, rows: &[Row], cx: &mut Context<Deck>) -> AnyElement {
    let children: Vec<AnyElement> = rows
        .iter()
        .map(|row| match row {
            Row::NeedsHeader { count } => needs_header(*count),
            Row::Need { index, title, word, text } => need_row(*index, title, word, text.clone(), cx),
            Row::Nothing => nothing(),
            Row::Project { index, name, tasks, attention, selected, .. } => {
                project_row(*index, name, *tasks, *attention, *selected, cx)
            }
            Row::Tasks => tasks_view(deck, cx),
            Row::Settled { count } => settled_fold(deck, *count, cx),
        })
        .collect();
    div().flex().flex_col().gap(px(space::HAIR)).children(children).into_any_element()
}

fn needs_header(count: usize) -> AnyElement {
    div()
        .debug_selector(|| "sidebar-needs-header".into())
        .h(px(28.))
        .px(px(space::TIGHT))
        .flex()
        .items_center()
        .text_size(px(size::SMALL))
        .text_color(Tokens::text_faint())
        .child(format!("Needs you ({count})"))
        .into_any_element()
}

fn need_row(index: usize, title: &str, word: &'static str, text: String, cx: &mut Context<Deck>) -> AnyElement {
    div()
        .id(("sidebar-need", index))
        .debug_selector(move || format!("sidebar-need-{index}"))
        .h(px(super::rail::ITEM_H))
        .px(px(space::TIGHT))
        .flex()
        .items_center()
        .gap(px(space::LINE))
        .rounded(px(radius::CONTROL))
        .cursor_pointer()
        .hover(|style| style.bg(Tokens::raised()))
        .on_click(cx.listener(move |this, _, _, cx| this.open_item(index, cx)))
        .child(div().flex_none().size(px(6.)).rounded_full().bg(Tokens::amber()))
        .child(div().flex_1().min_w_0().truncate().text_size(px(size::BODY)).child(title.to_string()))
        .child(div().flex_none().text_size(px(size::SMALL)).text_color(Tokens::amber()).child(word))
        .tooltip(move |_, cx| {
            let text = text.clone();
            cx.new(|_| Tooltip { text }).into()
        })
        .into_any_element()
}

fn nothing() -> AnyElement {
    div()
        .debug_selector(|| "sidebar-nothing".into())
        .h(px(super::rail::ITEM_H))
        .px(px(space::TIGHT))
        .flex()
        .items_center()
        .text_size(px(size::SMALL))
        .text_color(Tokens::text_faint())
        .child("Nothing needs you")
        .into_any_element()
}

fn project_row(
    index: usize,
    name: &str,
    tasks: usize,
    attention: bool,
    selected: bool,
    cx: &mut Context<Deck>,
) -> AnyElement {
    let fg = if selected { Tokens::text() } else { Tokens::text_dim() };
    let dot: gpui::Hsla = if attention { Tokens::amber() } else { Tokens::text_faint().into() };
    let row = div()
        .id(("sidebar-project", index))
        .debug_selector(move || format!("sidebar-project-{index}"))
        .h(px(super::rail::ITEM_H))
        .px(px(space::TIGHT))
        .flex()
        .items_center()
        .gap(px(space::LINE))
        .rounded(px(radius::CONTROL))
        .cursor_pointer()
        .text_color(fg)
        .when(selected, |s| s.bg(Tokens::raised_hover()))
        .hover(|style| style.bg(Tokens::raised_hover()))
        .on_click(cx.listener(move |this, _, _, cx| this.select_project(index, cx)))
        .child(div().flex_none().size(px(6.)).rounded_full().bg(dot))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_weight(if selected { FontWeight::MEDIUM } else { FontWeight::NORMAL })
                .child(name.to_string()),
        )
        .child(div().flex_none().text_size(px(size::SMALL)).text_color(Tokens::text_faint()).child(tasks.to_string()));
    row.into_any_element()
}

/// A hover tooltip for a row's full reason.
struct Tooltip {
    text: String,
}

impl gpui::Render for Tooltip {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .px(px(space::INSET))
            .py(px(space::TIGHT))
            .max_w(px(320.))
            .rounded(px(radius::CONTROL))
            .border_1()
            .border_color(Tokens::edge())
            .bg(Tokens::float())
            .text_size(px(size::SMALL))
            .text_color(Tokens::text())
            .child(self.text.clone())
    }
}
