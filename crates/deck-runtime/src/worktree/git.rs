//! Native Git registration and materialization use a private common
//! configuration. Git keeps its own locks around refs and indexes.

use anyhow::{Result, bail, ensure};
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

pub(super) struct Shadow(pub(super) PathBuf);

impl Shadow {
    pub(super) fn prepare(path: PathBuf, source: &Path, common: &Path) -> Result<Self> {
        let objects = git(source, &["rev-parse", "--show-object-format"])?;
        let refs = git(source, &["rev-parse", "--show-ref-format"])?;
        ensure!(matches!(objects.as_str(), "sha1" | "sha256"), "Unsupported Git object format.");
        let refs = if refs == "--show-ref-format" { "files" } else { &refs };
        ensure!(matches!(refs, "files" | "reftable"), "Unsupported Git reference format.");
        match path.symlink_metadata() {
            Ok(metadata) => ensure!(metadata.file_type().is_dir(), "The worktree setup directory is occupied."),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&path)?,
            Err(error) => return Err(error.into()),
        }
        fs::create_dir_all(common.join("worktrees"))?;
        for name in ["objects", "refs", "packed-refs", "worktrees", "logs", "reftable"] {
            let native = common.join(name);
            if native.exists() || matches!(name, "packed-refs" | "logs") {
                if native.exists() {
                    ensure!(native.canonicalize()? == native, "The Git store uses a redirected path.");
                }
                let link = path.join(name);
                match link.symlink_metadata() {
                    Ok(_) => ensure!(link.read_link()? == native, "The worktree setup store changed."),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => symlink(&native, &link)?,
                    Err(error) => return Err(error.into()),
                }
            }
        }
        if refs == "files" {
            fs::create_dir_all(common.join("logs"))?;
        }
        let extended = objects == "sha256" || refs == "reftable";
        let mut config = format!("[core]\nrepositoryformatversion = {}\nbare = false\n", u8::from(extended));
        if extended {
            config.push_str("[extensions]\n");
            if objects == "sha256" {
                config.push_str("objectFormat = sha256\n");
            }
            if refs == "reftable" {
                config.push_str("refStorage = reftable\n");
            }
        }
        write_private(&path.join("config"), &config)?;
        Ok(Self(path))
    }

    pub(super) fn git(&self, work: &Path, git_dir: &Path, args: &[&str]) -> Result<String> {
        output(
            command(work)
                .env("GIT_DIR", git_dir)
                .env("GIT_COMMON_DIR", &self.0)
                .env("GIT_WORK_TREE", work)
                .args(args)
                .output()?,
        )
    }
}

pub(super) fn write_private(path: &Path, text: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(file.metadata()?.is_file(), "Git setup needs a regular file.");
    file.write_all(text.as_bytes())?;
    Ok(())
}

pub(super) fn command(path: &Path) -> Command {
    let mut git = Command::new("git");
    git.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(path)
        .stdin(Stdio::null())
        .args([
            "--no-optional-locks",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
        ]);
    git
}

fn output(result: Output) -> Result<String> {
    ensure!(
        result.status.success(),
        "Git could not prepare the task worktree. {}",
        String::from_utf8_lossy(&result.stderr).trim().chars().take(512).collect::<String>()
    );
    Ok(String::from_utf8(result.stdout)?.trim_end_matches(['\n', '\r']).to_owned())
}

pub(super) fn path_output(result: Output) -> Result<PathBuf> {
    Ok(PathBuf::from(output(result)?))
}

fn git(path: &Path, args: &[&str]) -> Result<String> {
    output(command(path).args(args).output()?)
}

pub(super) fn common_dir(path: &Path) -> Result<PathBuf> {
    Ok(path_output(command(path).args(["rev-parse", "--path-format=absolute", "--git-common-dir"]).output()?)?
        .canonicalize()?)
}

pub(super) fn git_dir(path: &Path) -> Result<PathBuf> {
    Ok(path_output(command(path).args(["rev-parse", "--absolute-git-dir"]).output()?)?.canonicalize()?)
}

pub(super) fn commit(path: &Path, reference: &str) -> Result<String> {
    let oid = git(path, &["rev-parse", "--verify", &format!("{reference}^{{commit}}")])?;
    ensure!(crate::integration::is_commit(&oid), "Git returned an invalid commit.");
    Ok(oid)
}

pub(super) fn branch(path: &Path) -> Result<Option<String>> {
    let result = command(path).args(["symbolic-ref", "--quiet", "HEAD"]).output()?;
    if result.status.code() == Some(1) {
        return Ok(None);
    }
    let name = output(result)?;
    ensure!(name.starts_with("refs/heads/"), "Git HEAD is outside the branch namespace.");
    Ok(Some(name))
}

pub(super) fn reference_exists(path: &Path, name: &str) -> Result<bool> {
    let status = command(path).args(["show-ref", "--verify", "--quiet", name]).status()?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("Git could not check for an existing task branch."),
    }
}
