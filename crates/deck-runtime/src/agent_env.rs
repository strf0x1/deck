//! The environment an agent must not inherit: Deck's own configuration
//! (deck#201). The window, `deckd` and the holders pass their environment
//! down to every child, so without this an agent would see the data root,
//! the credential file's path, the window's control socket and the VM
//! backend's settings.
//!
//! The design choice: a named denylist removed at the one place an agent
//! process is spawned, never `env_clear`. The agent needs `PATH`, `HOME` and
//! its own configuration, and what Deck gives it on purpose (a coordinator's
//! control URL and token, the optchat strategy) is added after the removal.

use std::process::Command;

/// The window's dev control socket. Only the window reads it.
pub const WINDOW_CONTROL: &str = "DECK_CONTROL";

/// Variables removed by exact name.
const NAMES: &[&str] = &[
    WINDOW_CONTROL,
    crate::DATA_ENV,
    crate::credentials::FILE_ENV,
    crate::vm::GATEWAY_ENV,
    crate::vm::REQUEST_BUDGET_ENV,
    crate::vm::TOKEN_BUDGET_ENV,
    crate::vm::WEB_PROFILES_ENV,
    crate::github::REPOSITORY_ENV,
    crate::github::BASE_ENV,
    crate::github::BRANCHES_ENV,
    crate::github::TOKEN_ENV,
    // Deck sets these for the agents it means them for; one inherited from
    // whatever started Deck would grant a control surface or credential mode
    // the task was never given.
    deck_acp::names::CONTROL_URL_ENV,
    deck_acp::names::CONTROL_TOKEN_ENV,
    crate::boundary::MANAGED_ENV,
    "DECKD",
];

/// Prefixes removed whole: the VM backend's settings.
const PREFIXES: &[&str] = &["DECK_VM_"];

/// Whether `name` is Deck's own and stays out of an agent's environment.
pub fn internal(name: &str) -> bool {
    NAMES.contains(&name) || PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// Remove every internal variable this process has from `command`'s
/// environment.
pub fn scrub(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(internal) {
            command.env_remove(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deck_configuration_is_internal_and_the_agents_own_is_not() {
        for name in ["DECK_CONTROL", "DECK_DATA", "DECK_CREDENTIALS", "DECK_VM_IMAGE", "DECK_VM_KRUN", "DECKD"] {
            assert!(internal(name), "{name}");
        }
        for name in ["PATH", "HOME", "CONSTRUCT_MODEL", "OPENAI_API_KEY", "DECK_FAKE_STATE", "DECK_BOUNDARY_URL"] {
            assert!(!internal(name), "{name}");
        }
    }
}
