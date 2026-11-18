//! The window's header across the top of the transcript. On the left it names
//! the selected task, its project and the agent's status; on the right it
//! holds the publication and error chips and the window's actions.
//!
//! The design choice: the header is one row that stays put; the sidebar is the
//! only navigation, so the header carries no project or session controls.

use super::glyph::glyph;
use super::header_title::HeaderTitle;
use super::panels::{primary, state_color};
use super::root::Deck;
use super::theme::{MONO, Tokens, icon, radius, size, space, tint};
use gpui::{
    AnyElement, Context, Div, Font, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
    StatefulInteractiveElement as _, Styled as _, TextRun, Window, div, prelude::FluentBuilder as _, px,
};

/// The title block's widest run, so a long task title leaves room for the
/// header's right-hand controls.
const TITLE_MAX: f32 = 420.;
/// The project view's checkout line under its name, at most.
const PLACE_MAX: f32 = 280.;

pub fn header(deck: &Deck, window: &Window, cx: &mut Context<Deck>) -> impl IntoElement {
    let live_lane = deck.live_lane();
    let live = deck.board.cards().iter().find(|c| c.lane.id == live_lane).map(|c| c.lane.state);
    let dot = live.map_or(Tokens::text_faint().into(), state_color);
    // The project view carries its coordinator's status where a task's agent
    // sits, and never a publication (ADR 0011 §9).
    let project_view = deck.showing_project();
    let row = div()
        .min_h(px(40.))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(space::STACK))
        .children(title_block(deck, window, cx))
        .child(div().flex_1());
    let row = if project_view {
        row.children(super::coordinator_header::status_chip(deck))
    } else {
        row.child(agent_pill(deck, dot))
    };
    row.children(deck.publication_of(live_lane).filter(|_| !project_view).map(|view| {
        super::publication_panel::chip(view.dot, &format!("{} · {}", view.label, view.dot.label()))
            .max_w(px(240.))
            .min_h(px(30.))
            .px(px(11.))
            .id("hdr-publication")
            .debug_selector(|| "hdr-publication".into())
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_publication(live_lane, cx)))
    }))
    .children(deck.publish_notice().map(|notice| {
        notice_chip(icon::BELL, Tokens::amber(), notice.to_string(), 320.)
            .debug_selector(|| "hdr-publish-notice".into())
    }))
    .children(deck.new_session_state().error().map(|e| {
        notice_chip(icon::WARN, Tokens::red(), format!("New session failed: {e}"), 320.)
            .debug_selector(|| "hdr-new-error".into())
    }))
    .children(deck.mark_error().map(|e| {
        notice_chip(icon::WARN, Tokens::red(), format!("Mark done failed: {e}"), 320.)
            .debug_selector(|| "hdr-mark-error".into())
    }))
    .children(deck.projects_error().map(|e| {
        notice_chip(icon::WARN, Tokens::red(), e.to_string(), 420.).debug_selector(|| "hdr-project-error".into())
    }))
    .child(icon_button("hdr-bell", icon::BELL))
    .child(
        icon_button("hdr-access", icon::SHIELD)
            .debug_selector(|| "hdr-access".into())
            .when(deck.access.is_open(), |d| d.bg(Tokens::raised_hover()).text_color(Tokens::accent()))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_access(cx))),
    )
    .children(deck.can_stop().then(|| {
        div()
            .id("hdr-stop")
            .min_h(px(32.))
            .px(px(space::INSET))
            .flex()
            .items_center()
            .gap(px(space::LINE))
            .rounded(px(radius::CONTROL))
            .border_1()
            .border_color(Tokens::edge())
            .text_size(size::scaled(size::BODY))
            .text_color(Tokens::text_dim())
            .debug_selector(|| "hdr-stop".into())
            .cursor_pointer()
            .hover(|s| s.border_color(tint(Tokens::red(), 0.5)).bg(Tokens::well()).text_color(Tokens::text()))
            .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
            .child(glyph(icon::STOP, 11.).text_color(Tokens::red()))
            .child("Stop")
            .child(kbd(crate::app::STOP_HINT))
    }))
    .children(project_view.then(|| super::coordinator_header::actions(deck, cx)).into_iter().flatten())
    .children(deck.settings().session_model().filter(|_| !deck.settings().choices().is_empty()).map(|model| {
        // Deck's models reach only VM tasks, through their gateway; a local
        // Construct runs on its own config whatever is picked here (#202).
        let local = deck.next_backend() == "local";
        div()
            .id("hdr-model")
            .debug_selector(|| "hdr-model".into())
            .max_w(px(240.))
            .min_h(px(32.))
            .px(px(space::INSET))
            .flex()
            .items_center()
            .gap(px(space::LINE))
            .rounded(px(radius::CONTROL))
            .border_1()
            .border_color(Tokens::edge())
            .when(deck.settings().picking(), |d| d.border_color(tint(Tokens::cyan(), 0.5)))
            .text_size(size::scaled(size::SMALL))
            .font_family(super::theme::MONO)
            .text_color(Tokens::text_dim())
            .cursor_pointer()
            .hover(|s| s.text_color(Tokens::text()).bg(Tokens::raised()))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_model_picker(cx)))
            .child(glyph(icon::BOLT, 12.).text_color(if local { Tokens::text_faint().into() } else { Tokens::cyan() }))
            .child(if local {
                div().debug_selector(|| "hdr-model-local".into()).whitespace_nowrap().child(super::model_menu::LOCAL)
            } else {
                div().overflow_hidden().whitespace_nowrap().child(model)
            })
            .child(glyph(icon::CHEVRON_DOWN, 10.))
    }))
    .child(
        primary()
            .id("hdr-new")
            .debug_selector(|| "hdr-new".into())
            .cursor_pointer()
            .hover(|s| s.opacity(0.9))
            .on_click(cx.listener(|this, _, _, cx| this.new_session(cx)))
            .child(glyph(icon::PLUS, 13.))
            .child(deck.new_session_state().label()),
    )
}

/// The selected task's agent command in a pill wearing the task's state.
fn agent_pill(deck: &Deck, dot: Hsla) -> Div {
    div()
        .min_h(px(30.))
        .pl(px(space::DETAIL))
        .pr(px(space::INSET))
        .flex()
        .items_center()
        .gap(px(space::LINE))
        .rounded_full()
        .border_1()
        .border_color(tint(dot, 0.3))
        .bg(super::theme::Tokens::well())
        .text_color(Tokens::text_dim())
        .child(
            div()
                .size(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(tint(dot, 0.18))
                .text_color(dot)
                .child(glyph(icon::AGENTS, 12.)),
        )
        .child(div().font_family(MONO).text_size(size::scaled(size::SMALL)).child(agent_label(&deck.agent_cmd)))
}

/// A header notice: one line in the colour of what it reports.
fn notice_chip(g: &'static str, color: Hsla, text: String, max: f32) -> Div {
    div()
        .max_w(px(max))
        .min_h(px(30.))
        .px(px(11.))
        .flex()
        .items_center()
        .gap(px(space::LINE))
        .overflow_hidden()
        .rounded_full()
        .border_1()
        .border_color(tint(color, 0.3))
        .bg(super::theme::Tokens::well())
        .text_size(size::scaled(size::SMALL))
        .text_color(color)
        .child(glyph(g, 12.))
        .child(div().min_w_0().truncate().child(text))
}

/// The selected task's title, its project's faint chip and its status line;
/// the project alone when it has no task yet (ADR 0010 §1).
fn title_block(deck: &Deck, window: &Window, cx: &mut Context<Deck>) -> Option<AnyElement> {
    let HeaderTitle { title, project, detail, place, provenance } = deck.header_title();
    if let Some(title) = title {
        let width = title_width(&title, deck.typography.interface.family(), window);
        let chip = project.map(|project| {
            let hue = Tokens::hue(&project);
            div()
                .id("hdr-project")
                .debug_selector(|| "hdr-project".into())
                .flex_none()
                .min_h(px(18.))
                .px(px(space::TIGHT))
                .flex()
                .items_center()
                .rounded_full()
                .border_1()
                .border_color(tint(hue, 0.3))
                .bg(super::theme::Tokens::well())
                .text_size(size::scaled(size::MICRO))
                .text_color(tint(hue, 0.85))
                .cursor_pointer()
                .hover(move |style| style.text_color(hue).bg(Tokens::raised_hover()))
                .on_click(cx.listener(|this, _, _, cx| this.show_project(cx)))
                .child(project)
        });
        return Some(
            div()
                .flex_shrink()
                .min_w_0()
                .max_w(px(TITLE_MAX))
                .flex()
                .flex_col()
                .gap(px(space::HAIR))
                .child(
                    div()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(px(space::LINE))
                        .child(
                            div()
                                .debug_selector(|| "hdr-title".into())
                                .w(width)
                                .flex_shrink()
                                .min_w_0()
                                .truncate()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_size(size::scaled(size::TITLE))
                                .line_height(size::scaled(17.))
                                .child(title),
                        )
                        .children(chip),
                )
                .children(detail.map(|detail| {
                    let detail = match &provenance {
                        Some(provenance) => format!("{detail} · {provenance}"),
                        None => detail,
                    };
                    div()
                        .debug_selector(|| "hdr-detail".into())
                        .min_w_0()
                        .truncate()
                        .line_height(size::scaled(14.))
                        .text_size(size::scaled(size::SMALL))
                        .text_color(Tokens::text_faint())
                        .child(detail)
                }))
                .into_any_element(),
        );
    }
    project.map(|project| {
        div()
            .flex_shrink()
            .min_w_0()
            .max_w(px(TITLE_MAX))
            .flex()
            .flex_col()
            .gap(px(space::HAIR))
            .child(
                div()
                    .debug_selector(|| "hdr-title".into())
                    .w(title_width(&project, deck.typography.interface.family(), window))
                    .flex_shrink()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(size::scaled(size::TITLE))
                    .line_height(size::scaled(17.))
                    .child(project),
            )
            .children(place.map(|place| {
                // Measured like the title, so it is not cut to the name's
                // width, and capped, so a deep path leaves the controls room.
                div()
                    .debug_selector(|| "hdr-checkout".into())
                    .w(title_width(&place, MONO, window).min(px(PLACE_MAX)))
                    .flex_shrink()
                    .min_w_0()
                    .truncate()
                    .line_height(size::scaled(14.))
                    .font_family(MONO)
                    .text_size(size::scaled(size::SMALL))
                    .text_color(Tokens::text_faint())
                    .child(place)
            }))
            .into_any_element()
    })
}

/// The width the header title needs, capped at [`TITLE_MAX`]. A truncating
/// text measures as its ellipsis inside a flex row, so the title's own width
/// is measured here and the project chip can follow the text.
fn title_width(text: &str, family: &str, window: &Window) -> Pixels {
    let font = Font { family: family.to_owned().into(), weight: FontWeight::SEMIBOLD, ..Font::default() };
    let run = TextRun { len: text.len(), font, ..TextRun::default() };
    let width = window
        .text_system()
        .layout_line(text, size::scaled(size::TITLE).to_pixels(window.rem_size()), &[run], None)
        .width;
    (width + px(1.)).min(px(TITLE_MAX))
}

pub fn icon_button(id: &'static str, g: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(32.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::CONTROL))
        .text_color(Tokens::text_dim())
        .cursor_pointer()
        .hover(|s| s.bg(Tokens::raised_hover()).text_color(Tokens::text()))
        .child(glyph(g, 15.))
}

pub fn kbd(keys: &'static str) -> impl IntoElement {
    div()
        .px(px(space::TIGHT))
        .py(px(1.))
        .rounded(px(radius::CHIP))
        .border_1()
        .border_color(Tokens::edge())
        .bg(Tokens::sunken())
        .font_family(MONO)
        .text_size(size::scaled(size::MICRO))
        .text_color(Tokens::text_dim())
        .child(keys)
}

/// The agent command without leading `env` assignments or directories.
pub(super) fn agent_label(cmd: &str) -> String {
    let words: Vec<&str> = cmd.split_whitespace().skip_while(|w| *w == "env" || w.contains('=')).collect();
    let Some((prog, args)) = words.split_first() else { return String::from("no agent") };
    let prog = prog.rsplit('/').next().unwrap_or(prog);
    std::iter::once(prog).chain(args.iter().copied()).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    #[test]
    fn agent_label_drops_env_and_paths() {
        assert_eq!(super::agent_label("env A=1 B=2 /x/y/construct acp"), "construct acp");
        assert_eq!(super::agent_label("construct acp"), "construct acp");
        assert_eq!(super::agent_label(""), "no agent");
    }
}
