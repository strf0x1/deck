//! Worktree controls in the project and task views, with a review pane
//! separate from the worker's conversation.

use super::access_panel::button;
use super::glyph::glyph;
use super::root::Deck;
use super::theme::{MONO, Tokens, icon, radius, size, space, tint};
use deck_runtime::worktree::{WorktreeState, WorktreeStatus};
use gpui::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

fn status(state: WorktreeState) -> (&'static str, Hsla) {
    match state {
        WorktreeState::Preparing => ("Preparing worktree", Tokens::text_faint().into()),
        WorktreeState::InProgress => ("Work in progress", Tokens::cyan()),
        WorktreeState::Uncommitted => ("Uncommitted changes", Tokens::amber()),
        WorktreeState::Ready => ("Ready for review", Tokens::lime()),
        WorktreeState::Merged => ("Merged locally", Tokens::violet()),
        WorktreeState::Unchanged => ("No changes", Tokens::text_faint().into()),
        WorktreeState::Unavailable => ("Worktree unavailable", Tokens::red()),
        WorktreeState::Removed => ("Worktree removed", Tokens::text_faint().into()),
    }
}

/// An entry point tied to a task record, so it works even without an attachment.
pub(super) fn task_button(deck: &Deck, id: &str, selector: String, cx: &mut Context<Deck>) -> Option<AnyElement> {
    deck.tasks.iter().find(|t| t.id == id)?.workspace.as_ref()?;
    let id = id.to_string();
    let (label, color) =
        deck.worktree_status(&id).map_or(("Worktree", Tokens::text_dim().into()), |fact| status(fact.state));
    Some(
        div()
            .id(gpui::SharedString::from(format!("worktree-{id}")))
            .debug_selector(move || selector.clone())
            .max_w(px(190.))
            .h(px(23.))
            .px(px(space::LINE))
            .flex()
            .items_center()
            .gap(px(space::TIGHT))
            .rounded_full()
            .border_1()
            .border_color(tint(color, 0.35))
            .bg(Tokens::well())
            .text_color(color)
            .text_size(size::scaled(size::SMALL))
            .cursor_pointer()
            .hover(|style| style.bg(Tokens::raised_hover()))
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |deck, _, _, cx| {
                cx.stop_propagation();
                deck.open_worktree(&id, cx);
            }))
            .child(glyph(icon::BRANCH, 11.).flex_none())
            .child(div().min_w_0().truncate().child(label))
            .into_any_element(),
    )
}

pub(super) fn source(deck: &Deck, cx: &mut Context<Deck>) -> Option<AnyElement> {
    if !deck.local_worker_source() {
        return None;
    }
    let project = deck.projects.selected()?;
    let state = deck.worktree_source();
    let body = match state {
        Some(Ok(Some(source))) => {
            let label = format!("{} · {}", source.branch.as_deref().unwrap_or("detached HEAD"), short(&source.commit),);
            div()
                .flex()
                .flex_col()
                .gap(px(space::HAIR))
                .child(mono(label))
                .child(mono(super::prompt::tilde(&source.path)))
                .children(source.dirty.then(|| {
                    div()
                        .text_color(Tokens::amber())
                        .child("Workers start from committed files. Local edits and untracked files stay here.")
                }))
        }
        Some(Ok(None)) => return None,
        Some(Err(error)) => div().text_color(Tokens::amber()).child(error.clone()),
        None if project.id.starts_with("git:") => div().text_color(Tokens::text_faint()).child("Reading checkout…"),
        None => return None,
    };
    Some(
        div()
            .id("worker-source")
            .debug_selector(|| "worker-source".into())
            .mx(px(space::PAD))
            .mt(px(space::PAD))
            .px(px(space::INSET))
            .py(px(space::LINE))
            .flex_none()
            .flex()
            .items_start()
            .gap(px(space::STACK))
            .rounded(px(radius::CONTROL))
            .border_1()
            .border_color(Tokens::edge())
            .bg(Tokens::well())
            .text_size(size::scaled(size::LABEL))
            .child(glyph(icon::BRANCH, 13.).mt(px(space::HAIR)).text_color(Tokens::text_faint()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(space::HAIR))
                    .child(div().font_weight(FontWeight::MEDIUM).child("Worker source"))
                    .child(body),
            )
            .children(matches!(state, Some(Err(_))).then(|| {
                button("worktree-source-pick", "Choose source…", Tokens::cyan())
                    .debug_selector(|| "worktree-source-pick".into())
                    .flex_none()
                    .on_click(cx.listener(|deck, _, _, cx| deck.pick_worktree_source(cx)))
            }))
            .into_any_element(),
    )
}

pub(super) fn pane(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let Some(pane) = deck.worktree_pane() else { return div().into_any_element() };
    let fact = deck.worktree_status(&pane.id);
    let title = deck.task_title(&pane.id);
    let head = div()
        .flex()
        .items_center()
        .gap(px(space::STACK))
        .child(super::panels::icon_mark(icon::BRANCH, Tokens::violet(), 26.))
        .child(div().font_weight(FontWeight::SEMIBOLD).text_size(size::scaled(size::TITLE)).child("Worktree"))
        .child(div().min_w_0().truncate().text_color(Tokens::text_faint()).child(title))
        .child(div().flex_1())
        .child(control("worktree-close", "Close", Tokens::text_dim(), cx, Deck::close_worktree));
    let body = match fact {
        Some(fact) => details(deck, pane, fact, cx),
        None => div().text_color(Tokens::text_faint()).child("Reading worktree…").into_any_element(),
    };
    div()
        .id("worktree-pane")
        .debug_selector(|| "worktree-pane".into())
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(space::STACK))
        .px(px(space::PANE))
        .py(px(space::PANE_V))
        .child(head)
        .children(pane.error.as_deref().map(super::access_panel::error))
        .child(body)
        .into_any_element()
}

fn details(deck: &Deck, pane: &super::worktrees::Pane, fact: &WorktreeStatus, cx: &mut Context<Deck>) -> AnyElement {
    let (label, color) = status(fact.state);
    let exists = !matches!(fact.state, WorktreeState::Preparing | WorktreeState::Unavailable | WorktreeState::Removed);
    let path = fact.path.clone();
    let branch = fact.branch.clone();
    let copied = deck.copied.as_deref() == Some("worktree-copy");
    let buttons = div()
        .flex()
        .flex_wrap()
        .gap(px(space::LINE))
        .children((!matches!(fact.state, WorktreeState::Preparing | WorktreeState::Removed)).then(|| {
            button("worktree-open", "Open worktree", Tokens::text_dim())
                .debug_selector(|| "worktree-open".into())
                .on_click(cx.listener(move |_, _, _, cx| cx.open_with_system(&path)))
        }))
        .child(
            button("worktree-copy", if copied { "Copied" } else { "Copy branch" }, Tokens::text_dim())
                .debug_selector(|| "worktree-copy".into())
                .on_click(cx.listener(move |deck, _, _, cx| deck.copy("worktree-copy", &branch, cx))),
        )
        .children(
            ((exists || fact.state == WorktreeState::Removed) && !pane.reading && !pane.removing)
                .then(|| control("worktree-review", "Review changes", Tokens::cyan(), cx, Deck::review_worktree)),
        )
        .children((fact.removable && !pane.confirm && !pane.removing).then(|| {
            button("worktree-remove", "Remove worktree…", Tokens::red())
                .debug_selector(|| "worktree-remove".into())
                .on_click(cx.listener(|deck, _, _, cx| deck.confirm_worktree_removal(true, cx)))
        }));
    div()
        .flex()
        .flex_col()
        .gap(px(space::STACK))
        .child(super::panels::pill(color, label).self_start())
        .child(row("Branch", fact.branch.clone()))
        .child(row("Starting commit", fact.start.clone()))
        .children(fact.target.as_ref().map(|target| row("Target", target.clone())))
        .children(fact.tip.as_ref().filter(|tip| *tip != &fact.start).map(|tip| row("Tip", tip.clone())))
        .child(row("Checkout", super::prompt::tilde(&fact.path)))
        .children(fact.reason.as_ref().map(|reason| div().text_color(Tokens::text_dim()).child(reason.clone())))
        .child(buttons)
        .children(pane.confirm.then(|| confirm(fact, pane.removing, cx)))
        .children(pane.reading.then(|| div().text_color(Tokens::text_faint()).child("Reading changes…")))
        .children(pane.diff.as_ref().map(|diff| match diff {
            Ok(diff) => patch(diff),
            Err(error) => super::access_panel::error(error),
        }))
        .into_any_element()
}

fn confirm(fact: &WorktreeStatus, removing: bool, cx: &mut Context<Deck>) -> AnyElement {
    div()
        .id("worktree-confirm")
        .debug_selector(|| "worktree-confirm".into())
        .p(px(space::INSET))
        .flex()
        .flex_col()
        .gap(px(space::LINE))
        .rounded(px(radius::CONTROL))
        .border_1()
        .border_color(tint(Tokens::red(), 0.4))
        .bg(Tokens::well())
        .child(format!("Remove this checkout and keep {}?", fact.branch))
        .child(if removing {
            div().text_color(Tokens::text_faint()).child("Removing worktree…")
        } else {
            div()
                .flex()
                .gap(px(space::LINE))
                .child(control(
                    "worktree-confirm-remove",
                    "Keep branch and remove",
                    Tokens::red(),
                    cx,
                    Deck::remove_worktree,
                ))
                .child(
                    button("worktree-cancel-remove", "Cancel", Tokens::text_dim())
                        .on_click(cx.listener(|deck, _, _, cx| deck.confirm_worktree_removal(false, cx))),
                )
        })
        .into_any_element()
}

fn patch(diff: &deck_runtime::worktree::WorktreeDiff) -> AnyElement {
    let rows = diff.text.lines().take(800).map(|line| {
        let fg = if line.starts_with("@@") {
            Tokens::cyan()
        } else if line.starts_with('+') {
            Tokens::lime()
        } else if line.starts_with('-') {
            Tokens::red()
        } else {
            Tokens::text_dim().into()
        };
        mono(line.to_owned()).text_color(fg)
    });
    div()
        .id("worktree-diff")
        .debug_selector(|| "worktree-diff".into())
        .p(px(space::INSET))
        .rounded(px(radius::CONTROL))
        .border_1()
        .border_color(Tokens::edge())
        .bg(Tokens::sunken())
        .child(
            div().pb(px(space::LINE)).font_weight(FontWeight::MEDIUM).child("Tracked changes from the starting commit"),
        )
        .children(diff.text.is_empty().then(|| div().text_color(Tokens::text_faint()).child("No tracked changes.")))
        .children(rows)
        .children((diff.truncated || diff.text.lines().nth(800).is_some()).then(|| {
            div().pt(px(space::LINE)).text_color(Tokens::amber()).child("Open the worktree to see the full diff.")
        }))
        .into_any_element()
}

fn row(label: &'static str, text: String) -> gpui::Div {
    div()
        .flex()
        .gap(px(space::LINE))
        .child(div().w(px(122.)).flex_none().text_color(Tokens::text_faint()).child(label))
        .child(mono(text))
}

fn mono(text: String) -> gpui::Div {
    div().min_w_0().font_family(MONO).text_size(size::scaled(size::LABEL)).child(text)
}

fn short(commit: &str) -> String {
    commit.chars().take(12).collect()
}

fn control(
    id: &'static str,
    label: &str,
    color: impl Into<Hsla>,
    cx: &mut Context<Deck>,
    action: fn(&mut Deck, &mut Context<Deck>),
) -> gpui::Stateful<gpui::Div> {
    button(id, label, color)
        .debug_selector(move || id.into())
        .on_click(cx.listener(move |deck, _, _, cx| action(deck, cx)))
}
