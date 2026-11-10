//! The sidebar's one view: the shell that floats over the transcript, the
//! wordmark and the foot (ADR 0010 §1, §7, §9 as amended 2026-11-09).
//!
//! The design choice: the sidebar paints as the root's last child, absolutely
//! positioned, so when it opens it floats over the transcript (with a shadow)
//! instead of pushing it; pinned, the layout reserves its width. The open and
//! narrow views cross-fade on the label alpha the rail already computes, and
//! the more opaque one is painted last so it also takes the pointer. The open
//! rows live in `sidebar_rows.rs`, the task rows in `sidebar_tasks.rs`, and
//! the narrow glyphs in `sidebar_narrow.rs`.

use super::panels::glyph;
use super::rail::{COLLAPSED, ITEM_H};
use super::root::Deck;
use super::sidebar;
use super::sidebar_narrow::{count_badge, narrow_view};
use super::sidebar_rows::rows_view;
use super::theme::{Tokens, icon, radius, size, space, tint};
use gpui::{
    AnyElement, BoxShadow, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, point, px,
};

/// The rail's inner padding (sides, ends), the wordmark's height, and the gap
/// between the wordmark and the items; together they place each item.
const PAD_X: f32 = 6.;
const PAD_Y: f32 = 10.;
const MARK_H: f32 = 36.;
const MARK_GAP: f32 = 18.;

/// The centre of the narrow glyph's `index`, in window coordinates, for a rail
/// inset `inset` and a needs count `count` (the badge occupies one row).
pub fn rail_item_center(index: usize, inset: f32, count: usize) -> (f32, f32) {
    // The 1px border sits inside the inset, before the padding.
    let x = inset + 1. + PAD_X + (COLLAPSED - 12.) / 2.;
    let badge = if count > 0 { ITEM_H } else { 0. };
    let y = inset + 1. + PAD_Y + MARK_H + MARK_GAP + badge + (index as f32 + 0.5) * ITEM_H;
    (x, y)
}

/// The whole sidebar: the needs section, the projects and their tasks, the
/// settled fold, and the narrow glyphs, painted where the rail was.
pub fn sidebar_view(deck: &mut Deck, inset: f32, cx: &mut Context<Deck>) -> impl IntoElement {
    let (needs, projects) = deck.sidebar_inputs();
    let selected = deck.projects.selected_id().map(str::to_owned);
    let settled = deck.settled().len();
    let rows = sidebar::rows(&needs, &projects, selected.as_deref(), settled);
    let (count, glyphs) = sidebar::narrow(&needs, &projects, selected.as_deref());
    let open = deck.rail.openness().clamp(0., 1.2);
    // Labels arrive after the sidebar has started to open, and leave before it closes.
    let label_a = ((open - 0.35) / 0.65).clamp(0., 1.);
    let weak = cx.entity().downgrade();
    let marker = deck.rail.marker.pos;
    let pinned = deck.rail.pinned();
    let width = deck.rail.width.pos;

    let open_view = div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .opacity(label_a)
        .flex()
        .flex_col()
        .px(px(PAD_X))
        .py(px(PAD_Y))
        .gap(px(MARK_GAP))
        .child(mark(label_a))
        .child(rows_view(deck, &rows, cx))
        .child(div().flex_1())
        .child(open_project(label_a, cx))
        .child(pin_item(pinned, label_a, cx));
    let narrow = div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .opacity(1. - label_a)
        .flex()
        .flex_col()
        .px(px(PAD_X))
        .py(px(PAD_Y))
        .gap(px(MARK_GAP))
        .child(mark(0.))
        .children((count > 0).then(|| count_badge(count)))
        .child(narrow_view(&glyphs, marker, cx))
        .child(div().flex_1())
        .child(open_project(0., cx))
        .child(pin_item(pinned, 0., cx));

    div()
        .id("sidebar")
        .absolute()
        .top(px(inset))
        .bottom(px(inset))
        .left(px(inset))
        .w(px(width))
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
        // The more opaque view is painted last, so it also takes the pointer.
        .child(if label_a >= 0.5 {
            div().relative().size_full().child(narrow).child(open_view)
        } else {
            div().relative().size_full().child(open_view).child(narrow)
        })
}

/// The wordmark: a small glass tile, and the name once the sidebar has room.
fn mark(label_a: f32) -> AnyElement {
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
        .into_any_element()
}

fn open_project(label_a: f32, cx: &mut Context<Deck>) -> AnyElement {
    rail_item("sidebar-open", icon::PLUS, "Open project…", false, None, label_a)
        .debug_selector(|| "sidebar-open".into())
        .on_click(cx.listener(|this, _, _, cx| this.pick_project(cx)))
        .into_any_element()
}

fn pin_item(pinned: bool, label_a: f32, cx: &mut Context<Deck>) -> AnyElement {
    rail_item("sidebar-pin", icon::PIN, if pinned { "Unpin" } else { "Keep open" }, false, None, label_a)
        .when_pinned(pinned)
        .debug_selector(|| "sidebar-pin".into())
        .on_click(cx.listener(|this, _, _, cx| {
            this.rail.toggle_pin();
            cx.notify();
        }))
        .into_any_element()
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
