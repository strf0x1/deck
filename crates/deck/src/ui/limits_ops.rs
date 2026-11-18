//! What the Limits panel asks of `deckd` (deck#151 R7): read the
//! coordinator's policy, and save it unless it changed meanwhile, both off
//! the UI thread; and the panel's keys.
//!
//! The design choice: `coordinator_policy` has no revision, so a save reads
//! the current settings first and writes only if they are still the ones the
//! panel loaded; otherwise it reloads them and says so. A refusal from
//! `deckd` is shown in the panel, which stays open with the user's values.

use super::limits::Field;
use super::root::Deck;
use deck_runtime::coordinator::Settings;
use gpui::{Context, KeyDownEvent};

/// What a save came back with.
enum Saved {
    Done(Settings),
    Stale(Settings),
}

impl Deck {
    pub fn limits(&self) -> &super::limits::Limits {
        &self.limits
    }

    pub fn limits_open(&self) -> bool {
        self.limits.is_open()
    }

    /// Open the selected project's coordinator's limits with the caret in
    /// `field`, or close them. Raise on a cap row opens at that cap.
    pub fn toggle_limits(&mut self, field: Option<Field>, cx: &mut Context<Self>) {
        if self.limits.is_open() && field.is_none() {
            self.limits.close();
            return cx.notify();
        }
        let Some(id) = self.coordinator().map(|c| c.id.clone()) else { return };
        if self.settings.is_open() {
            self.settings.toggle();
        }
        if self.access.is_open() {
            self.toggle_access(cx);
        }
        self.publication_open = None;
        self.limits.open(&id, field);
        cx.notify();
        let Some(daemon) = self.daemon.clone() else { return };
        let asked = id.clone();
        let read = cx.background_executor().spawn(async move { daemon.coordinator_policy(&asked, None) });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |deck, cx| {
                if deck.limits.coordinator() != Some(id.as_str()) {
                    return;
                }
                match result {
                    Ok(settings) => deck.limits.loaded(settings),
                    Err(e) => deck.limits.refuse(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Save the panel's values: refused here when invalid, reloaded when
    /// someone changed them since they were loaded, else written by `deckd`.
    pub fn save_limits(&mut self, cx: &mut Context<Self>) {
        if self.limits.busy() {
            return;
        }
        let Some(id) = self.limits.coordinator().map(str::to_owned) else { return };
        let Some(base) = self.limits.base().cloned() else { return };
        let set = match self.limits.read() {
            Ok(set) => set,
            Err(why) => {
                self.limits.refuse(why);
                return cx.notify();
            }
        };
        let Some(daemon) = self.daemon.clone() else { return };
        self.limits.saving();
        cx.notify();
        let asked = id.clone();
        let written = cx.background_executor().spawn(async move {
            let current = daemon.coordinator_policy(&asked, None)?;
            if current != base {
                return anyhow::Ok(Saved::Stale(current));
            }
            daemon.coordinator_policy(&asked, Some(set)).map(Saved::Done)
        });
        cx.spawn(async move |this, cx| {
            let result = written.await;
            let _ = this.update(cx, |deck, cx| {
                if deck.limits.coordinator() != Some(id.as_str()) {
                    return;
                }
                match result {
                    Ok(Saved::Done(settings)) => {
                        let raised = deck.limit.as_ref().map(|cap| match cap.cap.as_str() {
                            "creates_per_hour" => settings.policy.creates_per_hour,
                            _ => settings.policy.max_concurrent,
                        });
                        deck.limits.saved(settings);
                        deck.settle_limit(raised);
                    }
                    Ok(Saved::Stale(current)) => deck.limits.stale(current),
                    Err(e) => deck.limits.refuse(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The panel's keys. A key it declines is text for the focused field,
    /// which the input handler delivers.
    pub(super) fn limits_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let k = &ev.keystroke;
        let m = &k.modifiers;
        match k.key.as_str() {
            "escape" => self.toggle_limits(None, cx),
            "tab" => self.limits.next_field(m.shift),
            "enter" => self.save_limits(cx),
            key => {
                let word = m.alt || m.control;
                let command = m.control || m.platform;
                let Some(e) = self.limits.editor_mut() else { return false };
                match key {
                    "backspace" => e.backspace(word),
                    "delete" => e.delete(word),
                    "left" if command => e.line_start(m.shift),
                    "right" if command => e.line_end(m.shift),
                    "left" => e.left(m.shift, word),
                    "right" => e.right(m.shift, word),
                    "home" => e.line_start(m.shift),
                    "end" => e.line_end(m.shift),
                    "a" if command => e.select_all(),
                    _ => return false,
                }
            }
        }
        cx.notify();
        true
    }
}
