//! The footer under a finished turn that changed files: how many, the
//! lines added and removed, and a chip per file that opens its last edit.
//!
//! The design choice: no box. The footer summarises cards already on
//! screen, so it is quiet text with chips, and a chip is a way back to the
//! card, not a second copy of the diff.

use super::panels::glyph;
use super::root::Deck;
use super::theme::{MONO, Tokens, icon, radius, size, space};
use super::transcript::{file_name, touched};
use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, StatefulInteractiveElement as _,
    Styled as _, div, prelude::FluentBuilder as _, px,
};
use std::ops::Range;

pub fn footer(deck: &Deck, turn: Range<usize>, cx: &mut Context<Deck>) -> AnyElement {
    let files = touched(&deck.session.entries[turn.clone()]);
    let (added, removed) = files.iter().fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
    let count = |text: String| div().flex_none().font_family(MONO).text_size(px(size::SMALL)).child(text);
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(space::TIGHT))
        .text_size(px(size::LABEL))
        .text_color(Tokens::text_faint())
        .child(glyph(icon::EDIT, 10.))
        .child(format!("{} file{} changed", files.len(), if files.len() == 1 { "" } else { "s" }))
        // With one file its chip already says it.
        .when(files.len() > 1, |d| {
            d.child(count(format!("+{added}")).text_color(Tokens::lime()))
                .child(count(format!("−{removed}")).text_color(Tokens::red()))
        })
        .children(files.into_iter().map(|f| {
            let entry = turn.start + f.entry;
            div()
                .id(format!("touched:{}:{}", turn.start, f.path))
                .debug_selector({
                    let name = format!("touched-{}", file_name(&f.path));
                    move || name
                })
                .flex()
                .gap(px(5.))
                .px(px(7.))
                .py(px(2.))
                .rounded(px(radius::CHIP))
                .bg(Tokens::raised())
                .hover(|s| s.bg(Tokens::raised_hover()))
                .cursor_pointer()
                .font_family(MONO)
                .text_size(px(size::SMALL))
                .on_click(cx.listener(move |this, _, _, cx| this.reveal(entry, cx)))
                .child(div().text_color(Tokens::text_dim()).child(file_name(&f.path).to_string()))
                .child(div().text_color(Tokens::lime()).child(format!("+{}", f.added)))
                .child(div().text_color(Tokens::red()).child(format!("−{}", f.removed)))
        }))
        .into_any_element()
}
