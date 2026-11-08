//! The window's frame: the navigation rail on the left and the header across
//! the top. Neither does much yet beyond the rail's motion; they exist so the
//! layout can be judged as a whole.
//!
//! The design choice: the rail paints as the root's last child, absolutely
//! positioned, so when it opens it floats over the sessions column (with a
//! shadow) instead of pushing it; the layout reserves only the collapsed width.

use super::board::LaneState;
use super::panels::{glyph, state_color};
use super::rail::{COLLAPSED, ITEM_H};
use super::root::Deck;
use super::theme::{MONO, Tokens, icon, radius, size, space, tint};
use gpui::{
    BoxShadow, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, point, px,
};

/// Space the root reserves on the left for the closed rail.
pub const RAIL_SLOT: f32 = COLLAPSED;

/// The rail's inner padding (sides, ends), the wordmark's height, and the gap
/// between the wordmark and the items; together they place each item.
const PAD_X: f32 = 6.;
const PAD_Y: f32 = 10.;
const MARK_H: f32 = 36.;
const MARK_GAP: f32 = 18.;

/// The centre of the closed rail's item `index`, in window coordinates, for a
/// rail inset `inset` from the window edges.
pub fn rail_item_center(index: usize, inset: f32) -> (f32, f32) {
    // The 1px border sits inside the inset, before the padding.
    let x = inset + 1. + PAD_X + (COLLAPSED - 12.) / 2.;
    let y = inset + 1. + PAD_Y + MARK_H + MARK_GAP + (index as f32 + 0.5) * ITEM_H;
    (x, y)
}

pub fn rail(deck: &Deck, inset: f32, cx: &mut Context<Deck>) -> impl IntoElement {
    let r = &deck.rail;
    let open = r.openness().clamp(0., 1.2);
    // Labels arrive after the rail has started to open, and leave before it closes.
    let label_a = ((open - 0.35) / 0.65).clamp(0., 1.);
    let waiting = deck.board.cards().iter().any(|c| c.lane.state == LaneState::Waiting);
    let weak = cx.entity().downgrade();
    let active = r.active();

    let items: Vec<_> = deck
        .projects()
        .iter()
        .enumerate()
        .map(|(i, project)| {
            let selected = active == Some(i);
            let badge = (selected && waiting).then(Tokens::amber);
            rail_item(("rail-project", i), icon::PROJECT, &project.name, selected, badge, label_a)
                .debug_selector(move || format!("rail-item-{i}"))
                .on_click(cx.listener(move |this, _, _, cx| this.select_project(i, cx)))
                .into_any_element()
        })
        .collect();
    let marker = active.map(|_| {
        div()
            .absolute()
            .top(px(r.marker.pos))
            .left_0()
            .right_0()
            .h(px(ITEM_H))
            .rounded(px(radius::CONTROL))
            .bg(Tokens::raised_hover())
            .child(
                div()
                    .absolute()
                    .left(px(-6.))
                    .top(px(12.))
                    .w(px(3.))
                    .h(px(16.))
                    .rounded(px(radius::MARK))
                    .bg(Tokens::accent()),
            )
    });

    let open_item = rail_item("rail-open", icon::PLUS, "Open project…", false, None, label_a)
        .debug_selector(|| "rail-open".into())
        .on_click(cx.listener(|this, _, _, cx| this.pick_project(cx)));
    let pin = rail_item("rail-pin", icon::PIN, if r.pinned() { "Unpin" } else { "Keep open" }, false, None, label_a)
        .when_pinned(r.pinned())
        .on_click(cx.listener(|this, _, _, cx| {
            this.rail.toggle_pin();
            cx.notify();
        }));

    div()
        .id("rail")
        .absolute()
        .top(px(inset))
        .bottom(px(inset))
        .left(px(inset))
        .w(px(r.width.pos))
        .rounded(px(radius::FIELD))
        .border_1()
        .border_color(Tokens::edge())
        .bg(Tokens::panel())
        .overflow_hidden()
        .shadow(vec![BoxShadow {
            color: tint(Tokens::shadow(), 0.55 * open.min(1.)),
            offset: point(px(0.), px(12.)),
            blur_radius: px(36.),
            spread_radius: px(0.),
        }])
        .on_hover(move |hovered, _, cx| {
            let hovered = *hovered;
            weak.update(cx, |deck, cx| {
                deck.rail.hover(hovered);
                cx.notify();
            })
            .ok();
        })
        .child(div().absolute().top_0().left_0().size_full().bg(tint(Tokens::float().into(), open.min(1.))))
        .child(
            div()
                .relative()
                .size_full()
                .flex()
                .flex_col()
                .px(px(PAD_X))
                .py(px(PAD_Y))
                .gap(px(MARK_GAP))
                .child(mark(label_a))
                .child(div().relative().flex().flex_col().children(marker).children(items))
                .child(div().flex_1())
                .child(open_item)
                .child(pin),
        )
}

/// The wordmark: a small glass tile, and the name once the rail has room.
fn mark(label_a: f32) -> impl IntoElement {
    div()
        .h(px(MARK_H))
        .flex()
        .items_center()
        .child(
            div().flex_none().w(px(COLLAPSED - 12.)).flex().justify_center().child(
                div()
                    .size(px(30.))
                    .rounded(px(radius::CONTROL))
                    .border_1()
                    .border_color(Tokens::edge_strong())
                    .bg(Tokens::raised_hover())
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(size::TITLE))
                    .child("d"),
            ),
        )
        .child(
            div()
                .pl(px(space::TIGHT))
                .opacity(label_a)
                .whitespace_nowrap()
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(size::TITLE))
                .child("deck"),
        )
}

fn rail_item(
    id: impl Into<gpui::ElementId>,
    icon: &'static str,
    label: &str,
    active: bool,
    badge: Option<gpui::Hsla>,
    label_a: f32,
) -> gpui::Stateful<gpui::Div> {
    let fg = if active { Tokens::text() } else { Tokens::text_dim() };
    div()
        .id(id.into())
        .relative()
        .h(px(ITEM_H))
        .flex()
        .items_center()
        .rounded(px(radius::CONTROL))
        .cursor_pointer()
        .text_color(fg)
        .hover(|s| s.text_color(Tokens::text()))
        .child(
            div()
                .relative()
                .flex_none()
                .w(px(COLLAPSED - 12.))
                .flex()
                .justify_center()
                .child(glyph(icon, 15.))
                .children(badge.map(|c| div().absolute().top(px(-2.)).right(px(8.)).size(px(7.)).rounded_full().bg(c))),
        )
        .child(
            div()
                .pl(px(6. + 10. * (1. - label_a)))
                .opacity(label_a)
                .whitespace_nowrap()
                .text_size(px(size::BODY))
                .font_weight(if active { FontWeight::MEDIUM } else { FontWeight::NORMAL })
                .child(label.to_string()),
        )
}

trait PinExt {
    fn when_pinned(self, pinned: bool) -> Self;
}

impl PinExt for gpui::Stateful<gpui::Div> {
    fn when_pinned(self, pinned: bool) -> Self {
        if pinned { self.text_color(Tokens::accent()) } else { self }
    }
}

pub fn header(deck: &Deck, cx: &mut Context<Deck>) -> impl IntoElement {
    let live = deck.board.cards().iter().find(|c| c.lane.id == super::root::LIVE).map(|c| c.lane.state);
    let dot = live.map_or(Tokens::text_faint().into(), state_color);
    div()
        .h(px(40.))
        .flex()
        .items_center()
        .gap(px(space::STACK))
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
