//! What the Limits panel asks of `deckd` (deck#151 R7): read the
//! coordinator's policy or a project's initial command, and save unless it changed meanwhile, both off
//! the UI thread; and the panel's keys.
//!
//! The design choice: `coordinator_policy` has no revision, so a save reads
//! the current settings first and writes only if they are still the ones the
//! panel loaded; otherwise it reloads them and says so. A refusal from
//! `deckd` is shown in the panel, which stays open with the user's values.

use super::limits::{Field, Target, Values};
use super::root::Deck;
use anyhow::Context as _;
use deck_runtime::DaemonClient;
use gpui::{Context, KeyDownEvent};

/// What a save came back with.
enum Saved {
    Done(Values),
    Stale(Values),
}

impl Target {
    fn load(&self, daemon: &DaemonClient) -> anyhow::Result<Values> {
        match self {
            Target::Coordinator(id) => daemon.coordinator_policy(id, None).map(Values::Policy),
            Target::Project(id) => {
                let project =
                    daemon.projects()?.into_iter().find(|p| &p.id == id).context("the project is no longer open")?;
                Ok(Values::Command(project.coordinator_argv))
            }
        }
    }

    fn save(&self, daemon: &DaemonClient, values: Values) -> anyhow::Result<Values> {
        match (self, values) {
            (Target::Coordinator(id), Values::Policy(settings)) => {
                daemon.coordinator_policy(id, Some(settings)).map(Values::Policy)
            }
            (Target::Project(id), Values::Command(argv)) => {
                daemon.set_coordinator_argv(id, argv.clone())?;
                Ok(Values::Command(argv))
            }
            _ => anyhow::bail!("the form does not match its destination"),
        }
    }
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
        let target = match self.coordinator() {
            Some(coordinator) => Target::Coordinator(coordinator.id.clone()),
            None => match self.project() {
                Some(project) => Target::Project(project.to_string()),
                None => return,
            },
        };
        if self.settings.is_open() {
            self.settings.toggle();
        }
        if self.access.is_open() {
            self.toggle_access(cx);
        }
        self.publication_open = None;
        self.limits.open(target.clone(), field);
        cx.notify();
        let Some(daemon) = self.daemon.clone() else { return };
        let asked = target.clone();
        let read = cx.background_executor().spawn(async move { asked.load(&daemon) });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |deck, cx| {
                if deck.limits.target() != Some(&target) {
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
        let Some(target) = self.limits.target().cloned() else { return };
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
        let asked = target.clone();
        let written = cx.background_executor().spawn(async move {
            let current = asked.load(&daemon)?;
            if current != base {
                return anyhow::Ok(Saved::Stale(current));
            }
            asked.save(&daemon, set).map(Saved::Done)
        });
        cx.spawn(async move |this, cx| {
            let result = written.await;
            let _ = this.update(cx, |deck, cx| {
                if deck.limits.target() != Some(&target) {
                    return;
                }
                match result {
                    Ok(Saved::Done(settings)) => {
                        let raised = match &settings {
                            Values::Policy(settings) => deck.limit.as_ref().map(|cap| match cap.cap.as_str() {
                                "creates_per_hour" => settings.policy.creates_per_hour,
                                _ => settings.policy.max_concurrent,
                            }),
                            Values::Command(_) => None,
                        };
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
