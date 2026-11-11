//! Entry point for the per-task gateway process. Arguments and the secret are
//! the whole configuration; the VMM starts it before boot and waits for its
//! readiness line.
//!
//! The design choice: no credential value reaches this process through its
//! arguments or environment. `--secret` and `--github-token` are credential
//! *names*; each operation fetches its value from `deckd` over `--control`
//! (ADR 0004 §4.4). The gateway prints one JSON line, then never writes a
//! secret anywhere. The
//! web gateway's policy — its profiles, its grant files, its optional pinned
//! hosts — is all given as arguments by the VMM, so nothing in the guest can
//! widen it.

use anyhow::{Context as _, Result, bail};
use deck_gateway::control::Control;
use deck_gateway::policy::{Budget, Usage};
use deck_gateway::server::{self, Config, GitHubSettings, WebSettings};
use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "deck-gateway --model-socket PATH --extension-socket PATH [--web-socket PATH] [--github-socket PATH] --upstream URL --model NAME [--model NAME] (--secret NAME | --keyless) [--request-budget N] [--token-budget N] [--used-requests N] [--used-tokens N] [--budget-file PATH] [--control PATH --task ID] [--web-profile NAME] [--grants PATH] [--project ID] [--web-hosts PATH] [--github-repository OWNER/NAME --github-token SECRET --github-scratch PATH [--github-base BRANCH] [--github-api URL] [--github-remote URL] [--github-branch BRANCH] [--github-git PATH]] [--exit-on-stdin-eof]";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("deck-gateway: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let (config, secret, watch_stdin) = parse(std::env::args().skip(1))?;
    let secret = secret.unwrap_or_else(|| "none (keyless)".into());
    if watch_stdin {
        // The VMM holds the write end. Its death closes this pipe, which
        // stops the gateway even when the VM's process group was not signalled.
        std::thread::Builder::new().name("gateway-parent".into()).spawn(|| {
            let mut byte = [0u8; 64];
            let mut stdin = std::io::stdin();
            loop {
                match stdin.read(&mut byte) {
                    Ok(0) => std::process::exit(1),
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => std::process::exit(1),
                }
            }
        })?;
    }
    server::serve(config).with_context(|| format!("serving the gateway for secret {secret}"))
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<(Config, Option<String>, bool)> {
    let mut model_socket = None;
    let mut extension_socket = None;
    let mut upstream = None;
    let mut models = Vec::new();
    let mut secret = None;
    let mut keyless = false;
    let mut request_budget = None;
    let mut token_budget = None;
    let mut used_requests = 0;
    let mut used_tokens = 0;
    let mut budget_file = None;
    let mut control = None;
    let mut task = None;
    let mut web_socket = None;
    let mut web_profiles = Vec::new();
    let mut grants = Vec::new();
    let mut project = None;
    let mut web_hosts = None;
    let mut web_test_connect = None;
    let mut github_socket = None;
    let mut github_repository = None;
    let mut github_base = None;
    let mut github_branches = Vec::new();
    let mut github_token = None;
    let mut github_scratch = None;
    let mut github_api = None;
    let mut github_remote = None;
    let mut github_git = None;
    let mut watch_stdin = false;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--model-socket" => model_socket = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--extension-socket" => extension_socket = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--upstream" => upstream = Some(value(&mut args, &flag)?),
            "--model" => models.push(value(&mut args, &flag)?),
            "--secret" => secret = Some(value(&mut args, &flag)?),
            "--keyless" => keyless = true,
            "--request-budget" => request_budget = Some(number(&mut args, &flag)?),
            "--token-budget" => token_budget = Some(number(&mut args, &flag)?),
            "--used-requests" => used_requests = number(&mut args, &flag)?,
            "--used-tokens" => used_tokens = number(&mut args, &flag)?,
            "--budget-file" => budget_file = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--control" => control = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--task" => task = Some(value(&mut args, &flag)?),
            "--web-socket" => web_socket = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--web-profile" => web_profiles.push(value(&mut args, &flag)?),
            "--grants" => grants.push(PathBuf::from(value(&mut args, &flag)?)),
            "--project" => project = Some(value(&mut args, &flag)?),
            "--web-hosts" => web_hosts = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--web-test-connect" => {
                let address = value(&mut args, &flag)?;
                web_test_connect = Some(address.parse().with_context(|| format!("--web-test-connect: {address:?}"))?);
            }
            "--github-socket" => github_socket = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--github-repository" => github_repository = Some(value(&mut args, &flag)?),
            "--github-base" => github_base = Some(value(&mut args, &flag)?),
            "--github-branch" => github_branches.push(value(&mut args, &flag)?),
            "--github-token" => github_token = Some(value(&mut args, &flag)?),
            "--github-scratch" => github_scratch = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--github-api" => github_api = Some(value(&mut args, &flag)?),
            "--github-remote" => github_remote = Some(value(&mut args, &flag)?),
            "--github-git" => github_git = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--exit-on-stdin-eof" => watch_stdin = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    // Exactly one of them, so a forgotten flag is an error rather than a
    // provider that silently gets no key (ADR 0012 §2).
    let secret = match (secret, keyless) {
        (Some(name), false) => Some(name),
        (None, true) => None,
        (Some(_), true) => bail!("--secret and --keyless are exclusive"),
        (None, false) => bail!("{USAGE}"),
    };
    let upstream = upstream.context(USAGE)?;
    if !upstream.starts_with("http://") && !upstream.starts_with("https://") {
        bail!("--upstream must be an http(s) URL, not {upstream:?}");
    }
    if models.is_empty() {
        bail!("--model names at least one model the task may use");
    }
    // `--task` names the task whose branch rules this gateway applies, with
    // or without a queue; `--control` needs it because a decision is per task.
    let control = match &control {
        Some(socket) => {
            let task = task.clone().context("--control needs --task")?;
            Some(Control::new(socket.clone(), task))
        }
        None => None,
    };
    let web = match web_socket {
        Some(socket) => {
            Some(WebSettings { socket, profiles: web_profiles, hosts: web_hosts, test_connect: web_test_connect })
        }
        None => {
            if !web_profiles.is_empty() || web_hosts.is_some() || web_test_connect.is_some() {
                bail!("--web-profile and --web-hosts need --web-socket");
            }
            None
        }
    };
    if (!grants.is_empty() || project.is_some()) && task.is_none() {
        bail!("--grants and --project need --task");
    }
    let task_id = task.clone().unwrap_or_default();
    let github = match github_socket {
        Some(socket) => {
            let repository = github_repository.context("--github-socket needs --github-repository")?;
            let policy = deck_gateway::github::policy::Policy::new(
                &repository,
                github_base.as_deref().unwrap_or("main"),
                github_branches,
            )
            .map_err(anyhow::Error::msg)?;
            let name = github_token.unwrap_or_else(|| "DECK_GITHUB_TOKEN".to_string());
            if task_id.is_empty() {
                bail!("--github-socket needs --task: every branch rule is derived from it");
            }
            let repository_name = policy.repository().to_string();
            Some(GitHubSettings {
                socket,
                api: github_api.unwrap_or_else(|| "https://api.github.com".into()),
                repository_url: github_remote.unwrap_or_else(|| format!("https://github.com/{repository_name}")),
                policy,
                task: task_id.clone(),
                secret: name,
                scratch: github_scratch.context("--github-socket needs --github-scratch")?,
                git: github_git,
            })
        }
        None => {
            if github_repository.is_some()
                || github_token.is_some()
                || github_scratch.is_some()
                || github_api.is_some()
                || github_remote.is_some()
                || github_git.is_some()
                || !github_branches.is_empty()
                || github_base.is_some()
            {
                bail!("the --github-* flags need --github-socket");
            }
            None
        }
    };
    Ok((
        Config {
            upstream,
            models,
            secret: secret.clone(),
            control,
            task: task_id,
            grants,
            project,
            budget: Budget { requests: request_budget, tokens: token_budget },
            usage: Usage { requests: used_requests, tokens: used_tokens },
            budget_file,
            model_socket: model_socket.context(USAGE)?,
            extension_socket: extension_socket.context(USAGE)?,
            web,
            github,
        },
        secret,
        watch_stdin,
    ))
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next().with_context(|| format!("{flag} needs a value"))
}

fn number(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<u64> {
    let value = value(args, flag)?;
    value.parse().with_context(|| format!("{flag} must be a whole number, not {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn requires_a_model_and_an_http_upstream() {
        let base = args(&["--model-socket", "/tmp/m", "--extension-socket", "/tmp/e", "--secret", "KEY"]);
        let mut no_model = base.clone();
        no_model.extend(args(&["--upstream", "https://example.com/v1"]));
        assert!(parse(no_model).is_err());

        let mut ftp = base;
        ftp.extend(args(&["--upstream", "ftp://example.com/v1", "--model", "m"]));
        assert!(parse(ftp).is_err());
    }

    #[test]
    fn a_provider_key_is_named_or_explicitly_absent() {
        let base = args(&[
            "--model-socket",
            "/tmp/m",
            "--extension-socket",
            "/tmp/e",
            "--upstream",
            "http://10.0.0.5:8000/v1",
            "--model",
            "m",
        ]);
        assert!(parse(base.clone()).is_err(), "neither --secret nor --keyless");
        let mut keyless = base.clone();
        keyless.push("--keyless".into());
        let (config, secret, _) = parse(keyless.clone()).unwrap();
        assert_eq!((config.secret, secret), (None, None));
        keyless.extend(args(&["--secret", "KEY"]));
        assert!(parse(keyless).is_err(), "both");
    }

    #[test]
    fn grant_policy_is_available_without_web_but_requires_a_task_identity() {
        let mut flags = args(&[
            "--model-socket",
            "/tmp/m",
            "--extension-socket",
            "/tmp/e",
            "--upstream",
            "https://example.com/v1",
            "--model",
            "m",
            "--secret",
            "KEY",
            "--grants",
            "/tmp/grants.json",
            "--project",
            "/project",
        ]);
        assert!(parse(flags.clone()).unwrap_err().to_string().contains("need --task"));
        flags.extend(args(&["--task", "t"]));
        let (config, _, _) = parse(flags).unwrap();
        assert!(config.web.is_none());
        assert_eq!(config.task, "t");
        assert_eq!(config.grants, vec![PathBuf::from("/tmp/grants.json")]);
        assert_eq!(config.project.as_deref(), Some("/project"));
    }

    #[test]
    fn budgets_and_prior_usage_are_parsed() {
        let (config, _, _) = parse(args(&[
            "--model-socket",
            "/tmp/m",
            "--extension-socket",
            "/tmp/e",
            "--upstream",
            "http://127.0.0.1:1/v1",
            "--model",
            "a",
            "--secret",
            "DECK_GATEWAY_TEST_SECRET",
            "--request-budget",
            "4",
            "--token-budget",
            "100",
            "--used-requests",
            "2",
            "--used-tokens",
            "40",
            "--budget-file",
            "/tmp/budget.json",
        ]))
        .unwrap();
        assert_eq!(config.budget.requests, Some(4));
        assert_eq!(config.budget.tokens, Some(100));
        assert_eq!(config.usage.requests, 2);
        assert_eq!(config.usage.tokens, 40);
        assert_eq!(config.budget_file, Some(PathBuf::from("/tmp/budget.json")));

        let mut bad = args(&[
            "--model-socket",
            "/tmp/m",
            "--extension-socket",
            "/tmp/e",
            "--upstream",
            "http://127.0.0.1:1/v1",
            "--model",
            "a",
            "--secret",
            "KEY",
            "--request-budget",
            "lots",
        ]);
        assert!(parse(bad.drain(..)).is_err());
    }

    #[test]
    fn accepts_repeated_models_and_a_missing_key() {
        let (config, secret, watch) = parse(args(&[
            "--model-socket",
            "/tmp/m",
            "--extension-socket",
            "/tmp/e",
            "--upstream",
            "http://127.0.0.1:1/v1",
            "--model",
            "a",
            "--model",
            "b",
            "--secret",
            "DECK_GATEWAY_TEST_SECRET",
        ]))
        .unwrap();
        assert_eq!(config.models, ["a", "b"]);
        assert_eq!(secret.as_deref(), Some("DECK_GATEWAY_TEST_SECRET"));
        assert!(!watch);
    }
}
