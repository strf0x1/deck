//! The settings panel's one open form (ADR 0012 §4): a provider's four
//! fields or a secret's one, which field has the caret, and how the fields
//! read back as a [`Provider`].
//!
//! The design choice: each field is a full composer [`Editor`], so typing,
//! IME and moves behave as in the prompt; the form only says which editor
//! the input handler should use. Field rules (the key name's two shorthands,
//! models one per line) live here, next to the fields they read.

use super::editor::Editor;
use deck_runtime::catalog::ModelInfo;
use deck_runtime::settings::Provider;

/// A field of the open form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Url,
    Key,
    Models,
    /// The search over a provider's fetched model list.
    Search,
    Secret,
}

/// One provider's fetched list, under its Models field: the ids the endpoint
/// serves, when they were fetched, and the filter over them (ADR 0012 §4).
pub struct Discovery {
    pub provider: String,
    pub models: Vec<ModelInfo>,
    pub fetched_at: Option<u64>,
    /// The list is a cache served after a failed refetch, with its reason.
    pub stale: bool,
    pub error: Option<String>,
    pub loading: bool,
    pub query: Editor,
}

impl Discovery {
    pub fn new(provider: String) -> Self {
        Self {
            provider,
            models: Vec::new(),
            fetched_at: None,
            stale: false,
            error: None,
            loading: false,
            query: Editor::new(),
        }
    }
}

/// The one open form: a provider to add or edit, or a secret to set.
#[expect(clippy::large_enum_variant, reason = "one form exists at a time, so its size never adds up")]
pub enum Form {
    Provider {
        editing: Option<String>,
        name: Editor,
        url: Editor,
        key: Editor,
        models: Editor,
        focus: Field,
        discovery: Option<Discovery>,
    },
    Secret {
        name: String,
        value: Editor,
    },
}

/// A destructive click waiting for its second click.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    DeleteProvider(String),
    DeleteSecret(String),
}

impl Form {
    pub fn new_provider() -> Self {
        Form::Provider {
            editing: None,
            name: Editor::new(),
            url: Editor::new(),
            key: Editor::new(),
            models: Editor::new(),
            focus: Field::Name,
            discovery: None,
        }
    }

    /// A provider's form filled with what it has now; `none` stands for keyless.
    pub fn editing(p: &Provider) -> Self {
        let filled = |text: &str| {
            let mut editor = Editor::new();
            editor.insert(text);
            editor
        };
        Form::Provider {
            editing: Some(p.name.clone()),
            name: filled(&p.name),
            url: filled(&p.url),
            key: filled(p.secret.as_deref().unwrap_or("none")),
            models: filled(&p.models.join("\n")),
            focus: Field::Url,
            discovery: None,
        }
    }

    pub fn focused(&self) -> Field {
        match self {
            Form::Provider { focus, .. } => *focus,
            Form::Secret { .. } => Field::Secret,
        }
    }

    pub fn focus(&mut self, field: Field) {
        if let Form::Provider { focus, discovery, .. } = self
            && field != Field::Secret
            && (field != Field::Search || discovery.is_some())
        {
            *focus = field;
        }
    }

    pub fn next_field(&mut self, back: bool) {
        const FIELDS: [Field; 4] = [Field::Name, Field::Url, Field::Key, Field::Models];
        const WITH_SEARCH: [Field; 5] = [Field::Name, Field::Url, Field::Key, Field::Models, Field::Search];
        if let Form::Provider { focus, discovery, .. } = self {
            let order: &[Field] = if discovery.is_some() { &WITH_SEARCH } else { &FIELDS };
            let at = order.iter().position(|f| f == focus).unwrap_or(0);
            *focus = order[if back { (at + order.len() - 1) % order.len() } else { (at + 1) % order.len() }];
        }
    }

    /// The fetched list under this form, while there is one.
    pub fn discovery(&self) -> Option<&Discovery> {
        match self {
            Form::Provider { discovery, .. } => discovery.as_ref(),
            Form::Secret { .. } => None,
        }
    }

    pub fn discovery_mut(&mut self) -> Option<&mut Discovery> {
        match self {
            Form::Provider { discovery, .. } => discovery.as_mut(),
            Form::Secret { .. } => None,
        }
    }

    /// Hide the fetched list; True when there was one. The caret returns to
    /// the Models text, which is still the allowlist.
    pub fn close_discovery(&mut self) -> bool {
        let Form::Provider { discovery, focus, .. } = self else { return false };
        if discovery.take().is_none() {
            return false;
        }
        if *focus == Field::Search {
            *focus = Field::Models;
        }
        true
    }

    /// Add `id` to the Models text, or take it out when it is already there.
    pub fn toggle_model(&mut self, id: &str) {
        if let Form::Provider { models, .. } = self {
            let toggled = toggle_model(models.text(), id);
            models.select_all();
            models.insert(&toggled);
        }
    }

    pub fn editor(&self) -> &Editor {
        match self {
            Form::Provider { name, url, key, models, focus, discovery, .. } => match focus {
                Field::Name => name,
                Field::Url => url,
                Field::Key => key,
                Field::Models | Field::Secret => models,
                Field::Search => discovery.as_ref().map_or(models, |d| &d.query),
            },
            Form::Secret { value, .. } => value,
        }
    }

    pub fn editor_mut(&mut self) -> &mut Editor {
        match self {
            Form::Provider { name, url, key, models, focus, discovery, .. } => match focus {
                Field::Name => name,
                Field::Url => url,
                Field::Key => key,
                Field::Models | Field::Secret => models,
                Field::Search => discovery.as_mut().map_or(models, |d| &mut d.query),
            },
            Form::Secret { value, .. } => value,
        }
    }

    /// The provider the fields describe, and the name it is replacing.
    pub fn provider(&self) -> Option<(Option<&str>, Provider)> {
        let Form::Provider { editing, name, url, key, models, .. } = self else { return None };
        let provider = Provider {
            name: name.text().trim().to_string(),
            url: url.text().trim().to_string(),
            secret: key_name(name.text().trim(), key.text()),
            models: model_list(models.text()),
        };
        Some((editing.as_deref(), provider))
    }
}

/// The key field as a credential name: empty means `<NAME>_API_KEY`, `none`
/// means a keyless server, anything else is the name as typed.
fn key_name(provider: &str, typed: &str) -> Option<String> {
    match typed.trim() {
        "" => Some(format!("{}_API_KEY", provider.to_ascii_uppercase().replace('-', "_"))),
        t if t.eq_ignore_ascii_case("none") => None,
        t => Some(t.to_string()),
    }
}

/// Models one per line (commas also split), trimmed, without repeats.
fn model_list(text: &str) -> Vec<String> {
    let mut models: Vec<String> = Vec::new();
    for model in text.split(['\n', ',']).map(str::trim).filter(|m| !m.is_empty()) {
        if !models.iter().any(|m| m == model) {
            models.push(model.to_string());
        }
    }
    models
}

/// Whether `id` is in the Models text.
pub fn lists(text: &str, id: &str) -> bool {
    model_list(text).iter().any(|model| model == id)
}

/// The Models text with `id` added at the end, or removed when present. Ids
/// typed by hand are kept as they are.
pub fn toggle_model(text: &str, id: &str) -> String {
    let mut models = model_list(text);
    match models.iter().position(|model| model == id) {
        Some(at) => {
            models.remove(at);
        }
        None => models.push(id.to_string()),
    }
    models.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_toggles_in_the_allowlist_without_touching_manual_ids() {
        let typed = "hand-typed\nqwen3-32b";
        assert!(lists(typed, "qwen3-32b"));
        assert!(!lists(typed, "llama-4"));
        let added = toggle_model(typed, "llama-4");
        assert_eq!(added, "hand-typed\nqwen3-32b\nllama-4");
        assert_eq!(toggle_model(&added, "qwen3-32b"), "hand-typed\nllama-4");
        assert_eq!(toggle_model("", "m"), "m");
    }
}
