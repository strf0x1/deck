//! Renders the publication panel and the chip from [`View`]. The panel takes
//! the conversation pane's body while open, like the access panel.
//!
//! The design choice: no control here changes GitHub. There is no merge,
//! close, ready or push: GitHub owns those (ADR 0007), and marking ready is
//! the agent's `update_pr` or the user's click on GitHub. The one action is
//! destroying the task once its pull request is merged, behind a second click.

use super::access_panel::button;
use super::panels::glyph;
use super::publication::{Dot, View};
use super::root::Deck;
use super::theme::{MONO, Tokens, icon, size};
use gpui::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

pub fn dot_color(dot: Dot) -> Hsla {
    match dot {
        Dot::Pushed | Dot::Closed => Tokens::text_faint().into(),
        Dot::Draft => Tokens::text_dim().into(),
        Dot::ChecksRunning => Tokens::amber(),
        Dot::ChecksFailing => Tokens::red(),
        Dot::Ready => Tokens::lime(),
        Dot::Merged => Tokens::accent(),
    }
}

/// A pill with one state dot and `#7` (or the branch, cut to fit: a card's
/// title must stay readable beside it).
pub fn chip(dot: Dot, label: &str) -> gpui::Div {
    div()
        .flex_none()
        .max_w(px(96.))
        .min_w_0()
        .h(px(20.))
        .px(px(7.))
        .flex()
        .items_center()
        .gap(px(5.))
        .rounded_full()
        .bg(Tokens::raised())
        .text_size(px(size::SMALL))
        .text_color(Tokens::text_dim())
        .child(div().flex_none().size(px(6.)).rounded_full().bg(dot_color(dot)))
        .child(div().min_w_0().truncate().font_family(MONO).child(label.to_owned()))
}

fn row(label: &'static str, value: impl Into<String>) -> AnyElement {
    div()
        .flex()
        .gap(px(12.))
        .child(div().w(px(110.)).flex_none().text_color(Tokens::text_faint()).child(label))
        .child(div().min_w_0().truncate().font_family(MONO).text_size(px(size::LABEL)).child(value.into()))
        .into_any_element()
}

pub fn panel(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let Some((task, view)) = deck.open_publication() else { return div().into_any_element() };
    let title = deck.task_title(task);
    let body: AnyElement = match view {
        None => div().text_color(Tokens::text_dim()).child("This task has published nothing yet.").into_any_element(),
        Some(view) => details(&view, deck.destroy_confirm, cx),
    };
    div()
        .id("publication")
        .debug_selector(|| "publication-panel".into())
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .gap(px(14.))
        .px(px(22.))
        .py(px(18.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(size::TITLE)).child("Publication"))
                .child(div().text_color(Tokens::text_faint()).child(title))
                .child(div().flex_1())
                .child(
                    button("publication-close", "Close", Tokens::text_dim())
                        .on_click(cx.listener(|this, _, _, cx| this.close_publication(cx))),
                ),
        )
        .child(body)
        .into_any_element()
}

fn details(view: &View, confirm: bool, cx: &mut Context<Deck>) -> AnyElement {
    let checks: Vec<AnyElement> = view
        .checks
        .iter()
        .map(|check| {
            let color: Hsla = if check.failed {
                Tokens::red()
            } else if check.verdict == "success" {
                Tokens::lime()
            } else {
                Tokens::text_faint().into()
            };
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(div().size(px(6.)).rounded_full().bg(color))
                .child(div().flex_1().min_w_0().truncate().child(check.name.clone()))
                .child(div().text_color(Tokens::text_dim()).child(check.verdict.clone()))
                .into_any_element()
        })
        .collect();
    let url = view.url.clone();
    div()
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(chip(view.dot, &view.label))
                .child(div().text_color(Tokens::text_dim()).child(view.dot.label())),
        )
        .children(view.title.clone().map(|t| row("pull request", format!("{} {t}", view.label))))
        .children(view.branch.clone().map(|b| row("branch", b)))
        .children(view.commit.clone().map(|c| row("last commit", c)))
        .children(
            (!checks.is_empty())
                .then(|| div().flex().flex_col().gap(px(6.)).pt(px(4.)).children(checks).into_any_element()),
        )
        .children(view.note.clone().map(|n| div().text_size(px(size::SMALL)).text_color(Tokens::text_faint()).child(n)))
        .child(
            div()
                .flex()
                .gap(px(8.))
                .pt(px(4.))
                .children(url.map(|url| {
                    button("publication-open", "Open on GitHub", Tokens::text())
                        .child(glyph(icon::SEND, 10.))
                        .gap(px(6.))
                        .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url)))
                }))
                .children(view.done().then(|| {
                    let label = if confirm { "Destroy: confirm" } else { "Destroy task" };
                    button("publication-destroy", label, Tokens::red())
                        .debug_selector(|| "publication-destroy".into())
                        .on_click(cx.listener(|this, _, _, cx| this.destroy_published(cx)))
                })),
        )
        .children(confirm.then(|| {
            div()
                .text_size(px(size::SMALL))
                .text_color(Tokens::amber())
                .child("Destroying deletes the task's VM and disk. The pull request stays on GitHub.")
        }))
        .into_any_element()
}
