//! Revoked fingerprints: `credentials/revoked.json` under the data root.
//!
//! The design choice: a revocation names a value (by fingerprint), not a
//! name, and is permanent. The name serves again as soon as a new value is
//! set; the revoked value never does. Each revocation is written and fsynced
//! before the caller is answered, so a reply means it is on disk.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revocation {
    pub name: String,
    pub fingerprint: String,
    /// Seconds since the Unix epoch.
    pub at: u64,
}

pub struct Revoked {
    path: PathBuf,
    list: Mutex<Vec<Revocation>>,
}

impl Revoked {
    /// Load the list under `data`. A missing file is an empty list; an
    /// unreadable one is an error, never an empty list, because forgetting a
    /// revocation would serve a leaked value again.
    pub fn open(data: &Path) -> Result<Self> {
        let path = data.join("credentials").join("revoked.json");
        let list = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path, list: Mutex::new(list) })
    }

    pub fn is_revoked(&self, fingerprint: &str) -> bool {
        self.lock().iter().any(|revocation| revocation.fingerprint == fingerprint)
    }

    /// The revocations for any of `names`, for a gateway's watch snapshot.
    pub fn for_names(&self, names: &[String]) -> Vec<Revocation> {
        self.lock().iter().filter(|revocation| names.contains(&revocation.name)).cloned().collect()
    }

    pub fn all(&self) -> Vec<Revocation> {
        self.lock().clone()
    }

    /// Record `fingerprint` as revoked. Revoking an already revoked value
    /// returns the first record and writes nothing.
    pub fn revoke(&self, name: &str, fingerprint: &str) -> Result<Revocation> {
        let mut list = self.lock();
        if let Some(existing) = list.iter().find(|revocation| revocation.fingerprint == fingerprint) {
            return Ok(existing.clone());
        }
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let revocation = Revocation { name: name.to_string(), fingerprint: fingerprint.to_string(), at };
        let mut next = list.clone();
        next.push(revocation.clone());
        write(&self.path, &next)?;
        *list = next;
        Ok(revocation)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Revocation>> {
        self.list.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn write(path: &Path, list: &[Revocation]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let directory = path.parent().context("revoked.json has a directory")?;
    std::fs::create_dir_all(directory)?;
    let temp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&temp)?;
    file.write_all(&serde_json::to_vec_pretty(list)?)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    std::fs::rename(&temp, path)?;
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}
