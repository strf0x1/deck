//! The file store: a JSON object of name to value at the path
//! `DECK_CREDENTIALS` names. The headless, CI and test store.
//!
//! The design choice: the permission check runs on every read, not only at
//! start, so a file that later gains group or other bits stops serving
//! values instead of serving them from a file other users can read. Writes
//! replace the file atomically at 0600.

use super::Absent;
use anyhow::{Context as _, Result, bail};
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

type Map = BTreeMap<String, String>;

/// Refuse a path that is not a regular file owned by this user with no group
/// or other permission bits.
pub fn check(path: &Path) -> Result<()> {
    let metadata =
        std::fs::symlink_metadata(path).with_context(|| format!("reading the credential file {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("the credential file {} is not a regular file", path.display());
    }
    // SAFETY: getuid has no arguments or pointers.
    let uid = unsafe { libc::getuid() };
    if metadata.uid() != uid {
        bail!("the credential file {} is not owned by this user", path.display());
    }
    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "the credential file {} has mode {mode:o}; it must have no group or other bits (chmod 600)",
            path.display()
        );
    }
    Ok(())
}

fn load(path: &Path) -> Result<Map> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        _ => check(path)?,
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading the credential file {}", path.display()))?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    // The parse error is not shown: serde quotes the offending input, and the
    // offending input is a credential file.
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("the credential file {} is not a JSON object of strings", path.display()))
}

fn store(path: &Path, map: &Map) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = std::path::PathBuf::from(temp);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("writing {}", temp.display()))?;
    file.write_all(&serde_json::to_vec_pretty(map)?)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    std::fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

pub fn read(path: &Path, name: &str) -> Result<String, Absent> {
    let map = load(path).map_err(|error| Absent::Unavailable(format!("{error:#}")))?;
    map.get(name).cloned().ok_or(Absent::Missing)
}

pub fn set(path: &Path, name: &str, value: &str) -> Result<()> {
    let mut map = load(path)?;
    map.insert(name.to_string(), value.to_string());
    store(path, &map)
}

pub fn delete(path: &Path, name: &str) -> Result<bool> {
    let mut map = load(path)?;
    let removed = map.remove(name).is_some();
    if removed {
        store(path, &map)?;
    }
    Ok(removed)
}

pub fn names(path: &Path) -> Result<Vec<String>> {
    Ok(load(path)?.into_keys().collect())
}
