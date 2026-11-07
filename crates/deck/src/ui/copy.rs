//! Copy buttons that appear on a transcript row while it is hovered: the
//! text of a message, an edit's path, a command.
//!
//! The design choice: a button shows only while its row is hovered (the
//! row is a GPUI hover group), so the transcript carries no permanent
//! chrome. After a copy it shows a check for a moment; the Deck remembers
//! which button was clicked and clears it on a timer.

use super::panels::glyph;
use super::root::Deck;
use super::theme::{Tokens, icon, radius};
use gpui::{
    AnyElement, ClipboardItem, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px,
};
use std::time::Duration;

/// The hover group each transcript row is.
pub const ROW: &str = "transcript-row";

/// How long the check shows after a copy.
const SHOWN: Duration = Duration::from_millis(1500);

/// A copy button for `text`, keyed by `key` so its check is its own.
pub fn button(deck: &Deck, key: String, text: String, cx: &mut Context<Deck>) -> AnyElement {
    let done = deck.copied.as_deref() == Some(key.as_str());
    let selector = format!("copy-{key}");
    div()
        .id(selector.clone())
        .debug_selector(move || selector)
        .flex_none()
        .size(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::CHIP))
        .cursor_pointer()
        .text_color(if done { Tokens::lime() } else { Tokens::text_faint().into() })
        .hover(|s| s.bg(Tokens::raised_hover()).text_color(Tokens::text_dim()))
        // Hidden until the row is hovered, unless it is showing its check.
        .when(!done, |d| d.invisible().group_hover(ROW, |s| s.visible()))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.copy(&key, &text, cx);
        }))
        .child(glyph(if done { icon::CHECK } else { icon::COPY }, 10.))
        .into_any_element()
}

impl Deck {
    /// Put `text` on the clipboard and show `key`'s check for a moment.
    pub fn copy(&mut self, key: &str, text: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        self.copied = Some(key.to_string());
        let key = key.to_string();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SHOWN).await;
            let _ = this.update(cx, |d, cx| {
                if d.copied.as_deref() == Some(key.as_str()) {
                    d.copied = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Which copy button last copied, while it shows its check.
    pub fn copied(&self) -> Option<&str> {
        self.copied.as_deref()
    }
}
