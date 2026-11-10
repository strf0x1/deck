//! The window's header across the top of the transcript. On the left it names
//! the selected task, its project and the agent's status; on the right it
//! holds the publication and error chips and the window's actions.
//!
//! The design choice: the header is one row that stays put; the sidebar is the
//! only navigation, so the header carries no project or session controls.

use super::header_title::HeaderTitle;
use super::panels::{glyph, state_color};
use super::root::Deck;
use super::theme::{MONO, SANS, Tokens, icon, radius, size, space};
use gpui::{
    AnyElement, Context, Font, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
    StatefulInteractiveElement as _, Styled as _, TextRun, Window, div, px,
};

/// The title block's widest run, so a long task title leaves room for the
/// header's right-hand controls.
const TITLE_MAX: f32 = 420.;

pub fn header(deck: &Deck, window: &Window, cx: &mut Context<Deck>) -> impl IntoElement {
    let live = deck.board.cards().iter().find(|c| c.lane.id == super::root::LIVE).map(|c| c.lane.state);
    let dot = live.map_or(Tokens::text_faint().into(), state_color);
    div()
        .h(px(40.))
        .flex()
        .items_center()
        .gap(px(space::STACK))
        .children(title_block(deck, window))
        .child(div().flex_1())
        .child(
            div()
                .h(px(30.))
                .px(px(space::INSET))
                .flex()
                .items_center()
                .gap(px(space::LINE))
                .rounded_full()
                .border_1()
                .border_color(Tokens::edge())
                .text_color(Tokens::text_dim())
                .child(div().size(px(7.)).rounded_full().bg(dot))
                .child(div().font_family(MONO).text_size(px(size::SMALL)).child(agent_label(&deck.agent_cmd))),
        )
        .children(deck.publication_of(super::root::LIVE).map(|view| {
            super::publication_panel::chip(view.dot, &format!("{} · {}", view.label, view.dot.label()))
                .max_w(px(240.))
                .h(px(30.))
                .px(px(11.))
                .border_1()
                .border_color(Tokens::edge())
                .id("hdr-publication")
                .debug_selector(|| "hdr-publication".into())
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| this.toggle_publication(super::root::LIVE, cx)))
        }))
        .children(deck.publish_notice().map(|notice| {
            div()
                .debug_selector(|| "hdr-publish-notice".into())
                .h(px(30.))
                .px(px(11.))
                .flex()
                .items_center()
                .rounded_full()
                .border_1()
                .border_color(Tokens::edge())
                .text_size(px(size::SMALL))
                .text_color(Tokens::amber())
                .child(notice)
        }))
        .children(deck.new_session_state().error().map(|e| {
            div()
                .debug_selector(|| "hdr-new-error".into())
                .max_w(px(320.))
                .h(px(30.))
                .px(px(11.))
                .flex()
                .items_center()
                .overflow_hidden()
                .rounded_full()
                .border_1()
                .border_color(Tokens::edge())
                .text_size(px(size::SMALL))
                .text_color(Tokens::red())
                .child(format!("New session failed: {e}"))
        }))
        .children(deck.mark_error().map(|e| {
            div()
                .debug_selector(|| "hdr-mark-error".into())
                .max_w(px(320.))
                .h(px(30.))
                .px(px(11.))
                .flex()
                .items_center()
                .overflow_hidden()
                .rounded_full()
                .border_1()
                .border_color(Tokens::edge())
                .text_size(px(size::SMALL))
                .text_color(Tokens::red())
                .child(format!("Mark done failed: {e}"))
        }))
        .children(deck.projects_error().map(|e| {
            div()
                .debug_selector(|| "hdr-project-error".into())
                .max_w(px(420.))
                .h(px(30.))
                .px(px(11.))
                .flex()
                .items_center()
                .overflow_hidden()
                .rounded_full()
                .border_1()
                .border_color(Tokens::edge())
                .text_size(px(size::SMALL))
                .text_color(Tokens::red())
                .child(e.to_string())
        }))
        .child(icon_button("hdr-bell", icon::BELL))
        .child(
            icon_button("hdr-access", icon::SHIELD)
                .debug_selector(|| "hdr-access".into())
                .on_click(cx.listener(|this, _, _, cx| this.toggle_access(cx))),
        )
        .children(deck.can_stop().then(|| {
            div()
                .id("hdr-stop")
                .h(px(32.))
                .px(px(space::INSET))
                .flex()
                .items_center()
                .gap(px(space::LINE))
                .rounded(px(radius::CONTROL))
                .border_1()
                .border_color(Tokens::edge())
                .text_size(px(size::BODY))
                .text_color(Tokens::text_dim())
                .debug_selector(|| "hdr-stop".into())
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                .child("Stop")
                .child(kbd(crate::app::STOP_HINT))
        }))
        .child(
            div()
                .id("hdr-new")
                .h(px(32.))
                .px(px(space::PAD))
                .flex()
                .items_center()
                .gap(px(space::LINE))
                .rounded(px(radius::CONTROL))
                .bg(Tokens::text())
                .text_color(Tokens::ink())
                .text_size(px(size::BODY))
                .font_weight(FontWeight::MEDIUM)
                .debug_selector(|| "hdr-new".into())
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| this.new_session(cx)))
                .child(glyph(icon::PLUS, 11.))
                .child(deck.new_session_state().label()),
        )
}

/// The selected task's title, its project's faint chip and its status line;
/// the project alone when it has no task yet (ADR 0010 §1).
fn title_block(deck: &Deck, window: &Window) -> Option<AnyElement> {
    let HeaderTitle { title, project, detail } = deck.header_title();
    if let Some(title) = title {
        let width = title_width(&title, window);
        let chip = project.map(|project| {
            div()
                .debug_selector(|| "hdr-project".into())
                .flex_none()
                .h(px(18.))
                .px(px(space::TIGHT))
                .flex()
                .items_center()
                .rounded_full()
                .border_1()
                .border_color(Tokens::edge())
                .text_size(px(size::MICRO))
                .text_color(Tokens::text_faint())
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
                                .text_size(px(size::TITLE))
                                .line_height(px(17.))
                                .child(title),
                        )
                        .children(chip),
                )
                .children(detail.map(|detail| {
                    div()
                        .debug_selector(|| "hdr-detail".into())
                        .min_w_0()
                        .truncate()
                        .line_height(px(14.))
                        .text_size(px(size::SMALL))
                        .text_color(Tokens::text_faint())
                        .child(detail)
                }))
                .into_any_element(),
        );
    }
    project.map(|project| {
        div()
            .debug_selector(|| "hdr-title".into())
            .w(title_width(&project, window))
            .flex_shrink()
            .min_w_0()
            .truncate()
            .font_weight(FontWeight::SEMIBOLD)
            .text_size(px(size::TITLE))
            .line_height(px(17.))
            .child(project)
            .into_any_element()
    })
}

/// The width the header title needs, capped at [`TITLE_MAX`]. A truncating
/// text measures as its ellipsis inside a flex row, so the title's own width
/// is measured here and the project chip can follow the text.
fn title_width(text: &str, window: &Window) -> Pixels {
    let font = Font { family: SANS.into(), weight: FontWeight::SEMIBOLD, ..Font::default() };
    let run = TextRun { len: text.len(), font, ..TextRun::default() };
    let width = window.text_system().layout_line(text, px(size::TITLE), &[run], None).width;
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
        .child(glyph(g, 13.))
}

pub fn kbd(keys: &'static str) -> impl IntoElement {
    div()
        .px(px(space::TIGHT))
        .py(px(1.))
        .rounded(px(radius::CHIP))
        .border_1()
        .border_color(Tokens::edge())
        .font_family(MONO)
        .text_size(px(size::MICRO))
        .text_color(Tokens::text_faint())
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
