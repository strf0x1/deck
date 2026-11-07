//! The host-side import view for a VM task (ADR 0008 §4).
//!
//! The design choice: the guest never sees a file the project's git ignores.
//! Filtering inside the guest would be too late — the file has already
//! crossed the virtiofs share — so the runtime stages a filtered view on the
//! host and shares that. A git checkout is staged by walking it and skipping
//! the paths `git ls-files --others --ignored --exclude-standard --directory`
//! names; anything else is copied whole. Files are hard-linked when the
//! staging directory is on the same filesystem, so a large checkout costs
//! inodes, not bytes; a cross-device link falls back to a copy.

use anyhow::{Context as _, Result, bail};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Stage `project` at `target`, leaving out the files its git ignores.
pub fn stage(project: &Path, target: &Path) -> Result<()> {
    if target.starts_with(project) {
        bail!("the staging directory {} is inside the project {}", target.display(), project.display());
    }
    if target.exists() {
        std::fs::remove_dir_all(target).with_context(|| format!("clearing {}", target.display()))?;
    }
    std::fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
    let ignored = ignored(project)?;
    copy_tree(project, target, ignored.as_ref())
}

/// The paths `project`'s git ignores, relative to `project`, or `None` when
/// `project` is not a checkout. A checkout whose ignored list cannot be read
/// is an error: copying it whole would put the ignored files in the guest.
fn ignored(project: &Path) -> Result<Option<HashSet<PathBuf>>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "-z"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) if project.join(".git").exists() => {
            return Err(error).with_context(|| format!("listing ignored files in {}", project.display()));
        }
        Err(_) => return Ok(None),
    };
    if !output.status.success() {
        if project.join(".git").exists() {
            bail!("listing ignored files in {}: {}", project.display(), String::from_utf8_lossy(&output.stderr).trim());
        }
        return Ok(None);
    }
    let mut ignored = HashSet::new();
    for path in output.stdout.split(|byte| *byte == 0).filter(|path| !path.is_empty()) {
        // `--directory` names an ignored directory with a trailing slash.
        let path = path.strip_suffix(b"/").unwrap_or(path);
        ignored.insert(PathBuf::from(OsStr::from_bytes(path)));
    }
    Ok(Some(ignored))
}

/// Copy `project` into `target`, skipping `ignored` (relative paths). A
/// directory in `ignored` is skipped whole, which is what `--directory`
/// promises: every path under it is ignored too.
fn copy_tree(project: &Path, target: &Path, ignored: Option<&HashSet<PathBuf>>) -> Result<()> {
    let empty = HashSet::new();
    let ignored = ignored.unwrap_or(&empty);
    let mut stack = vec![PathBuf::new()];
    while let Some(relative) = stack.pop() {
        let source = project.join(&relative);
        let entries = std::fs::read_dir(&source).with_context(|| format!("reading {}", source.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", source.display()))?;
            let child = relative.join(entry.file_name());
            if ignored.contains(&child) {
                continue;
            }
            let source = entry.path();
            let destination = target.join(&child);
            let metadata =
                std::fs::symlink_metadata(&source).with_context(|| format!("reading {}", source.display()))?;
            let file_type = metadata.file_type();
            if file_type.is_dir() {
                std::fs::create_dir(&destination).with_context(|| format!("creating {}", destination.display()))?;
                stack.push(child);
            } else if file_type.is_symlink() {
                let link = std::fs::read_link(&source).with_context(|| format!("reading {}", source.display()))?;
                std::os::unix::fs::symlink(&link, &destination)
                    .with_context(|| format!("linking {}", destination.display()))?;
            } else if file_type.is_file() && std::fs::hard_link(&source, &destination).is_err() {
                std::fs::copy(&source, &destination).with_context(|| format!("copying {}", source.display()))?;
            }
            // A socket, fifo or device is not project content.
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deck-import-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn checkout(name: &str) -> PathBuf {
        let dir = scratch(name);
        git(&dir, &["init", "--quiet"]);
        git(&dir, &["config", "user.name", "Ada Lovelace"]);
        git(&dir, &["config", "user.email", "ada@example.com"]);
        std::fs::write(dir.join(".gitignore"), "node_modules/\n*.env\n").unwrap();
        std::fs::write(dir.join("tracked.txt"), "tracked\n").unwrap();
        std::fs::write(dir.join("untracked.txt"), "untracked\n").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::write(dir.join("node_modules/pkg/index.js"), "ignored\n").unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/keep.txt"), "keep\n").unwrap();
        std::fs::write(dir.join("src/.env"), "SECRET=2\n").unwrap();
        git(&dir, &["add", ".gitignore", "tracked.txt", "src/keep.txt"]);
        git(&dir, &["commit", "--quiet", "-m", "init"]);
        dir
    }

    #[test]
    fn a_checkout_is_staged_without_ignored_files() {
        let project = checkout("checkout");
        let target = scratch("checkout-staged");
        stage(&project, &target).unwrap();
        assert!(target.join(".git").is_dir(), "the guest keeps the history");
        assert!(target.join("tracked.txt").is_file());
        assert!(target.join("untracked.txt").is_file(), "untracked work is imported");
        assert!(target.join("src/keep.txt").is_file());
        assert!(!target.join(".env").exists(), "an ignored file never reaches the guest");
        assert!(!target.join("src/.env").exists());
        assert!(!target.join("node_modules").exists(), "an ignored directory is skipped whole");
    }

    #[test]
    fn a_plain_directory_is_staged_whole() {
        let project = scratch("plain");
        std::fs::write(project.join(".env"), "SECRET=1\n").unwrap();
        std::fs::create_dir_all(project.join("nested")).unwrap();
        std::fs::write(project.join("nested/file.txt"), "file\n").unwrap();
        let target = scratch("plain-staged");
        stage(&project, &target).unwrap();
        assert!(target.join(".env").is_file(), "without git there is no ignore list to apply");
        assert!(target.join("nested/file.txt").is_file());
    }

    #[test]
    fn a_subdirectory_filters_its_own_ignored_files() {
        let project = checkout("subdir");
        let target = scratch("subdir-staged");
        stage(&project.join("src"), &target).unwrap();
        assert!(target.join("keep.txt").is_file());
        assert!(!target.join(".env").exists());
        assert!(!target.join(".git").exists(), "the checkout root is not inside the subdirectory");
    }

    #[test]
    fn staging_replaces_an_earlier_view() {
        let project = checkout("replace");
        let target = scratch("replace-staged");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("stale.txt"), "stale\n").unwrap();
        stage(&project, &target).unwrap();
        assert!(!target.join("stale.txt").exists());
        assert!(target.join("tracked.txt").is_file());
    }
}
