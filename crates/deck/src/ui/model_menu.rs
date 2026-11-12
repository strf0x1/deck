//! New session's model menu (ADR 0012 §4): every `<provider>/<model>` the
//! settings offer, the one New session will start on checked, `deckd`'s
//! default tagged. No state of its own.
//!
//! The design choice: it paints as an overlay on the root, under the
//! header's chip, so it floats over the transcript instead of growing the
//! header; a pick only changes what the next create sends.

use super::glyph::glyph;
use super::panels::eyebrow;
use super::root::Deck;
use super::settings_form::tag;
use super::theme::{MONO, Tokens, icon, radius, size, space, tint};
use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, StatefulInteractiveElement as _,
    Styled as _, div, px,
};

/// New session's model menu, under the header's model chip: every
/// `<provider>/<model>`, the one it will start on checked.
pub fn model_menu(deck: &Deck, top: f32, right: f32, cx: &mut Context<Deck>) -> AnyElement {
    let current = deck.settings().session_model();
    let default = deck.settings().settings().map(|s| s.default.clone());
    let rows = deck.settings().choices().into_iter().enumerate().map(|(i, choice)| {
        let on = current.as_deref() == Some(choice.as_str());
        let pick = choice.clone();
        div()
            .id(("hdr-model-choice", i))
            .debug_selector(move || format!("hdr-model-{i}"))
            .min_h(px(28.))
            .px(px(space::INSET))
            .flex()
            .items_center()
            .gap(px(space::LINE))
            .rounded(px(radius::CONTROL))
            .cursor_pointer()
            .text_color(if on { Tokens::text() } else { Tokens::text_dim() })
            .hover(|s| s.bg(Tokens::raised_hover()).text_color(Tokens::text()))
            .on_click(cx.listener(move |this, _, _, cx| this.pick_session_model(&pick, cx)))
            .child(div().w(px(12.)).children(on.then(|| glyph(icon::CHECK, 11.).text_color(Tokens::cyan()))))
            .child(div().flex_1().font_family(MONO).text_size(size::scaled(size::SMALL)).child(choice.clone()))
            .children((default.as_deref() == Some(choice.as_str())).then(|| tag("default", Tokens::accent())))
    });
    div()
        .id("hdr-model-menu")
        .debug_selector(|| "hdr-model-menu".into())
        .absolute()
        .top(px(top))
        .right(px(right))
        .w(px(320.))
        .max_h(px(360.))
        .overflow_y_scroll()
        .p(px(space::TIGHT))
        .flex()
        .flex_col()
        .rounded(px(radius::CARD))
        .border_1()
        .border_color(Tokens::edge_strong())
        .bg(Tokens::float())
        .shadow(vec![gpui::BoxShadow {
            color: tint(Tokens::shadow(), 0.5),
            offset: gpui::point(px(0.), px(10.)),
            blur_radius: px(28.),
            spread_radius: px(0.),
        }])
        .child(div().px(px(space::INSET)).py(px(space::TIGHT)).child(eyebrow("New session starts on")))
        .children(rows)
        .child(
            div()
                .id("hdr-model-manage")
                .debug_selector(|| "hdr-model-manage".into())
                .min_h(px(30.))
                .px(px(space::INSET))
                .mt(px(space::TIGHT))
                .flex()
                .items_center()
                .gap(px(space::LINE))
                .rounded(px(radius::CONTROL))
                .border_t_1()
                .border_color(Tokens::edge())
                .cursor_pointer()
                .text_size(size::scaled(size::SMALL))
                .text_color(Tokens::text_dim())
                .hover(|s| s.bg(Tokens::raised_hover()).text_color(Tokens::text()))
                .on_click(cx.listener(|this, _, _, cx| this.manage_models(cx)))
                .child(glyph(icon::GEAR, 11.))
                .child("Manage models…"),
        )
        .into_any_element()
}
