//! The settings panel's daemon calls (ADR 0012 §4): load and save settings,
//! list, set and delete stored secrets. Each runs off the UI thread and the
//! panel changes only from `deckd`'s reply.
//!
//! The design choice: the panel is the only surface over the conversation
//! body while it is open, so opening it closes Access and the publication
//! panel; and a secret goes straight from the field to its background call,
//! never through `Deck` state.

use super::root::Deck;
use super::settings::{Field, Form};
use deck_runtime::broker::Listing;
use deck_runtime::settings::Settings;
use deck_runtime::settings::file::Snapshot;
use gpui::{Context, KeyDownEvent};

impl Deck {
    pub fn settings(&self) -> &super::settings::SettingsPanel {
        &self.settings
    }

    pub fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings.toggle() {
            if self.access.is_open() {
                self.toggle_access(cx);
            }
            self.publication_open = None;
            self.load_settings(cx);
            self.load_secrets(cx);
        }
        cx.notify();
    }

    /// Read `deckd`'s settings: on launch (for New session's model) and
    /// each time the panel opens.
    pub(super) fn load_settings(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = self.daemon.clone() else { return };
        let ticket = self.settings.begin_load();
        let read = cx.background_executor().spawn(async move { daemon.settings() });
        cx.spawn(async move |this, cx| {
            let result = read.await.map_err(|e| format!("reading settings: {e:#}"));
            let _ = this.update(cx, |deck, cx| deck.on_settings(ticket, result, cx));
        })
        .detach();
    }

    /// Apply a settings read; replies to an older request are dropped.
    pub fn on_settings(&mut self, ticket: u64, result: Result<Snapshot, String>, cx: &mut Context<Self>) {
        self.settings.loaded(ticket, result);
        cx.notify();
    }

    fn load_secrets(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = self.daemon.clone() else { return };
        let ticket = self.settings.begin_secrets();
        let read = cx.background_executor().spawn(async move { daemon.stored_credentials() });
        cx.spawn(async move |this, cx| {
            let result = read.await.map_err(|e| format!("listing secrets: {e:#}"));
            let _ = this.update(cx, |deck, cx| deck.on_secrets(ticket, result, cx));
        })
        .detach();
    }

    pub fn on_secrets(&mut self, ticket: u64, result: Result<Listing, String>, cx: &mut Context<Self>) {
        self.settings.secrets_listed(ticket, result);
        cx.notify();
    }

    fn save_settings(&mut self, pending: Option<(Settings, u64)>, cx: &mut Context<Self>) {
        cx.notify();
        let Some((settings, revision)) = pending else { return };
        let Some(daemon) = self.daemon.clone() else {
            return self.settings.saved(Err("no daemon to save to".into()));
        };
        let write = cx.background_executor().spawn(async move { daemon.set_settings(settings, revision) });
        cx.spawn(async move |this, cx| {
            let result = write.await.map_err(|e| format!("saving settings: {e:#}"));
            let _ = this.update(cx, |deck, cx| {
                deck.settings.saved(result);
                cx.notify();
            });
        })
        .detach();
    }

    /// Cancel the confirmation or the form, keeping the panel open.
    pub fn settings_cancel(&mut self, cx: &mut Context<Self>) {
        self.settings.cancel();
        cx.notify();
    }

    pub fn add_provider(&mut self, cx: &mut Context<Self>) {
        self.settings.add_provider();
        cx.notify();
    }

    pub fn edit_provider(&mut self, name: &str, cx: &mut Context<Self>) {
        self.settings.edit_provider(name);
        cx.notify();
    }

    pub fn edit_models(&mut self, name: &str, cx: &mut Context<Self>) {
        self.settings.edit_models(name);
        cx.notify();
    }

    /// Ask `deckd` for the open provider's models. The first answer may come
    /// from its cache; Refresh asks the provider again.
    pub fn fetch_models(&mut self, cx: &mut Context<Self>) {
        self.read_models(false, cx);
    }

    pub fn refresh_models(&mut self, cx: &mut Context<Self>) {
        self.read_models(true, cx);
    }

    fn read_models(&mut self, refresh: bool, cx: &mut Context<Self>) {
        let Some((provider, ticket)) = self.settings.begin_models() else { return cx.notify() };
        cx.notify();
        let Some(daemon) = self.daemon.clone() else {
            self.settings.models_listed(ticket, Err("no daemon to ask for a model list".into()));
            return cx.notify();
        };
        let read = cx.background_executor().spawn(async move { daemon.list_models(&provider, refresh) });
        cx.spawn(async move |this, cx| {
            let result = read.await.map_err(|e| format!("{e:#}"));
            let _ = this.update(cx, |deck, cx| {
                deck.settings.models_listed(ticket, result);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn toggle_model(&mut self, id: &str, cx: &mut Context<Self>) {
        self.settings.toggle_model(id);
        cx.notify();
    }

    pub fn close_models(&mut self, cx: &mut Context<Self>) {
        self.settings.close_discovery();
        cx.notify();
    }

    /// The session header's Manage models: open the pane on the provider the
    /// next session would start on, once its settings have loaded.
    pub fn manage_models(&mut self, cx: &mut Context<Self>) {
        let provider = self.settings.session_model().and_then(|model| model.split_once('/').map(|(p, _)| p.to_owned()));
        self.settings.manage(provider);
        if self.settings.picking() {
            self.toggle_model_picker(cx);
        }
        if self.settings.is_open() {
            self.load_settings(cx);
            cx.notify();
        } else {
            self.toggle_settings(cx);
        }
    }

    pub fn focus_settings_field(&mut self, field: Field, cx: &mut Context<Self>) {
        self.settings.focus(field);
        cx.notify();
    }

    /// Save the open form: a provider's settings, or a secret's value.
    pub fn submit_settings_form(&mut self, cx: &mut Context<Self>) {
        match self.settings.form() {
            Some(Form::Provider { .. }) => {
                let pending = self.settings.submit_provider();
                self.save_settings(pending, cx);
            }
            Some(Form::Secret { .. }) => self.submit_secret(cx),
            None => {}
        }
    }

    pub fn delete_provider(&mut self, name: &str, cx: &mut Context<Self>) {
        let pending = self.settings.delete_provider(name);
        self.save_settings(pending, cx);
    }

    pub fn set_default_model(&mut self, selector: &str, cx: &mut Context<Self>) {
        let pending = self.settings.set_default(selector);
        self.save_settings(pending, cx);
    }

    pub fn open_secret(&mut self, name: &str, cx: &mut Context<Self>) {
        self.settings.open_secret(name);
        cx.notify();
    }

    fn submit_secret(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        let Some((name, secret)) = self.settings.submit_secret() else { return };
        let Some(daemon) = self.daemon.clone() else {
            return self.settings.secret_set(Err("no daemon to store it in.".into()));
        };
        let write = cx.background_executor().spawn(async move { daemon.set_credential(&name, secret) });
        cx.spawn(async move |this, cx| {
            let result = write.await.map_err(|e| format!("storing the key: {e:#}."));
            let _ = this.update(cx, |deck, cx| {
                deck.settings.secret_set(result);
                deck.load_secrets(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn delete_secret(&mut self, name: &str, cx: &mut Context<Self>) {
        cx.notify();
        let Some(name) = self.settings.delete_secret(name) else { return };
        let Some(daemon) = self.daemon.clone() else {
            return self.settings.secret_deleted(&name, Err("no daemon to delete from".into()));
        };
        let deleting = name.clone();
        let write = cx.background_executor().spawn(async move { daemon.delete_credential(&deleting) });
        cx.spawn(async move |this, cx| {
            let result = write.await.map_err(|e| format!("deleting {name}: {e:#}"));
            let _ = this.update(cx, |deck, cx| {
                deck.settings.secret_deleted(&name, result);
                deck.load_secrets(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn toggle_model_picker(&mut self, cx: &mut Context<Self>) {
        self.settings.toggle_picker();
        cx.notify();
    }

    pub fn pick_session_model(&mut self, selector: &str, cx: &mut Context<Self>) {
        self.settings.pick(selector);
        cx.notify();
    }

    /// Keys while the panel is open. Typed text reaches the focused field
    /// through the input handler after this declines the key; nothing here
    /// reaches the composer or a card behind the panel.
    pub(super) fn settings_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let k = &ev.keystroke;
        let m = &k.modifiers;
        let multiline = self.settings.focused() == Some(Field::Models);
        match k.key.as_str() {
            "escape" => {
                if !self.settings.cancel() {
                    self.toggle_settings(cx);
                }
            }
            "tab" => self.settings.next_field(m.shift),
            "enter" if self.settings.focused() == Some(Field::Search) => self.settings.add_top_match(),
            "enter" if multiline && !(m.control || m.platform) => {
                if let Some(e) = self.settings.editor_mut() {
                    e.insert("\n");
                }
            }
            "enter" => self.submit_settings_form(cx),
            _ => {
                let masked = self.settings.focused() == Some(Field::Secret);
                let word = m.alt || m.control;
                let command = m.control || m.platform;
                let Some(e) = self.settings.editor_mut() else { return false };
                match k.key.as_str() {
                    "backspace" => e.backspace(word),
                    "delete" => e.delete(word),
                    "left" => e.left(m.shift, word),
                    "right" => e.right(m.shift, word),
                    "home" => e.line_start(m.shift),
                    "end" => e.line_end(m.shift),
                    "a" if command => e.select_all(),
                    // A masked field is never copied out.
                    "c" | "x" if command && !masked => {
                        let selected = e.selected_text().to_string();
                        if !selected.is_empty() {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(selected));
                            if k.key == "x" {
                                e.insert("");
                            }
                        }
                    }
                    "c" | "x" if command => {}
                    "v" if command => {
                        if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                            e.insert(text.trim_end_matches(['\n', '\r']));
                        }
                    }
                    _ => return false,
                }
            }
        }
        cx.notify();
        true
    }
}
