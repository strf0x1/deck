//! Linux credential listing through the real CLI and a scripted secret-tool.
//! Each listing scenario owns a daemon with its own PATH and store environment;
//! no desktop keyring is touched, and the daemon is stopped before cleanup.

#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A fresh daemon keeps each scripted store scenario independent of the
/// environment inherited by an earlier listing. Reap it even after a panic.
struct ListingDaemon {
    child: Child,
    directory: PathBuf,
}

#[expect(clippy::unwrap_used, reason = "test fixture setup and process observation must succeed")]
impl ListingDaemon {
    fn start(data: &Path, tools: &Path, scenario: &str) -> Self {
        let directory = deck_runtime::paths::Paths::for_data(data).unwrap().directory;
        let mut daemon = Self {
            directory,
            child: Command::new(env!("CARGO_BIN_EXE_deckd"))
                .arg("--data")
                .arg(data)
                .args(["--idle-ms", "60000"])
                .env_remove("DECK_CREDENTIALS")
                .env("PATH", tools)
                .env("DECK_TEST_SEARCH", scenario)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if deck_runtime::DaemonClient::connect(data).is_ok() {
                return daemon;
            }
            assert!(daemon.child.try_wait().unwrap().is_none(), "daemon exited before opening its socket");
            assert!(Instant::now() < deadline, "daemon did not open its socket");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ListingDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn listing_secret_service_reports_names_and_fingerprints_or_a_safe_failure() {
    let data = std::env::temp_dir().join(format!("deck-secret-service-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    let tool = data.join("secret-tool");
    std::fs::write(
        &tool,
        r#"#!/bin/sh
case "$*" in
    'search --all service com.sprawlcommons.deck')
        case "$DECK_TEST_SEARCH" in
            stderr|stdout)
                printf 'secret = synthetic-value\n'
                if [ "$DECK_TEST_SEARCH" = stderr ]; then
                    printf 'attribute.name = TEST_KEY\n' >&2
                else
                    printf 'attribute.name = TEST_KEY\n'
                fi ;;
            empty) exit 0 ;;
            unavailable)
                printf 'secret = synthetic-value\n'
                printf 'bus unavailable: synthetic-diagnostic\n' >&2
                exit 1 ;;
            silent) exit 1 ;;
            killed) kill -TERM $$ ;;
        esac ;;
    'lookup service com.sprawlcommons.deck name TEST_KEY')
        printf 'synthetic-value\n' ;;
    *) exit 2 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
    let list = |scenario: &str| {
        let scenario_data = data.join(scenario);
        let _daemon = ListingDaemon::start(&scenario_data, &data, scenario);
        Command::new(env!("CARGO_BIN_EXE_deckd"))
            .arg("--data")
            .arg(&scenario_data)
            .args(["credential", "list"])
            .env_remove("DECK_CREDENTIALS")
            .env("PATH", &data)
            .env("DECKD", "/deck-test/unexpected-daemon-start")
            .output()
            .unwrap()
    };
    for scenario in ["stderr", "stdout"] {
        let output = list(scenario);
        assert!(output.status.success(), "{scenario}: {output:?}");
        let expected = format!(
            "TEST_KEY\t{}\tSecret Service\tactive\n",
            deck_runtime::credentials::fingerprint("synthetic-value")
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        assert!(output.stderr.is_empty());
    }
    let empty = list("empty");
    assert!(empty.status.success());
    assert!(empty.stdout.is_empty());
    for scenario in ["unavailable", "silent", "killed"] {
        let output = list(scenario);
        assert!(!output.status.success(), "{scenario}: {output:?}");
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("the Secret Service could not list Deck credentials"), "{error}");
        assert!(!error.contains("synthetic"), "{error}");
    }
    std::fs::remove_dir_all(data).unwrap();
}

#[test]
fn deletion_distinguishes_missing_items_from_store_and_clear_failures() {
    let data = std::env::temp_dir().join(format!("deck-secret-delete-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    let tool = data.join("secret-tool");
    std::fs::write(
        &tool,
        r#"#!/bin/sh
case "$*" in
    'lookup service com.sprawlcommons.deck name TEST_KEY')
        case "$DECK_TEST_DELETE" in
            unavailable)
                printf 'synthetic-diagnostic\n' >&2
                exit 1 ;;
            missing) exit 1 ;;
            lookup-killed) kill -TERM $$ ;;
            lookup-failure) exit 2 ;;
            *) printf 'synthetic-value\n' ;;
        esac ;;
    'search --all service com.sprawlcommons.deck name TEST_KEY')
        [ "$DECK_TEST_DELETE" = missing ] || exit 2 ;;
    'clear service com.sprawlcommons.deck name TEST_KEY')
        printf 'called' > "$DECK_TEST_MARKER"
        case "$DECK_TEST_DELETE" in
            clear-failure)
                printf 'synthetic-value\n' >&2
                exit 1 ;;
            clear-silent) exit 1 ;;
        esac ;;
    *) exit 2 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
    let marker = data.join("clear-called");
    for scenario in
        ["unavailable", "missing", "lookup-killed", "lookup-failure", "clear-failure", "clear-silent", "success"]
    {
        let _ = std::fs::remove_file(&marker);
        let output = Command::new(env!("CARGO_BIN_EXE_deckd"))
            .arg("--data")
            .arg(&data)
            .args(["credential", "delete", "TEST_KEY"])
            .env_remove("DECK_CREDENTIALS")
            .env("PATH", &data)
            .env("DECK_TEST_DELETE", scenario)
            .env("DECK_TEST_MARKER", &marker)
            .output()
            .unwrap();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(output.stdout.is_empty());
        assert!(!error.contains("synthetic"), "{error}");
        assert_eq!(
            marker.exists(),
            !matches!(scenario, "unavailable" | "missing" | "lookup-killed" | "lookup-failure")
        );
        match scenario {
            "success" => {
                assert!(output.status.success(), "{error}");
                assert!(error.contains("deleted from"), "{error}");
            }
            "missing" => {
                assert!(output.status.success(), "{error}");
                assert!(error.contains("was not in"), "{error}");
            }
            _ => {
                assert!(!output.status.success(), "{scenario}: {error}");
                assert!(error.contains("the Secret Service could not"), "{error}");
            }
        }
    }
    std::fs::remove_dir_all(data).unwrap();
}

#[test]
fn lookup_distinguishes_a_locked_item_from_a_missing_item() {
    let data = std::env::temp_dir().join(format!("deck-secret-lookup-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    let tool = data.join("secret-tool");
    std::fs::write(
        &tool,
        r#"#!/bin/sh
case "$*" in
    'lookup service com.sprawlcommons.deck name TEST_KEY') exit 1 ;;
    'search --all service com.sprawlcommons.deck name TEST_KEY')
        case "$DECK_TEST_LOOKUP" in
            locked) printf 'attribute.name = TEST_KEY\n' >&2 ;;
            missing) exit 0 ;;
            unavailable) exit 1 ;;
        esac ;;
    *) exit 2 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (scenario, expected) in [
        ("locked", "could not check TEST_KEY before deletion"),
        ("unavailable", "could not check TEST_KEY before deletion"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_deckd"))
            .args(["credential", "delete", "TEST_KEY"])
            .env_remove("DECK_CREDENTIALS")
            .env("PATH", &data)
            .env("DECK_TEST_LOOKUP", scenario)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{scenario}: {output:?}");
        assert!(String::from_utf8(output.stderr).unwrap().contains(expected));
    }
    let missing = Command::new(env!("CARGO_BIN_EXE_deckd"))
        .args(["credential", "delete", "TEST_KEY"])
        .env_remove("DECK_CREDENTIALS")
        .env("PATH", &data)
        .env("DECK_TEST_LOOKUP", "missing")
        .output()
        .unwrap();
    assert!(missing.status.success(), "{missing:?}");
    assert!(String::from_utf8(missing.stderr).unwrap().contains("was not in"));
    std::fs::remove_dir_all(data).unwrap();
}
