//! Acquire recorded commits from the project's one GitHub repository before
//! the VM imports a clean checkout. Git's configuration and credential stay
//! on the host. A moved ref may still carry an older pinned commit.

use super::{Plan, Reference, Target, is_commit};
use anyhow::{Context as _, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) const IMPORT: &str = "integration-import";
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn starting_commit(target: &Target, token: Option<&str>) -> Result<String> {
    let (repository, reference) = target.github()?;
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder().max_redirects(0).timeout_global(Some(LOOKUP_TIMEOUT)).build(),
    );
    let suffix = match reference {
        Reference::Base { branch } => format!("git/ref/heads/{branch}"),
        Reference::Pull { number } => format!("pulls/{number}"),
    };
    let url = format!("https://api.github.com/repos/{repository}/{suffix}");
    let mut request = agent.get(&url).header("accept", "application/vnd.github+json").header("user-agent", "Deck");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let mut response =
        request.call().map_err(|_| anyhow::anyhow!("Could not read the integration destination from GitHub."))?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(512 * 1024)
        .read_to_vec()
        .map_err(|_| anyhow::anyhow!("Could not read the GitHub destination response."))?;
    let body: Value = serde_json::from_slice(&bytes).context("GitHub returned an invalid destination response")?;
    let sha = match reference {
        Reference::Base { branch } => {
            ensure!(body["ref"] == format!("refs/heads/{branch}"), "GitHub returned a different destination branch.");
            body.pointer("/object/sha")
        }
        Reference::Pull { .. } => {
            ensure!(body["state"] == "open", "The destination pull request is no longer open.");
            body.pointer("/head/sha")
        }
    };
    Ok(sha
        .and_then(Value::as_str)
        .filter(|sha| is_commit(sha))
        .context("GitHub did not name a full destination commit")?
        .to_owned())
}

pub(crate) fn prepare(path: &Path, plan: &Plan, branch: &str, token: Option<&str>) -> Result<()> {
    let (repository, _) = plan.target.github()?;
    let url = format!("https://github.com/{repository}.git");
    checkout(path, plan, branch, &url, "https", token)
}

fn checkout(path: &Path, plan: &Plan, branch: &str, url: &str, protocol: &str, token: Option<&str>) -> Result<()> {
    let start = plan.base_commit.as_deref().context("The integration destination has no pinned commit.")?;
    let (_, reference) = plan.target.github()?;
    let base_ref = match reference {
        Reference::Base { branch } => format!("refs/heads/{branch}"),
        Reference::Pull { number } => format!("refs/pull/{number}/head"),
    };
    let mut pins = vec![(base_ref, start, "destination".to_owned())];
    for input in &plan.inputs {
        let commit =
            input.commit.as_deref().with_context(|| format!("Contributor {} has no pinned commit.", input.id))?;
        pins.push((
            format!("refs/heads/{}", crate::github::task_branch(&input.id)),
            commit,
            format!("contributor {}", input.id),
        ));
    }
    if path.exists() {
        std::fs::remove_dir_all(path).context("Resetting the integration import")?;
    }
    std::fs::create_dir(path).context("Creating the integration import")?;
    git(path, &["init", "--quiet", "--template="], protocol, None)?;
    // Fetch separately so a missing published ref identifies the input.
    for (index, (reference, commit, label)) in pins.into_iter().enumerate() {
        ensure!(is_commit(commit), "Invalid Git commit for {label}.");
        git(
            path,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-recurse-submodules",
                "--no-write-fetch-head",
                "--",
                url,
                &reference,
            ],
            protocol,
            token,
        )
        .map_err(|_| anyhow::anyhow!("Could not fetch {label} from GitHub."))?;
        git(path, &["cat-file", "-e", &format!("{commit}^{{commit}}")], protocol, None)
            .map_err(|_| anyhow::anyhow!("Pinned commit {commit} for {label} is unavailable."))?;
        git(path, &["update-ref", &format!("refs/deck-inputs/{index}"), commit], protocol, None)?;
    }
    git(path, &["checkout", "--quiet", "-b", branch, start, "--"], protocol, None)?;
    // The VM's normal first-boot import installs the canonical GitHub origin.
    // No credential, fetch ref, or caller's dirty files are stored here.
    Ok(())
}

fn git(path: &Path, args: &[&str], protocol: &str, token: Option<&str>) -> Result<()> {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", protocol)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .current_dir(path)
        .args(["-c", "core.hooksPath=/dev/null", "-c", "http.followRedirects=false", "-c", "gc.auto=0"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    if let Some(token) = token {
        let auth = STANDARD.encode(format!("x-access-token:{token}"));
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader")
            .env("GIT_CONFIG_VALUE_0", format!("Authorization: Basic {auth}"));
    }
    let mut child = command.spawn().context("Starting Git for the integration checkout")?;
    let deadline = Instant::now() + FETCH_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "Git could not prepare the integration checkout.");
            return Ok(());
        }
        if Instant::now() >= deadline {
            // SAFETY: Git was started in its own process group.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            let _ = child.wait();
            bail!("Git timed out preparing the integration checkout.");
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

#[cfg(test)]
#[path = "remote_tests.rs"]
mod tests;
