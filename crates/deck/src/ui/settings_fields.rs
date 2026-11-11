//! The settings panel's one open form (ADR 0012 §4): a provider's four
//! fields or a secret's one, which field has the caret, and how the fields
//! read back as a [`Provider`].
//!
//! The design choice: each field is a full composer [`Editor`], so typing,
//! IME and moves behave as in the prompt; the form only says which editor
//! the input handler should use. Field rules (the key name's two shorthands,
//! models one per line) live here, next to the fields they read.

use super::editor::Editor;
use deck_runtime::settings::Provider;

/// A field of the open form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Url,
    Key,
    Models,
    Secret,
}

/// The one open form: a provider to add or edit, or a secret to set.
#[expect(clippy::large_enum_variant, reason = "one form exists at a time, so its size never adds up")]
pub enum Form {
    Provider { editing: Option<String>, name: Editor, url: Editor, key: Editor, models: Editor, focus: Field },
    Secret { name: String, value: Editor },
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
        }
    }

    pub fn focused(&self) -> Field {
        match self {
            Form::Provider { focus, .. } => *focus,
            Form::Secret { .. } => Field::Secret,
        }
    }

    pub fn focus(&mut self, field: Field) {
        if let Form::Provider { focus, .. } = self
            && field != Field::Secret
        {
            *focus = field;
        }
    }

    pub fn next_field(&mut self, back: bool) {
        const ORDER: [Field; 4] = [Field::Name, Field::Url, Field::Key, Field::Models];
        if let Form::Provider { focus, .. } = self {
            let at = ORDER.iter().position(|f| f == focus).unwrap_or(0);
            *focus = ORDER[if back { (at + ORDER.len() - 1) % ORDER.len() } else { (at + 1) % ORDER.len() }];
        }
    }

    pub fn editor(&self) -> &Editor {
        match self {
            Form::Provider { name, url, key, models, focus, .. } => match focus {
                Field::Name => name,
                Field::Url => url,
                Field::Key => key,
                Field::Models | Field::Secret => models,
            },
            Form::Secret { value, .. } => value,
        }
    }

    pub fn editor_mut(&mut self) -> &mut Editor {
        match self {
            Form::Provider { name, url, key, models, focus, .. } => match focus {
                Field::Name => name,
                Field::Url => url,
                Field::Key => key,
                Field::Models | Field::Secret => models,
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
