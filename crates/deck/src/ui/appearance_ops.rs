//! Window-local typography actions. Every change remeasures the virtual
//! transcript while preserving its scroll position, including offscreen rows.

use super::root::Deck;
use super::typography::{CodeFont, InterfaceFont, Resize, Typography};
use gpui::{Context, KeyDownEvent};

impl Deck {
    pub fn typography(&self) -> &Typography {
        &self.typography
    }

    pub fn appearance_open(&self) -> bool {
        self.settings.is_open() && self.appearance_open && self.settings.form().is_none()
    }

    pub fn toggle_appearance(&mut self, cx: &mut Context<Self>) {
        if self.settings.form().is_some() {
            return;
        }
        if !self.settings.is_open() {
            self.toggle_settings(cx);
        }
        self.appearance_open = !self.appearance_open;
        cx.notify();
    }

    pub(super) fn set_interface_font(&mut self, font: InterfaceFont, cx: &mut Context<Self>) {
        self.typography.interface = font;
        self.transcript.remeasure();
        cx.notify();
    }

    pub(super) fn set_code_font(&mut self, font: CodeFont, cx: &mut Context<Self>) {
        self.typography.code = font;
        self.transcript.remeasure();
        cx.notify();
    }

    pub(super) fn change_text_size(&mut self, change: Resize, cx: &mut Context<Self>) {
        self.typography.resize(change);
        self.transcript.remeasure();
        cx.notify();
    }

    pub(super) fn reset_typography(&mut self, cx: &mut Context<Self>) {
        self.typography = Typography::default();
        self.transcript.remeasure();
        cx.notify();
    }

    pub(super) fn resize_text(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let key = &event.keystroke;
        let modifiers = key.modifiers;
        if !(modifiers.control || modifiers.platform) || modifiers.alt {
            return false;
        }
        let change = match key.key.as_str() {
            "+" | "=" => Resize::Larger,
            "-" if !modifiers.shift => Resize::Smaller,
            "0" if !modifiers.shift => Resize::Reset,
            _ => return false,
        };
        self.change_text_size(change, cx);
        true
    }
}
