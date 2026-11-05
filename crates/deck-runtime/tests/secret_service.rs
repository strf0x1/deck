//! Linux credential listing through the real CLI and a scripted secret-tool.
//! Each child gets its own PATH and store environment; no desktop keyring is touched.

#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

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
        Command::new(env!("CARGO_BIN_EXE_deckd"))
            .arg("--data")
            .arg(&data)
            .args(["credential", "list"])
            .env_remove("DECK_CREDENTIALS")
            .env("PATH", &data)
            .env("DECK_TEST_SEARCH", scenario)
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
