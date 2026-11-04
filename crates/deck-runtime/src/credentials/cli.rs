//! `deckd credential set|delete|list|revoke` (ADR 0004 §4.2).
//!
//! The design choice: the store is managed from the `deckd` binary itself, so
//! on macOS the Keychain item's access list names the application that later
//! reads it. A value arrives on stdin, never as an argument (an argument is
//! readable by every local user), with echo off on a terminal. `list` prints
//! names and fingerprints only. `revoke` is a request to the running daemon,
//! because only the daemon can close the operations using the value.

use super::revoked::Revoked;
use super::{Store, valid_name};
use crate::DaemonClient;
use anyhow::{Context as _, Result, bail};
use std::io::{BufRead as _, IsTerminal as _, Read as _};
use std::path::Path;

pub const USAGE: &str = "deckd [--data PATH] credential set NAME | delete NAME | list | revoke NAME";

/// Run one credential command. `args` is everything after `credential`.
pub fn run(data: &Path, args: &[String]) -> Result<()> {
    let name = |what: &str| -> Result<&str> {
        match args {
            [_, name] if valid_name(name) => Ok(name.as_str()),
            [_, name] => bail!("{name:?} is not a credential name: use [A-Z][A-Z0-9_]{{0,63}}, like OPENAI_API_KEY"),
            _ => bail!("credential {what} takes one NAME\nusage: {USAGE}"),
        }
    };
    match args.first().map(String::as_str) {
        Some("set") => {
            let name = name("set")?;
            let store = Store::from_env()?;
            let value = read_value(name)?;
            store.set(name, &value)?;
            eprintln!(
                "{name} set in the {} ({}); the next operation uses it",
                store.describe(),
                super::fingerprint(&value)
            );
            Ok(())
        }
        Some("delete") => {
            let name = name("delete")?;
            let store = Store::from_env()?;
            if store.delete(name)? {
                eprintln!("{name} deleted from the {}", store.describe());
            } else {
                eprintln!("{name} was not in the {}", store.describe());
            }
            Ok(())
        }
        Some("list") if args.len() == 1 => {
            let store = Store::from_env()?;
            let revoked = Revoked::open(data)?;
            let store_name = store.describe();
            for listed in crate::broker::list(&store, &revoked)? {
                let state = match (&listed.error, listed.revoked) {
                    (Some(error), _) => format!("unreadable: {error}"),
                    (None, true) => "revoked".to_string(),
                    (None, false) => "active".to_string(),
                };
                println!("{}\t{}\t{store_name}\t{state}", listed.name, listed.fingerprint.as_deref().unwrap_or("-"));
            }
            Ok(())
        }
        Some("revoke") => {
            let name = name("revoke")?;
            let client = DaemonClient::connect_or_start(data)?;
            let revocation = client.revoke_credential(name)?;
            eprintln!(
                "{} {} revoked; running operations that used it are closed. It is still valid at the provider: \
                 rotate it there, then `deckd credential set {}`",
                revocation.name, revocation.fingerprint, revocation.name
            );
            Ok(())
        }
        _ => bail!("usage: {USAGE}"),
    }
}

/// The value from stdin: one line on a terminal (echo off), or the whole
/// input otherwise, without its trailing newline.
fn read_value(name: &str) -> Result<String> {
    let stdin = std::io::stdin();
    let mut value = String::new();
    if stdin.is_terminal() {
        eprint!("{name}: ");
        let _echo = NoEcho::new();
        stdin.lock().read_line(&mut value).context("reading the value")?;
        eprintln!();
    } else {
        stdin.lock().read_to_string(&mut value).context("reading the value from stdin")?;
    }
    let value = value.strip_suffix('\n').unwrap_or(&value);
    let value = value.strip_suffix('\r').unwrap_or(value);
    if value.is_empty() {
        bail!("no value on stdin; pipe it in or type it at the prompt");
    }
    Ok(value.to_string())
}

/// Terminal echo off for the life of the guard.
struct NoEcho(Option<libc::termios>);

impl NoEcho {
    fn new() -> Self {
        // SAFETY: tcgetattr/tcsetattr read and write a termios we own, for
        // stdin, which is open for the life of the process.
        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut termios) != 0 {
                return Self(None);
            }
            let saved = termios;
            termios.c_lflag &= !libc::ECHO;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &termios) != 0 {
                return Self(None);
            }
            Self(Some(saved))
        }
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            // SAFETY: restores the termios read in `new`.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved);
            }
        }
    }
}
