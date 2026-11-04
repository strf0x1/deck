//! The host's side of a publish: fetch one bundle into a scratch repository,
//! check the ref rules against what is actually there, then push exactly one
//! branch and nothing else (ADR 0007 §2).
//!
//! The design choice: every `git` invocation names its refspec and never
//! publishes more than the branch the policy allowed, whatever the bundle
//! contains. The bundle is fetched from a local path under the gateway's own
//! scratch directory, `git push` gets `--force-with-lease=<branch>:<sha>`
//! whose expected sha was read a moment earlier, and the credential reaches
//! git as environment-supplied config (`GIT_CONFIG_*`), never as an argument,
//! a file or a remote URL that a process listing could show. The gateway's
//! own environment is not inherited by the children, and one publish runs at
//! a time, because they share the scratch repository.

use crate::github::policy::{Policy, branch_name};
use crate::github::problem::{Kind, Problem};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The commands the gateway runs. A git that cannot report in this window is
/// an outage for one operation, not a reason to keep a socket open.
const LIMIT: Duration = Duration::from_secs(120);
/// The directory the gateway owns for the bundle it is about to fetch.
const SCRATCH: &str = "scratch";
/// Where a publish keeps the remote branch's tip and the bundle's commit in
/// the scratch repository. Outside `refs/heads/`, so no branch is implied.
const REMOTE_REF: &str = "refs/deck/remote";
const INCOMING_REF: &str = "refs/deck/incoming";

/// One published commit and what the remote now says after it landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub branch: String,
    /// The commit the branch pointed at before this publish, when it existed.
    pub previous: Option<String>,
    /// The commit the bundle contained, as the local checkout of it saw it.
    pub commit: String,
}

/// What a remote branch is, as the host read it before a push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remote {
    Absent,
    At(Commit),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commit(pub [u8; 20]);

impl Commit {
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.len() != 40 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let mut bytes = [0u8; 20];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let pair = text.get(index * 2..index * 2 + 2)?;
            *byte = u8::from_str_radix(pair, 16).ok()?;
        }
        Some(Self(bytes))
    }

    pub fn hex(self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

/// The remote is the GitHub repository; `remote_url` is the pinned clone URL
/// the policy built, never a guest string.
pub struct Publisher {
    policy: Policy,
    task: String,
    remote_url: String,
    scratch: PathBuf,
    git: PathBuf,
    /// The host token, held in memory for this process only.
    token: Option<String>,
}

impl Publisher {
    pub fn new(policy: Policy, task: String, remote_url: String, scratch: PathBuf) -> Self {
        Self { policy, task, remote_url, scratch, git: git(), token: None }
    }

    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token.filter(|value| !value.is_empty());
        self
    }

    /// A `git` other than the one on `PATH`, for a host that pins its own.
    pub fn with_git(mut self, git: PathBuf) -> Self {
        self.git = git;
        self
    }

    /// The command's `PATH` once `env_clear` removed it: the gateway keeps its
    /// own `PATH` (or the platform default), and `git` is resolved there.
    fn path(&self) -> String {
        std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string())
    }

    /// Read the remote branch before anything is fetched, so a bundle that
    /// cannot be fast-forwarded is refused before git holds its objects.
    /// `git ls-remote --exit-code` reports an absent ref as status 2: every
    /// first publish starts there, so 2 is an answer and only other statuses
    /// are failures.
    pub fn remote(&self, branch: &str) -> Result<Remote, Problem> {
        branch_name(branch).ok_or_else(|| Problem::invalid(format!("{branch:?} is not a branch name")))?;
        let reference = format!("refs/heads/{branch}");
        let output =
            self.run(&["ls-remote", "--exit-code", self.remote_url(), &reference]).map_err(Problem::unavailable)?;
        match output.status.code() {
            Some(0) => {}
            Some(2) => return Ok(Remote::Absent),
            _ => return Err(transport(stderr(&output, self.token.as_deref()))),
        }
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let Some(line) = text.lines().next() else { return Ok(Remote::Absent) };
        let Some((commit, name)) = line.split_once('\t') else {
            return Err(Problem::unavailable(format!("unexpected ls-remote output: {line:?}")));
        };
        if name != reference {
            return Err(Problem::unavailable(format!("the remote answered about {name}")));
        }
        let commit = Commit::parse(commit).ok_or_else(|| Problem::unavailable("the remote named no commit"))?;
        Ok(Remote::At(commit))
    }

    /// Publish one bundle to one branch. The bundle was already checked by
    /// [`crate::github::bundle::Bundle::branch`]; this repeats the refspec so
    /// a bundle that lies about its contents cannot publish another ref.
    pub fn publish(&self, branch: &str, bytes: &[u8]) -> Result<Published, Problem> {
        if !self.policy.writable(branch, &self.task) {
            return Err(Problem::refused(format!("{branch} is not a branch this task may write")));
        }
        if self.token.is_none() {
            return Err(Problem::credential("the host has no GitHub credential"));
        }
        // The scratch repository is what every command runs in, `ls-remote`
        // included, so it exists before the first one.
        self.prepare()?;
        let remote = self.remote(branch)?;
        let repository = self.repository()?;
        let bundle = repository.join("incoming.bundle");
        std::fs::write(&bundle, bytes).map_err(|error| Problem::unavailable(format!("storing the bundle: {error}")))?;
        let bundle = bundle.to_string_lossy().into_owned();
        let reference = format!("refs/heads/{branch}");
        // The remote's tip is fetched first, so an incremental bundle's
        // prerequisites and the ancestry check below find their objects even
        // when the branch was last pushed by something other than this
        // scratch repository.
        if let Remote::At(_) = remote {
            self.fetch(self.remote_url(), &format!("+{reference}:{REMOTE_REF}"))?;
        }
        // The bundle's one allowed ref lands on a ref this module owns, so the
        // commit under test is exactly what this fetch brought in.
        // A bundle git cannot fetch (a prerequisite the branch does not have)
        // is the agent's to rebuild, not an outage.
        self.fetch(&bundle, &format!("+{reference}:{INCOMING_REF}"))
            .map_err(|problem| Problem::invalid(format!("the bundle cannot be fetched: {}", problem.message)))?;
        let commit = self.rev_parse(INCOMING_REF)?;
        if let Remote::At(previous) = remote
            && !self.ancestor(&previous.hex(), &commit.hex())?
        {
            // The remote must be an ancestor, so this is a fast-forward. A
            // bundle built on another commit is a rewrite in disguise.
            let previous = previous.hex();
            return Err(Problem::refused(format!(
                "{branch} is at {previous}, which the bundle does not build on; a publish only extends a \
                 branch, never rewrites it, so rebuild on {previous} and publish again"
            ))
            .at(previous));
        }
        let lease = match remote {
            Remote::Absent => format!("--force-with-lease={reference}:{}", zero()),
            Remote::At(previous) => format!("--force-with-lease={reference}:{}", previous.hex()),
        };
        let refspec = format!("{}:{reference}", commit.hex());
        let pushed = self
            .run(&["push", "--quiet", "--no-verify", &lease, self.remote_url(), &refspec])
            .map_err(Problem::unavailable)?;
        if !pushed.status.success() {
            return Err(match self.remote(branch) {
                // A lease failure means the branch moved between the read and
                // the push; say so instead of reporting a transport error.
                Ok(Remote::At(now)) if remote != Remote::At(now) => Problem::new(
                    Kind::Conflict,
                    format!(
                        "{branch} moved to {} while this publish ran; read it again, rebuild on it and publish again",
                        now.hex()
                    ),
                )
                .at(now.hex()),
                _ => transport(format!("push: {}", stderr(&pushed, self.token.as_deref()))),
            });
        }
        Ok(Published {
            branch: branch.to_string(),
            previous: match remote {
                Remote::At(previous) => Some(previous.hex()),
                Remote::Absent => None,
            },
            commit: commit.hex(),
        })
    }

    /// The scratch repository every command runs in. It exists before the
    /// first fetch and is never the caller's working directory: a git command
    /// without an explicit repository would otherwise read whatever repository
    /// the gateway happened to be started from.
    fn repository(&self) -> Result<PathBuf, Problem> {
        if self.scratch.as_os_str().is_empty() {
            return Err(Problem::unavailable("no scratch directory"));
        }
        Ok(self.scratch.join(SCRATCH))
    }

    fn prepare(&self) -> Result<(), Problem> {
        let repository = self.repository()?;
        // `git init --bare` creates the repository directory itself, so only
        // the root the gateway owns is made here.
        std::fs::create_dir_all(&self.scratch)
            .map_err(|error| Problem::unavailable(format!("preparing the scratch directory: {error}")))?;
        if repository.join("HEAD").is_file() {
            return Ok(());
        }
        let init = self.run_in(&self.scratch, &["init", "--quiet", "--bare", SCRATCH]).map_err(Problem::unavailable)?;
        if !init.status.success() {
            return Err(Problem::unavailable(stderr(&init, self.token.as_deref())));
        }
        Ok(())
    }

    /// Whether `ancestor` is reachable from `descendant`. `git merge-base
    /// --is-ancestor` reports "no" as exit status 1, so a failed status is an
    /// answer and only other statuses are an outage.
    fn ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, Problem> {
        let output = self.run(&["merge-base", "--is-ancestor", ancestor, descendant]).map_err(Problem::unavailable)?;
        match output.status.code() {
            Some(0) => Ok(true),
            // Exit 1 is git's "no". Anything else is an outage, and the
            // caller must not read it as a rewrite.
            Some(1) => Ok(false),
            _ => Err(Problem::unavailable(format!(
                "merge-base could not answer: {}",
                stderr(&output, self.token.as_deref())
            ))),
        }
    }

    /// Fetch one refspec into the scratch repository.
    fn fetch(&self, source: &str, refspec: &str) -> Result<(), Problem> {
        let fetched = self
            .run(&["fetch", "--quiet", "--no-tags", "--no-recurse-submodules", source, refspec])
            .map_err(Problem::unavailable)?;
        if !fetched.status.success() {
            return Err(transport(format!("fetch: {}", stderr(&fetched, self.token.as_deref()))));
        }
        Ok(())
    }

    fn rev_parse(&self, reference: &str) -> Result<Commit, Problem> {
        let output = self.run(&["rev-parse", "--verify", reference]).map_err(Problem::unavailable)?;
        if !output.status.success() {
            return Err(Problem::unavailable(format!("rev-parse: {}", stderr(&output, self.token.as_deref()))));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Commit::parse(&text).ok_or_else(|| Problem::unavailable(format!("git named no commit for {reference}")))
    }

    /// The one clone URL the policy builds: the repository from the host's
    /// environment, never a guest string.
    fn remote_url(&self) -> &str {
        &self.remote_url
    }

    /// The environment every child gets: this process's is not inherited, so
    /// no host variable (a proxy, a credential, a `GIT_*` override) reaches
    /// git. The token is a `-c` argument only when one is configured, and
    /// every command runs inside the scratch repository.
    fn command(&self) -> Command {
        let mut command = self.base();
        if let Ok(repository) = self.repository() {
            command.arg("--git-dir").arg(repository);
            command.current_dir(self.scratch.as_path());
        }
        command
    }

    /// The environment and credential arguments, without a repository: `git
    /// init` must run before the scratch repository exists.
    fn base(&self) -> Command {
        let mut command = Command::new(&self.git);
        command.env_clear();
        command.env("HOME", self.scratch.as_path());
        command.env("GIT_TERMINAL_PROMPT", "0");
        command.env("GIT_ASKPASS", "");
        command.env("GIT_CONFIG_NOSYSTEM", "1");
        command.env("GIT_AUTHOR_NAME", "deck");
        command.env("GIT_AUTHOR_EMAIL", "deck@localhost");
        command.env("PATH", self.path());
        // The credential reaches git as environment-supplied config, never as
        // an argument: a child's argv is readable by every local user, its
        // environment only by the same user. GitHub's smart HTTP takes the
        // token as Basic auth with the `x-access-token` user, which works for
        // both fine-grained and classic tokens.
        if let Some(token) = &self.token {
            let credential = base64(format!("x-access-token:{}", token.replace(['\r', '\n'], "")).as_bytes());
            command.env("GIT_CONFIG_COUNT", "1");
            command.env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader");
            command.env("GIT_CONFIG_VALUE_0", format!("Authorization: Basic {credential}"));
        }
        command
    }

    fn run(&self, args: &[&str]) -> Result<std::process::Output, String> {
        self.spawn(self.command(), args)
    }

    /// One command in an explicit directory, for `git init` before the
    /// scratch repository exists.
    fn run_in(&self, directory: &Path, args: &[&str]) -> Result<std::process::Output, String> {
        let mut command = self.base();
        command.current_dir(directory);
        self.spawn(command, args)
    }

    fn spawn(&self, mut command: Command, args: &[&str]) -> Result<std::process::Output, String> {
        for arg in args {
            command.arg(arg);
        }
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| format!("running {:?}: {error}", self.git))?;
        let started = std::time::Instant::now();
        loop {
            match child.try_wait().map_err(|error| error.to_string())? {
                Some(_) => return child.wait_with_output().map_err(|error| error.to_string()),
                None if started.elapsed() > LIMIT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{:?} did not finish within {} s",
                        args.first().unwrap_or(&""),
                        LIMIT.as_secs()
                    ));
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}

/// A child's stderr as a message the agent reads. A credential echo is the one
/// thing a git message must never carry into a guest response.
fn stderr(output: &std::process::Output, token: Option<&str>) -> String {
    let mut reason = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if let Some(token) = token.filter(|token| !token.is_empty()) {
        // Git could echo either form: the token, or the header it was sent in.
        let credential = base64(format!("x-access-token:{token}").as_bytes());
        reason = reason.replace(&credential, "***").replace(token, "***");
    }
    if reason.is_empty() { format!("git exited with {}", output.status) } else { reason }
}

/// A failed command that talked to GitHub, sorted by what its stderr says.
fn transport(reason: String) -> Problem {
    Problem::new(Kind::of_git(&reason), reason)
}

/// The `git` the gateway runs: the first one on the host's `PATH`, resolved to
/// an absolute path, because the children start with a cleared environment and
/// a relative name would depend on the `PATH` they were given.
fn git() -> PathBuf {
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string());
    for directory in path.split(':').filter(|directory| !directory.is_empty()) {
        let candidate = Path::new(directory).join("git");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("git")
}

/// Standard base64 with padding, for the one Basic credential git sends. Not
/// worth a dependency for four lines.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word =
            chunk.iter().enumerate().fold(0u32, |word, (index, byte)| word | u32::from(*byte) << (16 - 8 * index));
        for index in 0..4 {
            let sextet = ((word >> (18 - 6 * index)) & 0x3f) as usize;
            out.push(if index <= chunk.len() { char::from(ALPHABET[sextet]) } else { '=' });
        }
    }
    out
}

fn zero() -> String {
    "0".repeat(40)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_basic_credential_is_standard_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"x-access-token:ghp_abc"), "eC1hY2Nlc3MtdG9rZW46Z2hwX2FiYw==");
    }

    #[test]
    fn the_token_never_appears_in_a_childs_arguments() {
        let policy = Policy::new("o/r", "main", Vec::new()).unwrap();
        let publisher = Publisher::new(policy, "t-1".into(), "https://github.com/o/r".into(), PathBuf::from("/tmp/x"))
            .with_token(Some("ghp-SECRET".into()));
        let command = publisher.command();
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
        assert!(!args.iter().any(|arg| arg.contains("ghp-SECRET") || arg.contains("extraheader")), "{args:?}");
        let value = command
            .get_envs()
            .find(|(key, _)| *key == "GIT_CONFIG_VALUE_0")
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap();
        assert_eq!(value, format!("Authorization: Basic {}", base64(b"x-access-token:ghp-SECRET")));
    }

    #[test]
    fn a_commit_is_forty_hex_digits() {
        let text = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(Commit::parse(text).unwrap().hex(), text);
        assert!(Commit::parse("0123").is_none());
        assert!(Commit::parse(&format!("{text}g")).is_none());
        assert!(Commit::parse("").is_none());
    }

    #[test]
    fn the_publisher_refuses_a_branch_the_task_cannot_write() {
        let policy = Policy::new("o/r", "main", Vec::new()).unwrap();
        let publisher =
            Publisher::new(policy, "t-1".into(), "https://github.com/o/r".into(), PathBuf::from("/nonexistent"));
        for branch in ["main", "deck/t-2"] {
            let problem = publisher.publish(branch, b"").unwrap_err();
            assert_eq!(problem.kind, Kind::Refused);
            assert!(problem.message.contains("may write"), "{problem:?}");
        }
        // Its own branch reaches the credential check, not the ref rule.
        assert_eq!(publisher.publish("deck/t-1", b"").unwrap_err().kind, Kind::Credential);
    }
}
