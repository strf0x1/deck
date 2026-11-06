//! Grant and project-rule files under the data root (ADR 0004 §5).
//!
//! The design choice: both files are atomic replacements, written before the
//! decision is reported, and a missing file is an empty policy, not an error.
//! Pending items and leases never reach this module.

use super::Grant;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub(super) fn load_projects(data: &Path) -> Result<(Vec<Grant>, u64)> {
    let path = projects_file(data);
    match fs::read(&path) {
        Ok(bytes) => {
            let file: Projects =
                serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
            Ok((file.rules, file.version))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((vec![], 0)),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

#[derive(Serialize, Deserialize)]
struct Projects {
    version: u64,
    rules: Vec<Grant>,
}

pub(super) fn save_projects(data: &Path, rules: &[Grant], version: u64) -> Result<()> {
    write_json(&projects_file(data), &Projects { version, rules: rules.to_vec() })
}

pub(super) fn save_task(data: &Path, task: &str, grants: &[Grant]) -> Result<()> {
    let path = task_grants(data, task);
    // A retry allowance lives only in memory. Revoking another grant must
    // not turn the remaining allow-once items into durable policy.
    let grants: Vec<_> =
        grants.iter().filter(|grant| matches!(grant.scope, super::Scope::Timed | super::Scope::Task)).collect();
    if grants.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => {
                let directory = path.parent().context("a task grant file has a directory")?;
                File::open(directory)?.sync_all().context("syncing the removed task grant file")
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
        }
    } else {
        write_json(&path, &grants)
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let temp = path.with_extension("tmp");
    let mut file = File::create(&temp).with_context(|| format!("creating {}", temp.display()))?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    fs::rename(&temp, path).with_context(|| format!("renaming into {}", path.display()))?;
    if let Some(dir) = path.parent()
        && let Ok(dir) = File::open(dir)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn projects_file(data: &Path) -> PathBuf {
    data.join("approvals").join("projects.json")
}

pub(super) fn task_grants(data: &Path, task: &str) -> PathBuf {
    data.join("tasks").join(task).join("grants.json")
}

/// Append the removed grants before the control caller is answered. The
/// policy file has already been narrowed, so an audit failure cannot keep a
/// revoked allowance active; it does prevent a successful control reply.
pub(super) fn audit_revoked(data: &Path, grants: &[Grant], at_ms: u64) -> Result<()> {
    if grants.is_empty() {
        return Ok(());
    }
    let directory = data.join("approvals");
    fs::create_dir_all(&directory)?;
    let mut file = OpenOptions::new().create(true).append(true).open(directory.join("audit.jsonl"))?;
    for grant in grants {
        let event = serde_json::json!({ "event": "grant_revoked", "at_ms": at_ms, "grant": grant });
        serde_json::to_writer(&mut file, &event)?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    File::open(directory)?.sync_all()?;
    Ok(())
}
