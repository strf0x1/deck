//! Appearance is a window-local typography trial in the conversation pane.
//! It uses the same theme tokens and native shaping as the rest of Deck.

use super::fonts;
use super::root::Deck;
use super::specimens::{self, code, label, surface};
use super::theme::{Tokens, size, space, tint};
use super::typography::{CodeFont, InterfaceFont, Resize};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, StatefulInteractiveElement as _,
    Styled as _, div, px, relative,
};

fn button(
    id: impl Into<gpui::ElementId>,
    text: impl Into<gpui::SharedString>,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .flex_none()
        .px(px(space::STACK))
        .py(px(space::TIGHT))
        .rounded(px(super::theme::radius::CONTROL))
        .border_1()
        .border_color(if selected { Tokens::accent() } else { Tokens::edge().into() })
        .bg(if selected { tint(Tokens::accent(), 0.12) } else { Tokens::raised().into() })
        .cursor_pointer()
        .hover(|s| s.bg(Tokens::raised_hover()))
        .child(text.into())
}

pub fn panel(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let typeface = &deck.typography;
    let code_choices = CodeFont::ALL.into_iter().enumerate().map(|(i, font)| {
        let available = fonts::resolve(font.family(), cx).exact();
        button(("type-code-choice", i), font.label(), typeface.code == font)
            .when(available, |b| {
                b.on_click(cx.listener(move |this, _, _, cx| {
                    this.set_code_font(font, cx);
                }))
            })
            .when(!available, |b| b.opacity(0.45))
    });
    let ui_choices = InterfaceFont::ALL.into_iter().enumerate().map(|(i, font)| {
        let report = fonts::resolve(font.family(), cx);
        button(("type-ui", i), font.label(), typeface.interface == font)
            .debug_selector(move || format!("type-ui-{i}"))
            .when(report.exact(), |b| {
                b.on_click(cx.listener(move |this, _, _, cx| {
                    this.set_interface_font(font, cx);
                }))
            })
            .when(!report.exact(), |b| b.opacity(0.45))
    });
    let candidates = CodeFont::ALL.into_iter().enumerate().map(|(i, font)| {
        let report = fonts::resolve(font.family(), cx);
        surface()
            .flex_1()
            .min_w(px(230.))
            .child(
                button(("type-code", i), font.family(), typeface.code == font)
                    .debug_selector(move || format!("type-code-{i}"))
                    .when(report.exact(), |b| {
                        b.on_click(cx.listener(move |this, _, _, cx| {
                            this.set_code_font(font, cx);
                        }))
                    })
                    .when(!report.exact(), |b| b.opacity(0.45)),
            )
            .child(label("Code · 13 px at 100%"))
            .child(code(font.family(), specimens::CODE))
            .child(code(font.family(), "Il1  O0  {} [] () => !=  | / \\"))
            .child(code(font.family(), format!("OPENAI_API_KEY={}", specimens::DUMMY_KEY)))
            .child(div().text_size(size::scaled(size::SMALL)).text_color(Tokens::text_dim()).child(report.label()))
    });
    let modifier = if cfg!(target_os = "macos") { "Cmd" } else { "Ctrl" };
    div()
        .id("appearance")
        .debug_selector(|| "appearance".into())
        .flex_1()
        .min_h_0()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(space::ROW))

        .child(
            div()
                .id("appearance-scroll")
                .debug_selector(|| "appearance-scroll".into())
                .flex_1()
                .min_h_0()
                .min_w_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(space::PANE_V))
                .child(label("Live trial · changes last until this window closes"))
                .child(
                    div().flex_none().flex().flex_wrap().items_center().gap(px(space::LINE))
                        .child(label("Text size"))
                        .child(button("type-smaller", "−", false).debug_selector(|| "type-smaller".into()).on_click(cx.listener(|this, _, _, cx| {
                            this.change_text_size(Resize::Smaller, cx);
                        })))
                        .child(div().debug_selector(|| "type-percent".into()).child(format!("{}%", typeface.percent())))
                        .child(button("type-larger", "+", false).debug_selector(|| "type-larger".into()).on_click(cx.listener(|this, _, _, cx| {
                            this.change_text_size(Resize::Larger, cx);
                        })))
                        .child(button("type-reset-size", "100%", false).on_click(cx.listener(|this, _, _, cx| {
                            this.change_text_size(Resize::Reset, cx);
                        }))),
                )
                .child(label("Interface and prose"))
                .child(div().flex_none().flex().flex_wrap().gap(px(space::LINE)).children(ui_choices))
                .child(label("Code font"))
                .child(div().flex_none().flex().flex_wrap().gap(px(space::LINE)).children(code_choices))
                .child(label("Labels 14 · prose 15 · code 13 · hints 11 · headings 17 / 21"))
                .child(specimens::hierarchy(typeface))
                .child(label("Compare code fonts · select one to try it live"))
                .child(div().flex_none().min_w_0().flex().flex_wrap().gap(px(space::INSET)).children(candidates))
                .child(
                    div().flex_none().text_size(size::scaled(size::SMALL)).line_height(relative(1.5))
                        .text_color(Tokens::text_dim())
                        .child("Masked is an explicit visual option. Copied text keeps the original characters. All keys here are dummy samples. Editable values always use the regular build."),
                )
                .child(label("The code font applies to Markdown blocks, diffs, commands and tool output. Masked code uses one colour so key masking can cross syntax boundaries."))
                .child(label("Font resolution"))
                .children([
                    typeface.interface.family(), typeface.code.family(), typeface.code.value_family(),
                ].map(|family| div().flex_none().text_size(size::scaled(size::SMALL)).child(fonts::resolve(family, cx).label())))
                .child(label("Unavailable font choices are dimmed. Install the family and reopen Deck to try it."))
                .child(label("Keys"))
                .child(div().flex_none().text_size(size::scaled(size::SMALL)).child(format!("{modifier} + / − to resize · {modifier} 0 to reset size · Esc to close")))
                .child(button("type-defaults", "Reset appearance", false).on_click(cx.listener(|this, _, _, cx| {
                    this.reset_typography(cx);
                }))),
        )
        .into_any_element()
}
