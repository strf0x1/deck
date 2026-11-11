//! The conversation pane: the live session's title bar, transcript, the
//! permission question when one is open, and the prompt field.
//!
//! The design choice: the pane's state is said in colour before words. Its
//! top edge and status pill take the session's colour, a decision card is
//! the one amber surface, and its answers are green to allow and red to
//! deny, so a glance finds what needs attention and what each key will do.

use super::chrome::kbd;
use super::panels::{glyph, icon_tile, pane, pill, primary};
use super::root::Deck;
use super::session::Status;
use super::theme::{MONO, Tokens, glow, icon, radius, size, space, tint};
use deck_runtime::approvals::Decision;
use gpui::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

pub fn main(deck: &Deck, cx: &mut Context<Deck>) -> impl IntoElement {
    let s = deck.session();
    let has_task = deck.task_id().is_some();
    // A launched window with no selected task is an open project (or no
    // project); a headless deck still shows the idle session's transcript.
    let empty_project = !has_task && deck.is_launched();
    let (status, color): (&str, Hsla) = match &s.status {
        _ if s.ask.is_some() => ("Needs you", Tokens::amber()),
        Status::Connecting => ("Connecting", Tokens::text_dim().into()),
        Status::Ready => ("Ready", Tokens::lime()),
        Status::Waiting => ("Waiting", Tokens::amber()),
        Status::Working => ("Working", Tokens::cyan()),
        Status::Detached => ("Running, detached", Tokens::cyan()),
        Status::Down(_) => ("Down", Tokens::red()),
    };
    // The pane's top edge takes the session's colour, so its state reads
    // before any word on it does.
    let edge = if has_task { color } else { Tokens::accent() };
    let (tile, tile_color) = if empty_project {
        (icon::PROJECT, deck.project_name().map_or(Tokens::text_dim().into(), Tokens::hue))
    } else {
        (icon::AGENTS, color)
    };
    let title = if empty_project { deck.project_name().unwrap_or("No project").to_string() } else { s.agent.clone() };
    let subtitle = if empty_project { "project" } else { "live session" };
    let title_selector = if empty_project { "empty-project" } else { "conversation-agent" };
    pane()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(
            // A 1px state edge, drawn as the overlay's own top border so it
            // follows the pane's rounded corners instead of cutting across them.
            div().absolute().size_full().rounded(px(radius::PANE)).border_t_1().border_color(tint(edge, 0.6)),
        )
        .child(
            div()
                .h(px(54.))
                .px(px(18.))
                .flex()
                .items_center()
                .gap(px(space::STACK))
                .border_b_1()
                .border_color(Tokens::edge())
                .child(icon_tile(tile, tile_color, 26.))
                .child(
                    div()
                        .debug_selector(move || title_selector.into())
                        .text_size(px(size::TITLE))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(div().text_size(px(size::LABEL)).text_color(Tokens::text_faint()).child(subtitle))
                .child(div().flex_1())
                .children(has_task.then(|| pill(color, status))),
        )
        .child(if deck.settings.is_open() {
            super::settings_panel::panel(deck, cx)
        } else if deck.access.is_open() {
            super::access_panel::panel(deck, cx).into_any_element()
        } else if deck.open_publication().is_some() {
            super::publication_panel::panel(deck, cx).into_any_element()
        } else if deck.project().is_some() && !has_task {
            super::project_view::project_view(deck, cx)
        } else if empty_project {
            empty(cx)
        } else {
            super::transcript_list::transcript(deck, cx).into_any_element()
        })
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(space::STACK))
                .p(px(space::PAD))
                .pt(px(space::DETAIL))
                .children(deck.boundary().pending().map(|_| boundary(deck, cx)))
                .children(s.ask.as_ref().map(|_| ask(deck, cx)))
                // Access takes the keyboard, so the field it would block is not shown.
                .children((!deck.access.is_open() && !deck.settings.is_open()).then(|| {
                    if deck.can_restart() {
                        restart(deck, cx)
                    } else {
                        super::prompt::prompt(deck, cx).into_any_element()
                    }
                })),
        )
}

/// No project open: what the window says before there is anywhere to start
/// a task (ADR 0010 §1). A selected project shows its project view instead.
fn empty(cx: &mut Context<Deck>) -> AnyElement {
    div()
        .flex_1()
        .min_h_0()
        .debug_selector(|| "project-empty-state".into())
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(space::STACK))
        .px(px(space::PAD))
        .child(
            div()
                .mb(px(space::LINE))
                .size(px(64.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(radius::PANE))
                .border_1()
                .border_color(tint(Tokens::accent(), 0.35))
                .bg(Tokens::sweep(0.14))
                .shadow(glow(Tokens::accent(), 0.5))
                .text_color(Tokens::accent())
                .child(glyph(icon::CUBE, 26.)),
        )
        .child(div().text_size(px(size::DISPLAY)).font_weight(FontWeight::SEMIBOLD).child("No project open"))
        .child(
            div()
                .max_w(px(420.))
                .text_center()
                .text_size(px(size::LABEL))
                .text_color(Tokens::text_dim())
                .child("Open a checkout to start a task in its project."),
        )
        .child(
            primary()
                .mt(px(space::TIGHT))
                .id("empty-open")
                .debug_selector(|| "empty-open".into())
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| this.pick_project(cx)))
                .child(glyph(icon::PLUS, 11.))
                .child("Open project…"),
        )
        .into_any_element()
}

/// The host boundary card: an external operation that needs a human
/// decision. It is not an ACP permission; the daemon holds the request and
/// the agent learns the outcome through its own gateway error.
fn boundary(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let Some(item) = deck.boundary().pending() else { return div().into_any_element() };
    let options = [
        (Decision::Once, "Allow once"),
        (Decision::Timed, "Allow for 5 minutes"),
        (Decision::Task, "Allow for this task"),
        (Decision::Project, "Always for this project"),
        (Decision::Deny, "Deny"),
    ];
    let buttons = options.iter().enumerate().map(|(i, (decision, name))| {
        let weight = if i == 0 {
            Weight::Allow
        } else if *decision == Decision::Deny {
            Weight::Deny
        } else {
            Weight::Other
        };
        let decision = *decision;
        option(("boundary-option", i), name, i, weight)
            .on_click(cx.listener(move |this, _, _, cx| this.decide_boundary(decision, cx)))
    });
    let detail = format!(
        "{}{}",
        item.details.method.as_deref().unwrap_or(""),
        item.details.path.as_deref().map(|path| format!(" {path}")).unwrap_or_default()
    );
    div()
        .flex()
        .flex_col()
        .gap(px(space::SECTION))
        .p(px(space::PAD))
        .rounded(px(radius::BUBBLE))
        .border_1()
        .border_color(tint(Tokens::amber(), 0.5))
        .bg(tint(Tokens::amber(), 0.07))
        .shadow(glow(Tokens::amber(), 0.2))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(space::STACK))
                .child(icon_tile(icon::GLOBE, Tokens::amber(), 24.))
                .child(div().font_weight(FontWeight::SEMIBOLD).child("External access needs you"))
                .child(div().flex_1())
                .child(kbd("1–5")),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(space::LINE))
                .px(px(10.))
                .py(px(7.))
                .rounded(px(radius::CONTROL))
                .border_1()
                .border_color(Tokens::edge())
                .bg(Tokens::sunken())
                .font_family(MONO)
                .text_size(px(size::LABEL))
                .child(format!("{} · {}", item.gateway, item.destination))
                .children(item.credential.as_ref().map(|credential| {
                    div().text_color(Tokens::text_faint()).child(format!("credential {credential}"))
                })),
        )
        .children(item.details.summary.as_ref().map(|summary| {
            div()
                .debug_selector(|| "boundary-summary".into())
                .flex()
                .flex_col()
                .gap(px(space::HAIR))
                .child(
                    div().text_size(px(size::MICRO)).text_color(Tokens::text_faint()).child("the agent's description"),
                )
                .child(div().text_size(px(size::LABEL)).text_color(Tokens::text_dim()).child(format!(
                    "{} · {} commit{}",
                    summary.subject,
                    summary.count,
                    if summary.count == 1 { "" } else { "s" }
                )))
        }))
        .children(
            (!detail.trim().is_empty()).then(|| {
                div().text_size(px(size::SMALL)).text_color(Tokens::text_faint()).font_family(MONO).child(detail)
            }),
        )
        .children(
            item.reason
                .as_ref()
                .map(|reason| div().text_size(px(size::LABEL)).text_color(Tokens::text_dim()).child(reason.clone())),
        )
        .child(div().text_size(px(size::SMALL)).text_color(Tokens::text_faint()).child(if item.waiters > 0 {
            "the agent is waiting for this decision"
        } else {
            "the agent has moved on"
        }))
        .child(div().flex().flex_wrap().gap(px(space::LINE)).children(buttons))
        .into_any_element()
}

fn ask(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let Some(ask) = &deck.session().ask else { return div().into_any_element() };
    let first_allow = ask.options.iter().position(|o| o.allows());
    let buttons = ask.options.iter().enumerate().map(|(i, o)| {
        let weight = if Some(i) == first_allow {
            Weight::Allow
        } else if o.allows() {
            Weight::Other
        } else {
            Weight::Deny
        };
        option(("opt", i), &o.name, i, weight).on_click(cx.listener(move |this, _, _, cx| this.choose(Some(i), cx)))
    });
    div()
        .debug_selector(|| "permission-card".into())
        .flex()
        .flex_col()
        .gap(px(space::SECTION))
        .p(px(space::PAD))
        .rounded(px(radius::BUBBLE))
        .border_1()
        .border_color(tint(Tokens::amber(), 0.5))
        .bg(tint(Tokens::amber(), 0.07))
        .shadow(glow(Tokens::amber(), 0.2))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(space::STACK))
                .child(icon_tile(icon::SHIELD, Tokens::amber(), 24.))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(format!("{} wants to run", deck.session().agent)))
                .child(div().flex_1())
                .child(kbd("esc")),
        )
        .child(
            div()
                .px(px(10.))
                .py(px(7.))
                .rounded(px(radius::CONTROL))
                .border_1()
                .border_color(Tokens::edge())
                .bg(Tokens::sunken())
                .font_family(MONO)
                .text_size(px(size::LABEL))
                .child(ask.title.clone()),
        )
        .child(div().flex().gap(px(space::LINE)).children(buttons))
        .into_any_element()
}

fn restart(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let title = match &deck.session().status {
        Status::Detached => "Your task is still running.",
        Status::Down(detail) if detail.as_str() == "stopped" => "This task is stopped.",
        _ => "The task connection needs to be restored.",
    };
    let action = match &deck.session().status {
        Status::Down(detail) if detail.as_str() == "stopped" => "Restart",
        _ => "Stop and restart",
    };
    let note = match &deck.session().status {
        Status::Detached => "Restart to restore the conversation. Any unfinished turn will be interrupted.",
        Status::Down(detail) if detail.as_str() == "stopped" => {
            "Restart to start the agent again with its recorded history."
        }
        _ => "Restart to restore the conversation. Any unfinished turn will be interrupted.",
    };
    div()
        .flex()
        .flex_col()
        .gap(px(space::STACK))
        .p(px(space::PAD))
        .rounded(px(radius::BUBBLE))
        .border_1()
        .border_color(tint(Tokens::cyan(), 0.3))
        .bg(tint(Tokens::cyan(), 0.05))
        .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
        .child(div().text_color(Tokens::text_dim()).child(note))
        .child(
            div()
                .id("restart-task")
                .debug_selector(|| "restart-task".into())
                .flex()
                .items_center()
                .gap(px(space::STACK))
                .cursor_pointer()
                .text_color(Tokens::accent())
                .font_weight(FontWeight::MEDIUM)
                .child(glyph(icon::REFRESH, 11.))
                .child(action)
                .child(kbd(crate::app::RESTART_HINT))
                .on_click(cx.listener(|this, _, _, cx| this.restart(cx))),
        )
        .into_any_element()
}

/// What a decision does, which is what its button's colour says.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Weight {
    /// The first allowing answer: solid green, the one `1` or enter-like key.
    Allow,
    /// A wider or longer allow.
    Other,
    Deny,
}

/// One answer on a decision card, with the digit that picks it.
fn option(id: impl Into<gpui::ElementId>, name: &str, index: usize, weight: Weight) -> gpui::Stateful<gpui::Div> {
    let (bg, fg, edge): (Hsla, Hsla, Hsla) = match weight {
        Weight::Allow => (Tokens::lime(), Tokens::ink().into(), Tokens::lime()),
        Weight::Other => (tint(Tokens::lime(), 0.08), Tokens::lime(), tint(Tokens::lime(), 0.3)),
        Weight::Deny => (tint(Tokens::red(), 0.08), Tokens::red(), tint(Tokens::red(), 0.35)),
    };
    let hover = match weight {
        Weight::Allow => tint(Tokens::lime(), 0.88),
        Weight::Other => tint(Tokens::lime(), 0.16),
        Weight::Deny => tint(Tokens::red(), 0.16),
    };
    div()
        .id(id.into())
        .h(px(30.))
        .px(px(space::INSET))
        .flex()
        .items_center()
        .gap(px(space::LINE))
        .rounded(px(radius::CONTROL))
        .border_1()
        .border_color(edge)
        .bg(bg)
        .text_color(fg)
        .text_size(px(size::LABEL))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .child(name.to_string())
        .child(div().font_family(MONO).text_size(px(size::MICRO)).opacity(0.6).child(format!("{}", index + 1)))
}
