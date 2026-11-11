//! The sidebar's open rows: the Needs-you section and each project's header,
//! plus the task list and settled fold they open (ADR 0010 §7, §9).
//!
//! The design choice: the row builders are free functions over the rows
//! `sidebar::rows` returns, so the shell (`sidebar_view.rs`) stays a layout.

use super::root::Deck;
use super::sidebar::{Row, initial};
use super::sidebar_tasks::{settled_fold, tasks_view};
use super::theme::{Tokens, icon, radius, size, space};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, AppContext as _, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

/// The chevron's column and the project tile, which together set how far the
/// selected project's task rows indent.
pub(super) const CHEVRON_W: f32 = 12.;
pub(super) const TILE: f32 = 20.;
/// Where a task row starts so its dot sits under the project's tile.
pub(super) const TASK_INDENT: f32 = space::TIGHT + CHEVRON_W + space::LINE;

/// The open rows, top to bottom: the needs section, each project, the selected
/// project's tasks, then the settled fold.
pub(super) fn rows_view(deck: &mut Deck, rows: &[Row], cx: &mut Context<Deck>) -> AnyElement {
    let children: Vec<AnyElement> = rows
        .iter()
        .map(|row| match row {
            Row::NeedsHeader { count } => needs_header(*count),
            Row::Need { index, project_name, title, word, text } => {
                need_row(*index, project_name, title, word, text.clone(), cx)
            }
            Row::Nothing => nothing(),
            Row::ProjectsHeader => projects_header(),
            Row::Project { selected, .. } => {
                // The row marks the place shown: its project view, not a task.
                let shown = *selected && deck.task_id().is_none();
                project_row(row, shown, cx)
            }
            Row::Tasks => tasks_view(deck, cx),
            Row::Settled { count } => settled_fold(deck, *count, cx),
        })
        .collect();
    div().flex().flex_col().gap(px(space::HAIR)).children(children).into_any_element()
}

fn needs_header(count: usize) -> AnyElement {
    section_label(&format!("Needs you · {count}"), "sidebar-needs-header", (count > 0).then(Tokens::amber))
}

fn projects_header() -> AnyElement {
    div().mt(px(space::LINE)).child(section_label("Projects", "sidebar-projects-header", None)).into_any_element()
}

/// A section's heading; `lit` gives it the colour of what waits under it.
fn section_label(label: &str, selector: &'static str, lit: Option<gpui::Hsla>) -> AnyElement {
    let heading = super::panels::eyebrow(label);
    div()
        .debug_selector(move || selector.into())
        .h(px(28.))
        .px(px(space::TIGHT))
        .flex()
        .items_center()
        .child(match lit {
            Some(color) => heading.text_color(color),
            None => heading,
        })
        .into_any_element()
}

fn need_row(
    index: usize,
    project_name: &str,
    title: &str,
    word: &'static str,
    text: String,
    cx: &mut Context<Deck>,
) -> AnyElement {
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
        .hover(|style| style.bg(Tokens::raised_hover()))
        .on_click(cx.listener(move |this, _, _, cx| this.open_item(index, cx)))
        .child(super::panels::state_mark(super::board::LaneState::Waiting, 9.))
        .child(
            div()
                .flex_none()
                .max_w(px(104.))
                .truncate()
                .text_size(size::scaled(size::SMALL))
                .text_color(Tokens::text_faint())
                .child(project_name.to_string()),
        )
        .child(div().flex_none().text_size(size::scaled(size::SMALL)).text_color(Tokens::text_faint()).child("›"))
        .child(div().flex_1().min_w_0().truncate().text_size(size::scaled(size::BODY)).child(title.to_string()))
        .child(
            div()
                .flex_none()
                .text_size(size::scaled(size::SMALL))
                .font_weight(FontWeight::MEDIUM)
                .text_color(Tokens::amber())
                .child(word),
        )
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
        .text_size(size::scaled(size::SMALL))
        .text_color(Tokens::text_faint())
        .child("Nothing needs you")
        .into_any_element()
}

fn project_row(project: &Row, shown: bool, cx: &mut Context<Deck>) -> AnyElement {
    let Row::Project { index, name, tasks, attention, selected, open, .. } = project else {
        return div().into_any_element();
    };
    let (index, tasks, attention, selected, open) = (*index, *tasks, *attention, *selected, *open);
    let fg = if shown { Tokens::text() } else { Tokens::text_dim() };
    let chevron = if open { icon::CHEVRON_DOWN } else { icon::CHEVRON_RIGHT };
    let row = div()
        .relative()
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
        .when(shown, |s| s.bg(Tokens::raised()))
        .hover(|style| style.bg(Tokens::raised_hover()))
        .children(shown.then(|| {
            div()
                .debug_selector(|| "sidebar-project-marker".into())
                .absolute()
                .left(px(0.))
                .top(px(12.))
                .w(px(3.))
                .h(px(16.))
                .rounded(px(radius::MARK))
                .bg(Tokens::accent())
        }))
        .on_click(cx.listener(move |this, _, _, cx| this.select_project(index, cx)))
        .child(
            div()
                .id(("sidebar-project-chevron", index))
                .debug_selector(move || format!("sidebar-project-chevron-{index}"))
                .flex_none()
                .w(px(CHEVRON_W))
                .flex()
                .justify_center()
                .text_color(Tokens::text_faint())
                .hover(|style| style.text_color(Tokens::text_dim()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    if selected {
                        this.rail.toggle_collapsed();
                        cx.notify();
                    } else {
                        this.select_project(index, cx);
                    }
                }))
                .child(super::panels::glyph(chevron, 10.)),
        )
        .child(super::sidebar_narrow::tile(initial(name), Tokens::hue(name), shown, TILE))
        .child(div().flex_1().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(name.to_string()))
        .children(attention.then(|| div().flex_none().size(px(6.)).rounded_full().bg(Tokens::amber())))
        .child(
            div()
                .flex_none()
                .min_w(px(18.))
                .h(px(16.))
                .px(px(space::DETAIL))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(Tokens::raised())
                .font_family(super::theme::MONO)
                .text_size(size::scaled(size::MICRO))
                .text_color(Tokens::text_dim())
                .child(tasks.to_string()),
        );
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
            .border_color(Tokens::edge_strong())
            .bg(Tokens::float())
            .text_size(size::scaled(size::SMALL))
            .text_color(Tokens::text())
            .child(self.text.clone())
    }
}
