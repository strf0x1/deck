//! Entry point for the per-task gateway process. Arguments and the secret are
//! the whole configuration; the VMM starts it before boot and waits for its
//! readiness line.
//!
//! The design choice: the provider key comes from the host environment (the
//! daemon's, never the guest's) until deckd's credential broker owns it. The
//! gateway prints one JSON line, then never writes a secret anywhere.

use anyhow::{Context as _, Result, bail};
use deck_gateway::control::Control;
use deck_gateway::policy::{Budget, Usage};
use deck_gateway::server::{self, Config};
use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "deck-gateway --model-socket PATH --extension-socket PATH --upstream URL --model NAME [--model NAME] --secret NAME [--request-budget N] [--token-budget N] [--used-requests N] [--used-tokens N] [--budget-file PATH] [--control PATH --task ID] [--exit-on-stdin-eof]";

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

fn parse(args: impl IntoIterator<Item = String>) -> Result<(Config, String, bool)> {
    let mut model_socket = None;
    let mut extension_socket = None;
    let mut upstream = None;
    let mut models = Vec::new();
    let mut secret = None;
    let mut request_budget = None;
    let mut token_budget = None;
    let mut used_requests = 0;
    let mut used_tokens = 0;
    let mut budget_file = None;
    let mut control = None;
    let mut task = None;
    let mut watch_stdin = false;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--model-socket" => model_socket = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--extension-socket" => extension_socket = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--upstream" => upstream = Some(value(&mut args, &flag)?),
            "--model" => models.push(value(&mut args, &flag)?),
            "--secret" => secret = Some(value(&mut args, &flag)?),
            "--request-budget" => request_budget = Some(number(&mut args, &flag)?),
            "--token-budget" => token_budget = Some(number(&mut args, &flag)?),
            "--used-requests" => used_requests = number(&mut args, &flag)?,
            "--used-tokens" => used_tokens = number(&mut args, &flag)?,
            "--budget-file" => budget_file = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--control" => control = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--task" => task = Some(value(&mut args, &flag)?),
            "--exit-on-stdin-eof" => watch_stdin = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    let secret = secret.context(USAGE)?;
    let upstream = upstream.context(USAGE)?;
    if !upstream.starts_with("http://") && !upstream.starts_with("https://") {
        bail!("--upstream must be an http(s) URL, not {upstream:?}");
    }
    if models.is_empty() {
        bail!("--model names at least one model the task may use");
    }
    let key = std::env::var(&secret).ok().filter(|value| !value.is_empty());
    let control = match (control, task) {
        (Some(socket), Some(task)) => Some(Control::new(socket, task)),
        (None, None) => None,
        _ => bail!("--control and --task are given together or not at all"),
    };
    Ok((
        Config {
            upstream,
            models,
            key,
            secret: secret.clone(),
            control,
            budget: Budget { requests: request_budget, tokens: token_budget },
            usage: Usage { requests: used_requests, tokens: used_tokens },
            budget_file,
            model_socket: model_socket.context(USAGE)?,
            extension_socket: extension_socket.context(USAGE)?,
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
        assert_eq!(secret, "DECK_GATEWAY_TEST_SECRET");
        assert!(!watch);
    }
}
