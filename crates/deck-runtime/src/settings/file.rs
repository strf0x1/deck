//! `<data>/settings.json`: the one copy of Deck's settings, written only by
//! `deckd` (ADR 0012 §1).
//!
//! The design choice: every write names the revision it replaces. The file
//! carries a counter; a save from a window that read an older one is refused
//! with what is there now, so two windows never silently overwrite each
//! other. The file is re-read on every call, never cached, so what `deckd`
//! creates a task from is always what is on disk.

use super::{Env, Settings};
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const VERSION: u32 = 1;
const NAME: &str = "settings.json";

#[derive(Serialize, Deserialize)]
struct OnDisk {
    version: u32,
    revision: u64,
    #[serde(flatten)]
    settings: Settings,
}

/// What a window reads: the stored settings (or the implicit provider), the
/// revision to save against, and what the environment overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// 0 while no file exists.
    pub revision: u64,
    /// False while `deckd` is running on the implicit provider.
    pub stored: bool,
    pub settings: Settings,
    /// Notes for each environment variable that changes what a task gets.
    pub overrides: Vec<String>,
}

/// The reply to a save.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Saved {
    Saved(Snapshot),
    /// Someone saved since `revision` was read; this is what is there now.
    Stale(Snapshot),
}

#[derive(Clone)]
pub struct Store {
    path: PathBuf,
    env: Env,
}

impl Store {
    /// Refuse a file `deckd` could not create a task from, rather than start
    /// and later rewrite it.
    pub fn open(data: &Path, env: Env) -> Result<Self> {
        let store = Self { path: data.join(NAME), env };
        store.read()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        let (revision, stored) = match self.read()? {
            Some(disk) => (disk.revision, Some(disk.settings)),
            None => (0, None),
        };
        let overrides = match &stored {
            Some(settings) => settings.clone().overridden(&self.env).1,
            None => Vec::new(),
        };
        Ok(Snapshot {
            revision,
            stored: stored.is_some(),
            settings: stored.unwrap_or_else(|| Settings::implicit(&self.env)),
            overrides,
        })
    }

    /// What a task is created from: the file with the environment applied,
    /// or the implicit provider.
    pub fn effective(&self) -> Result<Settings> {
        Ok(match self.read()? {
            Some(disk) => disk.settings.overridden(&self.env).0,
            None => Settings::implicit(&self.env),
        })
    }

    /// Replace the settings if `revision` is still current.
    pub fn replace(&self, settings: Settings, revision: u64) -> Result<Saved> {
        settings.validate()?;
        let current = self.snapshot()?;
        if current.revision != revision {
            return Ok(Saved::Stale(current));
        }
        let disk = OnDisk { version: VERSION, revision: revision + 1, settings };
        let mut bytes = serde_json::to_vec_pretty(&disk)?;
        bytes.push(b'\n');
        crate::vm::write_atomic(&self.path, &bytes).with_context(|| format!("writing {}", self.path.display()))?;
        Ok(Saved::Saved(self.snapshot()?))
    }

    fn read(&self) -> Result<Option<OnDisk>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.path.display())),
        };
        let at = || self.path.display().to_string();
        let version: serde_json::Value = serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", at()))?;
        if version.get("version").and_then(serde_json::Value::as_u64) != Some(u64::from(VERSION)) {
            bail!("{} is not version {VERSION} settings; deckd will not rewrite it", at());
        }
        let disk: OnDisk = serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", at()))?;
        disk.settings.validate().with_context(|| format!("{} is not valid", at()))?;
        Ok(Some(disk))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Provider;

    fn data(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deck-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn local() -> Settings {
        Settings {
            default: "local/qwen3-32b".into(),
            providers: vec![Provider {
                name: "local".into(),
                url: "http://10.0.0.5:8000/v1".into(),
                secret: None,
                models: vec!["qwen3-32b".into()],
            }],
        }
    }

    #[test]
    fn no_file_is_the_implicit_provider_at_revision_zero() {
        let store = Store::open(&data("implicit"), Env::default()).unwrap();
        let snapshot = store.snapshot().unwrap();
        assert_eq!((snapshot.revision, snapshot.stored), (0, false));
        assert_eq!(snapshot.settings, Settings::implicit(&Env::default()));
        assert_eq!(store.effective().unwrap(), snapshot.settings);
    }

    #[test]
    fn a_save_names_the_revision_it_replaces() {
        let dir = data("revision");
        let store = Store::open(&dir, Env::default()).unwrap();
        let Saved::Saved(first) = store.replace(local(), 0).unwrap() else { panic!("first save was stale") };
        assert_eq!((first.revision, first.stored), (1, true));
        assert_eq!(first.settings, local());

        // A second window that read revision 0 is told what is there now.
        let Saved::Stale(now) = store.replace(Settings::implicit(&Env::default()), 0).unwrap() else {
            panic!("a stale save was accepted")
        };
        assert_eq!(now, first);
        assert!(matches!(store.replace(local(), 1).unwrap(), Saved::Saved(s) if s.revision == 2));

        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(store.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // A restart reads the same file.
        assert_eq!(Store::open(&dir, Env::default()).unwrap().snapshot().unwrap().revision, 2);
    }

    #[test]
    fn an_invalid_save_writes_nothing() {
        let store = Store::open(&data("invalid"), Env::default()).unwrap();
        let mut bad = local();
        bad.default = "local/missing".into();
        assert!(store.replace(bad, 0).is_err());
        assert!(!store.path().exists());
    }

    #[test]
    fn an_unknown_version_or_bad_file_is_refused_at_open() {
        let dir = data("version");
        std::fs::write(dir.join(NAME), r#"{"version":2,"revision":1}"#).unwrap();
        let error = Store::open(&dir, Env::default()).err().unwrap().to_string();
        assert!(error.contains("not version 1"), "{error}");

        std::fs::write(dir.join(NAME), r#"{"version":1,"revision":1,"default":"x/y","providers":[]}"#).unwrap();
        assert!(Store::open(&dir, Env::default()).is_err());
    }

    #[test]
    fn the_environment_overrides_a_file_and_is_reported() {
        let dir = data("override");
        Store::open(&dir, Env::default()).unwrap().replace(local(), 0).unwrap();
        let env = Env { base_url: Some("http://127.0.0.1:9/v1".into()), model: None };
        let store = Store::open(&dir, env).unwrap();
        let snapshot = store.snapshot().unwrap();
        // The window edits what is stored, and is told what differs.
        assert_eq!(snapshot.settings, local());
        assert_eq!(snapshot.overrides.len(), 1);
        assert_eq!(store.effective().unwrap().providers[0].url, "http://127.0.0.1:9/v1");
    }
}
