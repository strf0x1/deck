//! The fake agent's publication turn: a local commit, then the GitHub
//! operation API (contract 1.2.0 §11) against `DECK_GITHUB_URL`.
//!
//! The design choice: this is a script, not a tool loop. Each step reports one
//! tool result to the caller, and the first failure ends the turn; nothing is
//! retried, because the fake exists to watch what a client sends and how the
//! gateway answers, not to recover from either. The first refusal is reported
//! as the tool's result so a Deck test can read the card's approval id from
//! the transcript.

use serde_json::{Value, json};
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// The operation paths (contract 1.2.0 §11.2).
const REFS: &str = "/operations/refs";
const PUBLISH: &str = "/operations/publish";
const CREATE_PR: &str = "/operations/pull_request";
const UPDATE_PR: &str = "/operations/pull_request/update";
const REVIEW: &str = "/operations/pull_request/review";

/// The file a turn writes, so every prompt has a commit to publish.
const FILE: &str = "deck-fake-agent.txt";
/// The contract's bound on a publish summary's subject.
const MAX_SUBJECT: usize = 256;
/// The pull request body the script writes.
const BODY: &str = "Published by deck-fake-agent.";

/// Run the whole turn, reporting each step as its title and outcome. A step
/// that fails is reported and ends the script; the turn still ends normally,
/// so the prompt gets its response and a client sees the failure in the
/// transcript.
pub fn run(base: &str, cwd: &Path, prompt: &str, mut report: impl FnMut(&str, Result<String, String>)) {
    let title = title(prompt);
    let commit = match commit(cwd, &title) {
        Ok(commit) => commit,
        Err(error) => return report("shell git commit", Err(error)),
    };
    report("shell git commit", Ok(format!("commit {}", commit.sha)));

    let gateway = Gateway::new(base);
    let refs = match gateway.post(REFS, &json!({ "operation": "refs" })) {
        Ok(refs) => {
            report("github refs", Ok(refs.to_string()));
            refs
        }
        Err(error) => return report("github refs", Err(error)),
    };
    let Some(branch) = refs["task_branch"].as_str().filter(|branch| !branch.is_empty()) else {
        return report("github refs", Err("refs named no task_branch".into()));
    };
    let branch = branch.to_string();
    // The guest checkout is already on the task branch (ADR 0008 §4). A fake
    // run started somewhere else can still publish when the branch is behind
    // the commit it just made, but never rewrites a branch ahead of it.
    if let Err(error) = ensure_branch(cwd, &branch) {
        return report("github publish", Err(error));
    }
    let tip = refs["branches"]
        .as_array()
        .and_then(|branches| branches.iter().find(|candidate| candidate["branch"].as_str() == Some(&branch)))
        .and_then(|candidate| candidate["commit"].as_str())
        .filter(|sha| !sha.is_empty());
    let tip = match tip {
        Some(sha) if has_commit(cwd, sha) => match is_ancestor(cwd, sha, &format!("refs/heads/{branch}")) {
            Ok(true) => Some(sha.to_string()),
            // A bundle cannot fast-forward a branch that has moved elsewhere;
            // the gateway would refuse it, so name the commit to rebase onto.
            Ok(false) => {
                return report(
                    "github publish",
                    Err(format!("{branch} on GitHub is at {sha}, which is not in your history: rebase onto it first")),
                );
            }
            Err(error) => return report("github publish", Err(error)),
        },
        // An unknown tip is not a prerequisite this checkout can name; send
        // the whole branch and let the gateway decide.
        _ => None,
    };
    let bundle = match bundle(cwd, &branch, tip.as_deref()) {
        Ok(bytes) => bytes,
        Err(error) => return report("github publish", Err(error)),
    };
    let count = match count(cwd, &branch, tip.as_deref()) {
        Ok(count) => count,
        Err(error) => return report("github publish", Err(error)),
    };
    let head = json!({
        "operation": "publish",
        "branch": branch,
        "summary": { "subject": commit.subject, "count": count.max(1) },
    });
    let mut body = head.to_string().into_bytes();
    let Ok(length) = u32::try_from(bundle.len()) else {
        return report("github publish", Err("the bundle does not fit a 32-bit length".into()));
    };
    body.extend_from_slice(&length.to_be_bytes());
    body.extend_from_slice(&bundle);
    let published = match gateway.exchange(PUBLISH, &body) {
        Ok(reply) => {
            report("github publish", Ok(reply.to_string()));
            reply
        }
        Err(error) => return report("github publish", Err(error)),
    };
    if published["commit"].as_str().is_none() {
        return report("github publish", Err("the publish reply named no commit".into()));
    }

    let default_base = refs["base"].as_str().unwrap_or("main");
    let create = json!({
        "operation": "create_pr", "head": branch, "base": default_base,
        "title": title, "body": BODY, "draft": true,
    });
    let pull = match gateway.post(CREATE_PR, &create) {
        Ok(reply) => {
            report("github create_pr", Ok(reply.to_string()));
            reply
        }
        Err(error) => return report("github create_pr", Err(error)),
    };
    let Some(number) = pull["number"].as_u64() else {
        return report("github create_pr", Err("the pull request reply named no number".into()));
    };

    match gateway.post(REVIEW, &json!({ "operation": "review", "number": number })) {
        Ok(reply) => report("github review", Ok(reply.to_string())),
        Err(error) => return report("github review", Err(error)),
    }

    // Marking a pull request ready is the one card this script expects: the
    // gateway asks the user, and a pending answer is the turn's last result.
    match gateway.post(UPDATE_PR, &json!({ "operation": "update_pr", "number": number, "draft": false })) {
        Ok(reply) => report("github update_pr", Ok(reply.to_string())),
        Err(error) => report("github update_pr", Err(error)),
    }
}

fn title(prompt: &str) -> String {
    let title: String = prompt.trim().chars().take(MAX_SUBJECT).collect();
    if title.is_empty() { "publish the task branch".into() } else { title }
}

struct Commit {
    sha: String,
    subject: String,
}

/// Commit one new file on the current branch. The identity is the fake's, so
/// a test repository needs no configuration; the guest's real identity is
/// Construct's business, not this script's.
fn commit(cwd: &Path, title: &str) -> Result<Commit, String> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
    std::fs::write(cwd.join(FILE), format!("{title}\n{nonce}\n"))
        .map_err(|error| format!("writing {FILE}: {error}"))?;
    git(cwd, &["add", "-f", "--", FILE])?;
    let message = format!("deck-fake-agent: {title}");
    git(cwd, &["commit", "--quiet", "-m", &message])?;
    let sha = git_text(cwd, &["rev-parse", "HEAD"])?;
    let subject = git_text(cwd, &["log", "-1", "--format=%s", &sha])?;
    Ok(Commit { sha, subject })
}

/// Whether `sha` is a commit this checkout has.
fn has_commit(cwd: &Path, sha: &str) -> bool {
    git(cwd, &["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_ok()
}

fn is_ancestor(cwd: &Path, ancestor: &str, descendant: &str) -> Result<bool, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .output()
        .map_err(|error| format!("running git merge-base: {error}"))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "git merge-base --is-ancestor {ancestor} {descendant}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// The local branch the bundle must carry. A checkout already on it is left
/// alone; one that is not is moved forward to `HEAD`, never backwards.
fn ensure_branch(cwd: &Path, branch: &str) -> Result<(), String> {
    let current = git_text(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if current == branch {
        return Ok(());
    }
    if git(cwd, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok() {
        if !is_ancestor(cwd, branch, "HEAD")? {
            return Err(format!(
                "the checkout is on {current}, and {branch} is not in its history: check out {branch} first"
            ));
        }
        git(cwd, &["branch", "-f", branch, "HEAD"])?;
    } else {
        git(cwd, &["branch", branch, "HEAD"])?;
    }
    Ok(())
}

/// `git bundle create` of the branch, incremental from `tip` when one is
/// named (`^tip refs/heads/<branch>`, a prerequisite line in the header).
fn bundle(cwd: &Path, branch: &str, tip: Option<&str>) -> Result<Vec<u8>, String> {
    let reference = format!("refs/heads/{branch}");
    let range = match tip {
        Some(tip) => format!("^{tip} {reference}"),
        None => reference,
    };
    let args: Vec<&str> = ["bundle", "create", "--quiet", "-"].into_iter().chain(range.split(' ')).collect();
    git(cwd, &args)
}

/// How many commits the bundle adds, for the card's summary.
fn count(cwd: &Path, branch: &str, tip: Option<&str>) -> Result<u64, String> {
    let range = match tip {
        Some(tip) => format!("{tip}..refs/heads/{branch}"),
        None => format!("refs/heads/{branch}"),
    };
    git_text(cwd, &["rev-list", "--count", &range])?
        .parse()
        .map_err(|_| format!("git rev-list --count {range} did not return a number"))
}

fn git(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["-c", "user.name=deck-fake-agent", "-c", "user.email=deck-fake-agent@invalid"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .map_err(|error| format!("running git {}: {error}", args.first().copied().unwrap_or_default()))?;
    if !output.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(output.stdout)
}

fn git_text(cwd: &Path, args: &[&str]) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&git(cwd, args)?).trim().to_string())
}

/// The operation API's one carrier: one POST, one JSON reply, one connection.
struct Gateway {
    addr: String,
    host: String,
}

impl Gateway {
    /// `base` is `http://host:port`, the guest's `DECK_GITHUB_URL`.
    fn new(base: &str) -> Self {
        let rest = base.trim_end_matches('/').strip_prefix("http://").unwrap_or(base);
        let (host, port) = match rest.rsplit_once(':') {
            Some((host, port)) => (host, port),
            None => (rest, "80"),
        };
        let host = if host.is_empty() { "127.0.0.1" } else { host };
        Self { addr: format!("{host}:{port}"), host: host.to_string() }
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.exchange(path, body.to_string().as_bytes())
    }

    fn exchange(&self, path: &str, body: &[u8]) -> Result<Value, String> {
        let mut stream =
            TcpStream::connect(&self.addr).map_err(|error| format!("connecting to {}: {error}", self.addr))?;
        let head = format!(
            "POST {path} HTTP/1.1\r\nhost: {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            self.host,
            body.len()
        );
        stream.write_all(head.as_bytes()).map_err(|error| format!("sending {path}: {error}"))?;
        stream.write_all(body).map_err(|error| format!("sending {path}: {error}"))?;
        reply(&mut stream)
    }
}

/// A gateway reply: the success body, or the refusal's message. The gateway
/// frames every reply with `content-length` and closes; reading exactly that
/// many bytes is what makes a reply independent of when the close arrives.
fn reply(stream: &mut TcpStream) -> Result<Value, String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).map_err(|error| format!("reading the gateway's reply: {error}"))?;
        head.push(byte[0]);
        if head.len() > 64 * 1024 {
            return Err("the gateway's reply head is too large".into());
        }
    }
    let head = String::from_utf8_lossy(&head[..head.len() - 4]);
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or_else(|| format!("the gateway's reply is not HTTP: {head}"))?;
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then_some(value.trim())
        })
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| format!("the gateway's reply has no content-length: {head}"))?;
    let mut body = vec![0; length];
    stream.read_exact(&mut body).map_err(|error| format!("reading the gateway's reply body: {error}"))?;
    if status == 200 {
        return serde_json::from_slice(&body).map_err(|error| format!("the gateway's reply is not JSON: {error}"));
    }
    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let code = value.pointer("/error/code").and_then(Value::as_str).unwrap_or("the gateway refused");
    let message = value.pointer("/error/message").and_then(Value::as_str).unwrap_or("no reason given");
    let mut text = format!("{code}: {message}");
    if let Some(retryable) = value.get("retryable").and_then(Value::as_bool) {
        text.push_str(&format!(" (retryable: {retryable})"));
    }
    if let Some(current) = value.get("current").and_then(Value::as_str) {
        text.push_str(&format!(" current {current}"));
    }
    if let Some(approval) = value.get("approval_id").and_then(Value::as_str) {
        text.push_str(&format!(" approval {approval}"));
    }
    Err(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> std::path::PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("deck-fake-agent-bundle-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_bundle_from_a_known_tip_names_its_prerequisite() {
        let dir = scratch();
        git(&dir, &["init", "--quiet"]).unwrap();
        git(&dir, &["checkout", "--quiet", "-b", "deck/t-1"]).unwrap();
        std::fs::write(dir.join("one"), "one").unwrap();
        git(&dir, &["add", "one"]).unwrap();
        git(&dir, &["commit", "--quiet", "-m", "one"]).unwrap();
        let first = git_text(&dir, &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(dir.join("two"), "two").unwrap();
        git(&dir, &["add", "two"]).unwrap();
        git(&dir, &["commit", "--quiet", "-m", "two"]).unwrap();
        let second = git_text(&dir, &["rev-parse", "HEAD"]).unwrap();

        let bytes = bundle(&dir, "deck/t-1", Some(&first)).unwrap();
        let header = String::from_utf8_lossy(&bytes);
        let header: Vec<&str> = header.lines().take_while(|line| !line.is_empty()).collect();
        assert_eq!(header.first(), Some(&"# v2 git bundle"), "a v2 bundle: {header:?}");
        assert!(
            header.iter().any(|line| line.starts_with(&format!("-{first}"))),
            "the tip is the prerequisite: {header:?}"
        );
        assert!(header.iter().any(|line| line.ends_with(&format!("{second} refs/heads/deck/t-1"))), "{header:?}");
        assert_eq!(count(&dir, "deck/t-1", Some(&first)).unwrap(), 1);
        assert_eq!(count(&dir, "deck/t-1", None).unwrap(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
