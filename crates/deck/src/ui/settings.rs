//! The settings panel's state (ADR 0012 §4): `deckd`'s providers and default
//! model, the stored secrets, the one open form, and the model a New session
//! starts on.
//!
//! The design choice: the panel never edits settings in place. A form builds
//! a whole new [`Settings`] from the last snapshot and checks it with the
//! runtime's own `validate`, so it refuses what `deckd` would refuse before
//! anything is sent, and a stale save just reloads. A secret's value lives
//! only in its field's editor and is taken out (leaving the field empty) the
//! moment it is submitted; nothing here keeps or reports it.

use super::editor::Editor;
use deck_runtime::broker::{Listed, Listing};
use deck_runtime::catalog::ModelListing;
use deck_runtime::credentials::Secret;
use deck_runtime::settings::Settings as Stored;
use deck_runtime::settings::file::{Saved, Snapshot};

pub use super::settings_fields::{Confirm, Discovery, Field, Form};
use super::settings_rows::{self as rows, repair_default};
pub use super::settings_rows::{Key, ProviderRow, SecretRow};

#[derive(Default)]
pub struct SettingsPanel {
    open: bool,
    snapshot: Option<Snapshot>,
    secrets: Option<Vec<Listed>>,
    store: Option<String>,
    form: Option<Form>,
    confirm: Option<Confirm>,
    busy: bool,
    error: Option<String>,
    notice: Option<String>,
    settings_ticket: u64,
    secrets_ticket: u64,
    models_ticket: u64,
    /// A provider to open after the next settings load names one (Manage
    /// models, from the session header).
    pending_models: Option<String>,
    /// The New session model, when the user picked one other than the default.
    session_model: Option<String>,
    picking: bool,
}

impl SettingsPanel {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open or close; returns whether it is now open. Closing drops the form
    /// and any value typed into it.
    pub fn toggle(&mut self) -> bool {
        self.open = !self.open;
        self.form = None;
        self.confirm = None;
        self.error = None;
        self.notice = None;
        if !self.open {
            self.pending_models = None;
        }
        self.open
    }

    /// Escape: the confirmation, then the fetched model list, then the form,
    /// then the panel. False means there was nothing inside to cancel and
    /// the panel should close.
    pub fn cancel(&mut self) -> bool {
        if self.confirm.take().is_some() {
            return true;
        }
        if let Some(form) = &mut self.form
            && form.close_discovery()
        {
            return true;
        }
        self.form.take().is_some()
    }

    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }
    pub fn settings(&self) -> Option<&Stored> {
        self.snapshot.as_ref().map(|s| &s.settings)
    }
    pub fn store(&self) -> Option<&str> {
        self.store.as_deref()
    }
    pub fn form(&self) -> Option<&Form> {
        self.form.as_ref()
    }
    pub fn confirm(&self) -> Option<&Confirm> {
        self.confirm.as_ref()
    }
    pub fn busy(&self) -> bool {
        self.busy
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }
    pub fn picking(&self) -> bool {
        self.picking
    }

    pub fn begin_load(&mut self) -> u64 {
        self.settings_ticket += 1;
        self.settings_ticket
    }

    pub fn loaded(&mut self, ticket: u64, result: Result<Snapshot, String>) {
        if ticket != self.settings_ticket {
            return;
        }
        match result {
            Ok(snapshot) => {
                self.snapshot = Some(snapshot);
                // A pick the settings no longer offer falls back to the default.
                if self.session_model.as_ref().is_some_and(|m| !self.choices().contains(m)) {
                    self.session_model = None;
                }
                // Manage models opened the pane to edit a provider; the
                // settings that name it have only just arrived.
                if self.open
                    && let Some(provider) = self.pending_models.take()
                    && self.settings().is_some_and(|settings| settings.provider(&provider).is_some())
                {
                    self.edit_models(&provider);
                }
            }
            Err(e) => self.error = Some(e),
        }
    }

    pub fn begin_secrets(&mut self) -> u64 {
        self.secrets_ticket += 1;
        self.secrets_ticket
    }

    pub fn secrets_listed(&mut self, ticket: u64, result: Result<Listing, String>) {
        if ticket != self.secrets_ticket {
            return;
        }
        match result {
            Ok(listing) => {
                self.store = Some(listing.store);
                self.secrets = Some(listing.credentials);
            }
            Err(e) => self.error = Some(e),
        }
    }

    pub fn provider_rows(&self) -> Vec<ProviderRow> {
        self.settings().map(|s| rows::providers(s, self.secrets.as_deref())).unwrap_or_default()
    }

    /// Every stored name, and every name a provider needs that is not stored.
    pub fn secret_rows(&self) -> Vec<SecretRow> {
        rows::secrets(self.settings(), self.secrets.as_deref())
    }

    pub fn add_provider(&mut self) {
        self.open_form(Form::new_provider());
    }

    pub fn edit_provider(&mut self, name: &str) {
        if let Some(provider) = self.settings().and_then(|s| s.provider(name)).cloned() {
            self.open_form(Form::editing(&provider));
        }
    }

    /// Edit a provider with its model list in view: what the row's Models
    /// button does.
    pub fn edit_models(&mut self, name: &str) {
        self.edit_provider(name);
        self.focus(Field::Models);
    }

    /// The next settings load opens this provider's model list (the session
    /// header's Manage models, before the snapshot is there).
    pub fn manage(&mut self, provider: Option<String>) {
        self.pending_models = provider.filter(|name| !name.is_empty());
    }

    /// The fetched list under the open form, while the form has one.
    pub fn discovery(&self) -> Option<&Discovery> {
        self.form.as_ref()?.discovery()
    }

    /// Hide the fetched list, keeping the form and its allowlist text.
    pub fn close_discovery(&mut self) {
        if let Some(form) = &mut self.form {
            form.close_discovery();
        }
    }

    /// The rows drawn and the matches in all, for the discovery block and
    /// the control socket.
    pub fn discovery_shown(&self) -> Option<(usize, usize)> {
        let discovery = self.discovery()?;
        let (rows, total) = super::model_search::shown(&discovery.models, discovery.query.text());
        Some((rows.len(), total))
    }

    /// Ask `deckd` for the form's saved provider. `None` for a new provider:
    /// it has no name the daemon could resolve yet, and while the typed URL
    /// differs from the saved one, since the daemon fetches what is saved.
    pub fn begin_models(&mut self) -> Option<(String, u64)> {
        let Form::Provider { editing: Some(provider), .. } = self.form.as_ref()? else { return None };
        let provider = provider.clone();
        let saved = self.settings().and_then(|s| s.provider(&provider)).map(|p| p.url.clone());
        let Form::Provider { url, discovery, .. } = self.form.as_mut()? else { return None };
        if saved.as_deref() != Some(url.text().trim()) {
            if let Some(discovery) = discovery {
                discovery.loading = false;
                discovery.error = Some("Save the new URL first, then fetch its models.".into());
            }
            return None;
        }
        let discovery = discovery.get_or_insert_with(|| Discovery::new(provider.clone()));
        discovery.loading = true;
        discovery.error = None;
        self.models_ticket += 1;
        Some((provider, self.models_ticket))
    }

    /// Apply a `/models` read; a reply to an older request is dropped.
    pub fn models_listed(&mut self, ticket: u64, result: Result<ModelListing, String>) {
        if ticket != self.models_ticket {
            return;
        }
        let Some(discovery) = self.form.as_mut().and_then(Form::discovery_mut) else { return };
        discovery.loading = false;
        match result {
            Ok(listing) => {
                discovery.models = listing.models;
                discovery.fetched_at = listing.fetched_at;
                discovery.stale = listing.stale;
                discovery.error = listing.error;
            }
            Err(error) => {
                discovery.stale = !discovery.models.is_empty();
                discovery.error = Some(error);
            }
        }
    }

    /// Add or remove one model from the allowlist text.
    pub fn toggle_model(&mut self, id: &str) {
        if let Some(form) = &mut self.form {
            form.toggle_model(id);
        }
    }

    /// Enter in the search: the first match joins (or leaves) the allowlist.
    pub fn add_top_match(&mut self) {
        let Some(Form::Provider { discovery: Some(discovery), .. }) = self.form.as_ref() else { return };
        let id = super::model_search::search(&discovery.models, discovery.query.text())
            .first()
            .map(|model| model.id.clone());
        if let Some(id) = id {
            self.toggle_model(&id);
        }
    }

    pub fn open_secret(&mut self, name: &str) {
        self.open_form(Form::Secret { name: name.into(), value: Editor::new() });
    }

    fn open_form(&mut self, form: Form) {
        self.confirm = None;
        self.error = None;
        self.pending_models = None;
        self.form = Some(form);
    }

    pub fn focused(&self) -> Option<Field> {
        self.form.as_ref().map(Form::focused)
    }

    pub fn focus(&mut self, field: Field) {
        if let Some(form) = &mut self.form {
            form.focus(field);
        }
    }

    /// Tab: the next field of a provider form, wrapping.
    pub fn next_field(&mut self, back: bool) {
        if let Some(form) = &mut self.form {
            form.next_field(back);
        }
    }

    /// The editor typing goes to, while a form is open.
    pub fn editor(&self) -> Option<&Editor> {
        self.form.as_ref().map(Form::editor)
    }

    pub fn editor_mut(&mut self) -> Option<&mut Editor> {
        self.form.as_mut().map(Form::editor_mut)
    }

    /// The provider form as settings to save, with the revision they replace.
    pub fn submit_provider(&mut self) -> Option<(Stored, u64)> {
        let snapshot = self.snapshot.clone()?;
        let (editing, provider) = self.form.as_ref()?.provider()?;
        let mut settings = snapshot.settings;
        match editing.and_then(|old| settings.providers.iter().position(|p| p.name == old)) {
            Some(at) => {
                let old = std::mem::replace(&mut settings.providers[at], provider.clone());
                if let Some(rest) = settings.default.strip_prefix(&format!("{}/", old.name)) {
                    settings.default = format!("{}/{rest}", provider.name);
                }
            }
            None => settings.providers.push(provider),
        }
        repair_default(&mut settings);
        self.validated(settings, snapshot.revision)
    }

    /// The settings with `name` removed, once the delete was confirmed.
    pub fn delete_provider(&mut self, name: &str) -> Option<(Stored, u64)> {
        if self.confirm.as_ref() != Some(&Confirm::DeleteProvider(name.into())) {
            self.confirm = Some(Confirm::DeleteProvider(name.into()));
            return None;
        }
        self.confirm = None;
        let snapshot = self.snapshot.clone()?;
        let mut settings = snapshot.settings;
        settings.providers.retain(|p| p.name != name);
        repair_default(&mut settings);
        self.validated(settings, snapshot.revision)
    }

    pub fn set_default(&mut self, selector: &str) -> Option<(Stored, u64)> {
        let snapshot = self.snapshot.clone()?;
        let mut settings = snapshot.settings;
        settings.default = selector.to_string();
        self.validated(settings, snapshot.revision)
    }

    fn validated(&mut self, settings: Stored, revision: u64) -> Option<(Stored, u64)> {
        match settings.validate() {
            Ok(()) => {
                self.busy = true;
                self.error = None;
                Some((settings, revision))
            }
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                None
            }
        }
    }

    /// `deckd`'s answer to a save.
    pub fn saved(&mut self, result: Result<Saved, String>) {
        self.busy = false;
        match result {
            Ok(Saved::Saved(snapshot)) => {
                self.snapshot = Some(snapshot);
                self.form = None;
                self.notice = Some("Saved. New sessions use these settings.".into());
            }
            Ok(Saved::Stale(snapshot)) => {
                self.snapshot = Some(snapshot);
                self.error = Some("Another window saved settings first. Review them and save again.".into());
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// The secret to send. The field is emptied now, before any reply, so the
    /// value is gone from the window whatever happens next.
    pub fn submit_secret(&mut self) -> Option<(String, Secret)> {
        let Some(Form::Secret { name, value }) = &mut self.form else { return None };
        // A fresh editor, not `take`: that would keep the text as history.
        let text = std::mem::take(value).text().to_string();
        if text.is_empty() {
            self.error = Some("Type or paste the key first.".into());
            return None;
        }
        self.busy = true;
        self.error = None;
        Some((name.clone(), Secret::new(text)))
    }

    pub fn secret_set(&mut self, result: Result<Listed, String>) {
        self.busy = false;
        match result {
            Ok(listed) => {
                self.notice =
                    Some(format!("Stored {} ({}).", listed.name, listed.fingerprint.as_deref().unwrap_or("?")));
                let secrets = self.secrets.get_or_insert_with(Vec::new);
                secrets.retain(|l| l.name != listed.name);
                secrets.push(listed);
                secrets.sort_by(|a, b| a.name.cmp(&b.name));
                self.form = None;
            }
            Err(e) => self.error = Some(format!("{e} The field was cleared; type the key again.")),
        }
    }

    /// The name to delete, once the delete was confirmed.
    pub fn delete_secret(&mut self, name: &str) -> Option<String> {
        if self.confirm.as_ref() != Some(&Confirm::DeleteSecret(name.into())) {
            self.confirm = Some(Confirm::DeleteSecret(name.into()));
            return None;
        }
        self.confirm = None;
        self.busy = true;
        Some(name.into())
    }

    pub fn secret_deleted(&mut self, name: &str, result: Result<bool, String>) {
        self.busy = false;
        match result {
            Ok(_) => {
                if let Some(secrets) = &mut self.secrets {
                    secrets.retain(|l| l.name != name);
                }
                self.notice = Some(format!("Deleted {name}."));
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Every `<provider>/<model>` a session may start on.
    pub fn choices(&self) -> Vec<String> {
        let providers = self.settings().map(|s| s.providers.as_slice()).unwrap_or_default();
        providers.iter().flat_map(|p| p.models.iter().map(move |m| format!("{}/{m}", p.name))).collect()
    }

    /// What New session starts on: the pick, else the default.
    pub fn session_model(&self) -> Option<String> {
        self.session_model.clone().or_else(|| self.settings().map(|s| s.default.clone()))
    }

    /// The selector to send with a create: only a pick, so a task without one
    /// gets whatever `deckd`'s settings say when it is created.
    pub fn session_selector(&self) -> Option<String> {
        self.session_model.clone()
    }

    pub fn toggle_picker(&mut self) {
        self.picking = !self.picking;
    }

    pub fn pick(&mut self, selector: &str) {
        let default = self.settings().map(|s| s.default.as_str());
        self.session_model = (default != Some(selector)).then(|| selector.to_string());
        self.picking = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_runtime::settings::Provider;

    fn snapshot() -> Snapshot {
        Snapshot {
            revision: 3,
            stored: true,
            settings: Stored {
                default: "openrouter/openrouter/free".into(),
                providers: vec![Provider {
                    name: "openrouter".into(),
                    url: "https://openrouter.ai/api/v1".into(),
                    secret: Some("OPENROUTER_API_KEY".into()),
                    models: vec!["openrouter/free".into()],
                }],
            },
            overrides: Vec::new(),
        }
    }

    fn loaded() -> SettingsPanel {
        let mut panel = SettingsPanel::default();
        panel.toggle();
        let ticket = panel.begin_load();
        panel.loaded(ticket, Ok(snapshot()));
        panel
    }

    fn type_into(panel: &mut SettingsPanel, field: Field, text: &str) {
        panel.focus(field);
        let editor = panel.editor_mut().unwrap();
        editor.select_all();
        editor.insert(text);
    }

    #[test]
    fn a_new_provider_is_validated_and_saved_against_the_revision() {
        let mut panel = loaded();
        panel.add_provider();
        type_into(&mut panel, Field::Name, "local");
        type_into(&mut panel, Field::Url, "http://10.0.0.5:8000/v1");
        type_into(&mut panel, Field::Key, "none");
        type_into(&mut panel, Field::Models, "qwen3-32b\nqwen3-8b, qwen3-32b");
        let (settings, revision) = panel.submit_provider().unwrap();
        assert_eq!(revision, 3);
        assert!(panel.busy());
        let local = settings.provider("local").unwrap();
        assert_eq!((local.secret.clone(), local.models.clone()), (None, vec!["qwen3-32b".into(), "qwen3-8b".into()]));
        assert_eq!(settings.default, "openrouter/openrouter/free", "adding does not move the default");

        let mut saved = snapshot();
        saved.revision = 4;
        saved.settings = settings;
        panel.saved(Ok(Saved::Saved(saved)));
        assert!(panel.form().is_none() && panel.notice().is_some());
        assert_eq!(panel.choices().len(), 3);
    }

    #[test]
    fn an_invalid_form_is_refused_before_anything_is_sent() {
        let mut panel = loaded();
        panel.add_provider();
        type_into(&mut panel, Field::Name, "Local");
        type_into(&mut panel, Field::Url, "http://x/v1");
        type_into(&mut panel, Field::Models, "m");
        assert!(panel.submit_provider().is_none());
        assert!(panel.error().is_some_and(|e| e.contains("lowercase")), "{:?}", panel.error());
        assert!(!panel.busy() && panel.form().is_some(), "the form stays for a fix");
    }

    #[test]
    fn an_empty_key_field_names_the_providers_key_and_a_rename_moves_the_default() {
        let mut panel = loaded();
        panel.edit_provider("openrouter");
        type_into(&mut panel, Field::Name, "or-main");
        type_into(&mut panel, Field::Key, "");
        let (settings, _) = panel.submit_provider().unwrap();
        assert_eq!(settings.providers[0].secret.as_deref(), Some("OR_MAIN_API_KEY"));
        assert_eq!(settings.default, "or-main/openrouter/free");
    }

    #[test]
    fn a_stale_save_keeps_the_form_and_shows_what_is_there_now() {
        let mut panel = loaded();
        panel.edit_provider("openrouter");
        panel.submit_provider().unwrap();
        let mut now = snapshot();
        now.revision = 9;
        panel.saved(Ok(Saved::Stale(now)));
        assert!(panel.form().is_some());
        assert_eq!(panel.snapshot().unwrap().revision, 9);
        assert!(panel.error().is_some_and(|e| e.contains("Another window")));
    }

    #[test]
    fn deleting_takes_a_second_click_and_the_last_provider_cannot_go() {
        let mut panel = loaded();
        assert!(panel.delete_provider("openrouter").is_none());
        assert_eq!(panel.confirm(), Some(&Confirm::DeleteProvider("openrouter".into())));
        assert!(panel.delete_provider("openrouter").is_none(), "settings need a provider");
        assert!(panel.error().is_some_and(|e| e.contains("at least one")));
        assert!(panel.cancel() || panel.confirm().is_none());
    }

    #[test]
    fn a_secret_leaves_the_field_on_submit_whatever_the_reply() {
        let mut panel = loaded();
        panel.open_secret("OPENROUTER_API_KEY");
        assert!(panel.submit_secret().is_none(), "an empty field sends nothing");
        panel.editor_mut().unwrap().insert("sk-typed");
        let (name, secret) = panel.submit_secret().unwrap();
        assert_eq!((name.as_str(), secret.expose()), ("OPENROUTER_API_KEY", "sk-typed"));
        assert_eq!(panel.editor().unwrap().text(), "", "taken out before the reply");
        assert!(!panel.editor_mut().unwrap().recall_prev(), "and not kept as recallable history");
        panel.secret_set(Err("store locked.".into()));
        assert_eq!(panel.editor().unwrap().text(), "");
        assert!(panel.error().is_some_and(|e| e.contains("type the key again")));
        assert!(!panel.error().unwrap_or_default().contains("sk-typed"));
    }

    #[test]
    fn rows_show_each_providers_key_state_and_unused_secrets() {
        let mut panel = loaded();
        assert_eq!(panel.provider_rows()[0].key, Key::Unknown);
        let listed = |name: &str, fingerprint: Option<&str>, revoked| Listed {
            name: name.into(),
            fingerprint: fingerprint.map(Into::into),
            revoked,
            error: None,
            paused: false,
        };
        let ticket = panel.begin_secrets();
        panel.secrets_listed(
            ticket,
            Ok(Listing { store: "file".into(), credentials: vec![listed("OLD_KEY", Some("sha256:1"), false)] }),
        );
        assert_eq!(panel.provider_rows()[0].key, Key::NotSet);
        let rows = panel.secret_rows();
        assert_eq!(
            rows.iter().map(|r| (r.name.as_str(), r.providers.len())).collect::<Vec<_>>(),
            [("OLD_KEY", 0), ("OPENROUTER_API_KEY", 1)]
        );
        panel.secret_set(Ok(listed("OPENROUTER_API_KEY", Some("sha256:2"), false)));
        assert_eq!(panel.provider_rows()[0].key, Key::Set("sha256:2".into()));
        // A listing from an older request is ignored.
        panel.secrets_listed(ticket - 1, Ok(Listing { store: "x".into(), credentials: Vec::new() }));
        assert_eq!(panel.store(), Some("file"));
    }

    fn listing() -> ModelListing {
        use deck_runtime::catalog::ModelInfo;
        ModelListing {
            provider: "openrouter".into(),
            url: "https://openrouter.ai/api/v1".into(),
            models: vec![
                ModelInfo { id: "openrouter/free".into(), name: "Free".into(), tools: true, ..ModelInfo::default() },
                ModelInfo {
                    id: "anthropic/claude-sonnet-5-5".into(),
                    name: "Claude".into(),
                    tools: true,
                    ..ModelInfo::default()
                },
            ],
            fetched_at: Some(1),
            stale: false,
            error: None,
        }
    }

    #[test]
    fn a_model_list_lands_on_its_open_form_and_toggles_the_allowlist() {
        let mut panel = loaded();
        panel.edit_models("openrouter");
        assert_eq!(panel.focused(), Some(Field::Models));
        let (provider, ticket) = panel.begin_models().unwrap();
        assert_eq!(provider, "openrouter");
        assert!(panel.discovery().unwrap().loading);
        panel.models_listed(ticket - 1, Ok(listing()));
        assert!(panel.discovery().unwrap().models.is_empty(), "a reply to an older request is dropped");
        panel.models_listed(ticket, Ok(listing()));
        let discovery = panel.discovery().unwrap();
        assert_eq!((discovery.models.len(), discovery.loading, discovery.stale), (2, false, false));
        assert_eq!(panel.discovery_shown(), Some((2, 2)));

        panel.toggle_model("anthropic/claude-sonnet-5-5");
        assert!(panel.editor().unwrap().text().contains("openrouter/free\nanthropic/claude-sonnet-5-5"));
        panel.toggle_model("anthropic/claude-sonnet-5-5");
        assert_eq!(panel.editor().unwrap().text(), "openrouter/free");

        // Enter in the search adds the first match to the allowlist.
        panel.focus(Field::Search);
        panel.editor_mut().unwrap().insert("claude");
        assert_eq!(panel.discovery_shown(), Some((1, 1)));
        panel.add_top_match();
        panel.focus(Field::Models);
        assert!(panel.editor().unwrap().text().contains("anthropic/claude-sonnet-5-5"));

        // A new provider has no saved name, so there is nothing to ask for.
        panel.cancel();
        panel.add_provider();
        assert!(panel.begin_models().is_none());
    }

    #[test]
    fn a_typed_url_must_be_saved_before_its_models_can_be_fetched() {
        let mut panel = loaded();
        panel.edit_models("openrouter");
        type_into(&mut panel, Field::Url, "http://127.0.0.1:8000/v1");
        assert!(panel.begin_models().is_none(), "deckd would fetch the saved URL, not this one");
        type_into(&mut panel, Field::Url, "https://openrouter.ai/api/v1");
        assert!(panel.begin_models().is_some());
    }

    #[test]
    fn a_failed_refetch_keeps_the_list_and_says_why() {
        let mut panel = loaded();
        panel.edit_models("openrouter");
        let (_, ticket) = panel.begin_models().unwrap();
        panel.models_listed(ticket, Ok(listing()));
        let (_, ticket) = panel.begin_models().unwrap();
        panel.models_listed(ticket, Err("model list returned 500".into()));
        let discovery = panel.discovery().unwrap();
        assert_eq!(discovery.models.len(), 2);
        assert!(discovery.stale && discovery.error.as_deref().is_some_and(|e| e.contains("500")));
    }

    #[test]
    fn escape_closes_the_fetched_list_before_the_form() {
        let mut panel = loaded();
        panel.edit_models("openrouter");
        let (_, ticket) = panel.begin_models().unwrap();
        panel.models_listed(ticket, Ok(listing()));
        assert!(panel.cancel());
        assert!(panel.form().is_some() && panel.discovery().is_none());
        assert_eq!(panel.focused(), Some(Field::Models), "the caret returns to the allowlist");
        assert!(panel.cancel());
        assert!(panel.form().is_none());
    }

    #[test]
    fn manage_models_opens_the_named_provider_after_the_load() {
        let mut panel = SettingsPanel::default();
        panel.manage(Some("openrouter".into()));
        panel.toggle();
        let ticket = panel.begin_load();
        panel.loaded(ticket, Ok(snapshot()));
        assert!(matches!(panel.form(), Some(Form::Provider { editing: Some(name), .. }) if name == "openrouter"));
        assert_eq!(panel.focused(), Some(Field::Models));

        // A provider the settings do not name leaves the pane on its list.
        let mut panel = loaded();
        panel.manage(Some("gone".into()));
        let ticket = panel.begin_load();
        panel.loaded(ticket, Ok(snapshot()));
        assert!(panel.form().is_none());
    }

    #[test]
    fn a_session_pick_is_only_sent_when_it_differs_from_the_default() {
        let mut panel = loaded();
        assert_eq!(panel.session_model().as_deref(), Some("openrouter/openrouter/free"));
        assert_eq!(panel.session_selector(), None);
        panel.pick("openrouter/openrouter/free");
        assert_eq!(panel.session_selector(), None);
        let mut two = snapshot();
        two.settings.providers[0].models.push("anthropic/claude-sonnet-5-5".into());
        let ticket = panel.begin_load();
        panel.loaded(ticket, Ok(two));
        panel.pick("openrouter/anthropic/claude-sonnet-5-5");
        assert_eq!(panel.session_selector().as_deref(), Some("openrouter/anthropic/claude-sonnet-5-5"));
        // The model is removed: the pick falls back to the default.
        let ticket = panel.begin_load();
        panel.loaded(ticket, Ok(snapshot()));
        assert_eq!(panel.session_selector(), None);
    }
}
