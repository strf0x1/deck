//! Deck's settings: the named providers a VM task may be created with, and
//! the default `provider/model` (ADR 0012). `deckd` owns them; `file` keeps
//! them under the data root.
//!
//! The design choice: settings hold names, never values. A provider names its
//! key the way the credential store does, so the file can be shown, logged
//! and diffed. What a task actually gets is a [`Choice`], resolved once at
//! create and recorded with it, so a later edit never widens a running task.

pub mod file;

use crate::boundary;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// The settings a window shows and edits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// `<provider>/<id>`, split at the first `/`.
    pub default: String,
    pub providers: Vec<Provider>,
}

/// One OpenAI-compatible endpoint, its key's name and the models a task on
/// it may use without asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provider {
    pub name: String,
    /// The base URL including its version segment, as `CONSTRUCT_BASE_URL`.
    pub url: String,
    /// A credential name, or `None` for a keyless server.
    pub secret: Option<String>,
    pub models: Vec<String>,
}

/// What one task is created with: its provider and the model it starts on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub provider: Provider,
    pub model: String,
}

/// A task spec's model: what a window asked for, until the backend pins it.
/// Only [`Pick::Choice`] is ever recorded by a VM task; a local task records
/// none (ADR 0012 §2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pick {
    /// `<provider>/<id>` from the window, resolved at create.
    Selector(String),
    /// The provider and model the task was created with.
    Choice(Box<Choice>),
}

/// The two variables `deckd` read before settings existed. They define the
/// implicit provider, and override a settings file (logged at start).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    pub base_url: Option<String>,
    pub model: Option<String>,
}

impl Env {
    pub fn from_process() -> Self {
        let var = |key| std::env::var(key).ok().filter(|v| !v.is_empty());
        Self { base_url: var("CONSTRUCT_BASE_URL"), model: var("CONSTRUCT_MODEL") }
    }
}

/// The provider name used when no settings file exists.
pub const IMPLICIT: &str = "default";

impl Settings {
    /// What `deckd` behaved as before settings: one provider from the
    /// environment, or Construct's own defaults.
    pub fn implicit(env: &Env) -> Self {
        let model = env.model.clone().unwrap_or_else(|| boundary::DEFAULT_MODEL.into());
        Self {
            default: format!("{IMPLICIT}/{model}"),
            providers: vec![Provider {
                name: IMPLICIT.into(),
                url: env.base_url.clone().unwrap_or_else(|| boundary::DEFAULT_UPSTREAM.into()),
                secret: Some(boundary::SECRET.into()),
                models: vec![model],
            }],
        }
    }

    /// Refuse anything `deckd` would not create a task from.
    pub fn validate(&self) -> Result<()> {
        if self.providers.is_empty() {
            bail!("at least one provider is needed");
        }
        for (i, provider) in self.providers.iter().enumerate() {
            provider.validate()?;
            if self.providers[..i].iter().any(|p| p.name == provider.name) {
                bail!("two providers are named {:?}", provider.name);
            }
        }
        self.resolve(None).map(|_| ())
    }

    pub fn provider(&self, name: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.name == name)
    }

    /// The task's provider and model: `selector` (`<provider>/<id>`) when
    /// given, else the default. Either must name a listed model.
    pub fn resolve(&self, selector: Option<&str>) -> Result<Choice> {
        let selector = selector.unwrap_or(&self.default);
        let Some((name, model)) = selector.split_once('/') else {
            bail!("{selector:?} is not <provider>/<model>");
        };
        let Some(provider) = self.provider(name) else {
            bail!("no provider is named {name:?}");
        };
        if !provider.models.iter().any(|m| m == model) {
            bail!("provider {name:?} does not list the model {model:?}");
        }
        Ok(Choice { provider: provider.clone(), model: model.into() })
    }

    /// What a task runs with: its recorded choice, else `pick`'s selector or
    /// the default resolved now (a record from before settings existed).
    pub fn pin(&self, pick: Option<&Pick>) -> Result<Choice> {
        match pick {
            Some(Pick::Choice(choice)) => Ok(choice.as_ref().clone()),
            Some(Pick::Selector(selector)) => self.resolve(Some(selector)),
            None => self.resolve(None),
        }
    }

    /// These settings with the environment applied: `CONSTRUCT_BASE_URL`
    /// replaces the default provider's url, `CONSTRUCT_MODEL` the default
    /// model (added to that provider's list if missing, as it was always
    /// allowed). Returns what was overridden, for the start-up log and the
    /// panel.
    pub fn overridden(mut self, env: &Env) -> (Self, Vec<String>) {
        let mut notes = Vec::new();
        let Some((name, _)) = self.default.split_once('/') else { return (self, notes) };
        let name = name.to_owned();
        let Some(provider) = self.providers.iter_mut().find(|p| p.name == name) else { return (self, notes) };
        if let Some(url) = &env.base_url
            && *url != provider.url
        {
            provider.url = url.clone();
            notes.push(format!("CONSTRUCT_BASE_URL overrides provider {name:?}'s url"));
        }
        if let Some(model) = &env.model {
            if !provider.models.contains(model) {
                provider.models.push(model.clone());
            }
            let default = format!("{name}/{model}");
            if default != self.default {
                self.default = default;
                notes.push("CONSTRUCT_MODEL overrides the default model".into());
            }
        }
        (self, notes)
    }
}

impl Provider {
    fn validate(&self) -> Result<()> {
        let name = &self.name;
        let mut bytes = name.bytes();
        let shaped = matches!(bytes.next(), Some(b'a'..=b'z'))
            && name.len() <= 32
            && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !shaped {
            bail!("provider name {name:?} must be lowercase letters, digits and '-', starting with a letter");
        }
        valid_url(&self.url).map_err(|why| anyhow::anyhow!("provider {name:?}: {why}"))?;
        if let Some(secret) = &self.secret
            && !crate::credentials::valid_name(secret)
        {
            bail!("provider {name:?}: key name {secret:?} must look like OPENROUTER_API_KEY");
        }
        if self.models.is_empty() {
            bail!("provider {name:?} lists no models");
        }
        if let Some(bad) = self.models.iter().find(|m| m.is_empty() || m.chars().any(char::is_whitespace)) {
            bail!("provider {name:?}: model {bad:?} is empty or has whitespace");
        }
        Ok(())
    }
}

/// An `http` or `https` URL with a host, no whitespace, no credentials and no
/// query or fragment: the gateway appends `/chat/completions` to it.
fn valid_url(url: &str) -> std::result::Result<(), String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| format!("{url:?} must start with http:// or https://"))?;
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("{url:?} has no host or contains whitespace"));
    }
    if host.contains('@') || url.contains(['?', '#']) {
        return Err(format!("{url:?} must not carry credentials, a query or a fragment"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two() -> Settings {
        Settings {
            default: "openrouter/openrouter/free".into(),
            providers: vec![
                Provider {
                    name: "openrouter".into(),
                    url: "https://openrouter.ai/api/v1".into(),
                    secret: Some("OPENROUTER_API_KEY".into()),
                    models: vec!["openrouter/free".into(), "anthropic/claude-sonnet-5-5".into()],
                },
                Provider {
                    name: "local".into(),
                    url: "http://10.0.0.5:8000/v1".into(),
                    secret: None,
                    models: vec!["qwen3-32b".into()],
                },
            ],
        }
    }

    #[test]
    fn the_implicit_provider_is_what_deckd_did_before_settings() {
        let settings = Settings::implicit(&Env::default());
        settings.validate().unwrap();
        let choice = settings.resolve(None).unwrap();
        assert_eq!(choice.provider.url, boundary::DEFAULT_UPSTREAM);
        assert_eq!(choice.provider.secret.as_deref(), Some(boundary::SECRET));
        assert_eq!(choice.model, boundary::DEFAULT_MODEL);

        let env = Env { base_url: Some("http://127.0.0.1:9/v1".into()), model: Some("mock".into()) };
        let choice = Settings::implicit(&env).resolve(None).unwrap();
        assert_eq!((choice.provider.url.as_str(), choice.model.as_str()), ("http://127.0.0.1:9/v1", "mock"));
    }

    #[test]
    fn a_selector_splits_at_the_first_slash() {
        let settings = two();
        settings.validate().unwrap();
        let choice = settings.resolve(None).unwrap();
        assert_eq!((choice.provider.name.as_str(), choice.model.as_str()), ("openrouter", "openrouter/free"));
        let choice = settings.resolve(Some("local/qwen3-32b")).unwrap();
        assert_eq!(choice.provider.secret, None);
        assert!(settings.resolve(Some("local/unlisted")).is_err());
        assert!(settings.resolve(Some("nowhere/qwen3-32b")).is_err());
        assert!(settings.resolve(Some("qwen3-32b")).is_err());
    }

    #[test]
    fn a_recorded_choice_wins_over_the_settings() {
        let settings = two();
        let recorded = settings.resolve(Some("local/qwen3-32b")).unwrap();
        let mut changed = two();
        changed.providers.retain(|p| p.name != "local");
        changed.validate().unwrap();
        // The provider is gone from the settings; the task keeps what it had.
        assert_eq!(changed.pin(Some(&Pick::Choice(Box::new(recorded.clone())))).unwrap(), recorded);
        assert_eq!(settings.pin(Some(&Pick::Selector("local/qwen3-32b".into()))).unwrap(), recorded);
        assert_eq!(settings.pin(None).unwrap(), settings.resolve(None).unwrap());
        assert!(changed.pin(Some(&Pick::Selector("local/qwen3-32b".into()))).is_err());
    }

    #[test]
    fn validation_refuses_what_a_task_could_not_use() {
        let refused = |edit: fn(&mut Settings)| {
            let mut settings = two();
            edit(&mut settings);
            settings.validate().unwrap_err().to_string()
        };
        assert!(refused(|s| s.providers.clear()).contains("at least one"));
        assert!(refused(|s| s.providers[1].name = "openrouter".into()).contains("two providers"));
        assert!(refused(|s| s.providers[1].name = "Local".into()).contains("lowercase"));
        assert!(refused(|s| s.providers[1].url = "ftp://x/v1".into()).contains("http"));
        assert!(refused(|s| s.providers[1].url = "https://u:p@x/v1".into()).contains("credentials"));
        assert!(refused(|s| s.providers[1].url = "https:///v1".into()).contains("no host"));
        assert!(refused(|s| s.providers[1].secret = Some("lower".into())).contains("key name"));
        assert!(refused(|s| s.providers[1].models.clear()).contains("no models"));
        assert!(refused(|s| s.providers[1].models = vec!["a b".into()]).contains("whitespace"));
        assert!(refused(|s| s.default = "local/missing".into()).contains("does not list"));
    }

    #[test]
    fn the_environment_overrides_the_default_provider_and_says_so() {
        let env = Env { base_url: Some("http://127.0.0.1:9/v1".into()), model: Some("mock".into()) };
        let (settings, notes) = two().overridden(&env);
        assert_eq!(notes.len(), 2);
        let choice = settings.resolve(None).unwrap();
        assert_eq!((choice.provider.url.as_str(), choice.model.as_str()), ("http://127.0.0.1:9/v1", "mock"));
        assert!(choice.provider.models.contains(&"openrouter/free".to_string()));
        assert_eq!(settings.provider("local"), two().provider("local"));

        let (same, notes) = two().overridden(&Env::default());
        assert_eq!((same, notes.len()), (two(), 0));
    }
}
