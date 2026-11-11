//! What the settings panel derives from `deckd`'s replies: one row per
//! provider with its key's state, one per secret with who uses it, and the
//! provider form's field rules. Pure functions, used by
//! [`SettingsPanel`](super::settings::SettingsPanel).
//!
//! The design choice: a key's state is read from the secret listing by name
//! only, never by asking for a value, so the rows say "set", "not set" or
//! "revoked" without the window ever learning more than a fingerprint.

use deck_runtime::broker::Listed;
use deck_runtime::settings::Settings as Stored;

/// A provider's key as its row shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Set(String),
    Revoked(String),
    NotSet,
    Keyless,
    /// The secret listing has not arrived, or could not be read.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRow {
    pub name: String,
    pub url: String,
    pub secret: Option<String>,
    pub key: Key,
    pub models: usize,
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRow {
    pub name: String,
    pub listed: Option<Listed>,
    /// The providers whose key this is; empty means unused.
    pub providers: Vec<String>,
}

/// One row per provider. `secrets` is `None` until the listing arrives.
pub fn providers(settings: &Stored, secrets: Option<&[Listed]>) -> Vec<ProviderRow> {
    let listed = |name: &str| secrets?.iter().find(|l| l.name == name && l.fingerprint.is_some());
    settings
        .providers
        .iter()
        .map(|p| ProviderRow {
            name: p.name.clone(),
            url: p.url.clone(),
            secret: p.secret.clone(),
            key: match &p.secret {
                None => Key::Keyless,
                Some(_) if secrets.is_none() => Key::Unknown,
                Some(name) => match listed(name).and_then(|l| l.fingerprint.clone().map(|f| (f, l.revoked))) {
                    Some((f, true)) => Key::Revoked(f),
                    Some((f, false)) => Key::Set(f),
                    None => Key::NotSet,
                },
            },
            models: p.models.len(),
            default: settings.default.split_once('/').is_some_and(|(name, _)| name == p.name),
        })
        .collect()
}

/// Every stored name, and every name a provider needs that is not stored.
pub fn secrets(settings: Option<&Stored>, secrets: Option<&[Listed]>) -> Vec<SecretRow> {
    let providers = settings.map(|s| s.providers.as_slice()).unwrap_or_default();
    let stored = secrets.unwrap_or_default();
    let mut names: Vec<String> = stored.iter().map(|l| l.name.clone()).collect();
    names.extend(providers.iter().filter_map(|p| p.secret.clone()));
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| SecretRow {
            listed: stored.iter().find(|l| l.name == name).cloned(),
            providers: providers
                .iter()
                .filter(|p| p.secret.as_deref() == Some(name.as_str()))
                .map(|p| p.name.clone())
                .collect(),
            name,
        })
        .collect()
}

/// Keep the default on a listed model: a removed provider or model moves it
/// to the first model of the first provider.
pub fn repair_default(settings: &mut Stored) {
    if settings.resolve(None).is_err()
        && let Some(first) = settings.providers.first()
        && let Some(model) = first.models.first()
    {
        settings.default = format!("{}/{model}", first.name);
    }
}
