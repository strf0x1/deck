//! The OS credential store: a macOS Keychain generic password (service
//! `com.sprawlcommons.deck`, account = the name), or a Linux Secret Service
//! item (`service=com.sprawlcommons.deck`, `name=<name>`).
//!
//! The design choice: on macOS `deckd` calls the Security framework itself,
//! so the item's access list names `deckd`, the same application that later
//! reads it. On Linux it runs `secret-tool`, with the value on stdin and
//! stdout and never in an argument, so this crate needs no D-Bus stack.
//! Anything the store cannot answer is *unavailable*, not *missing*.

use super::Absent;
use anyhow::Result;

#[cfg(target_os = "macos")]
pub const NAME: &str = "macOS Keychain";
#[cfg(target_os = "linux")]
pub const NAME: &str = "Secret Service";
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub const NAME: &str = "OS credential store (unsupported here)";

#[cfg(target_os = "macos")]
mod imp {
    use super::Absent;
    use crate::credentials::SERVICE;
    use anyhow::{Context as _, Result};
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit};
    use security_framework::passwords;

    /// `errSecItemNotFound`.
    const NOT_FOUND: i32 = -25300;
    /// `errSecUserCanceled`: explicit cancellation, not an unavailable store.
    const USER_CANCELED: i32 = -128;
    /// `errSecAuthFailed`: access/authentication was refused.
    const AUTH_FAILED: i32 = -25293;

    pub fn read(name: &str) -> Result<String, Absent> {
        match passwords::get_generic_password(SERVICE, name) {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|_| Absent::Unavailable(format!("the Keychain item {name} is not UTF-8"))),
            Err(error) => Err(read_error(error.code(), name)),
        }
    }

    fn read_error(code: i32, name: &str) -> Absent {
        match code {
            NOT_FOUND => Absent::Missing,
            USER_CANCELED | AUTH_FAILED => Absent::Denied,
            _ => Absent::Unavailable(format!("the Keychain refused {name} (OSStatus {code})")),
        }
    }

    pub fn set(name: &str, value: &str) -> Result<()> {
        passwords::set_generic_password(SERVICE, name, value.as_bytes())
            .with_context(|| format!("storing {name} in the Keychain"))
    }

    pub fn delete(name: &str) -> Result<bool> {
        match passwords::delete_generic_password(SERVICE, name) {
            Ok(()) => Ok(true),
            Err(error) if error.code() == NOT_FOUND => Ok(false),
            Err(error) => Err(error).with_context(|| format!("removing {name} from the Keychain")),
        }
    }

    pub fn names() -> Result<Vec<String>> {
        let found = ItemSearchOptions::new()
            .class(ItemClass::generic_password())
            .service(SERVICE)
            .load_attributes(true)
            .limit(Limit::All)
            .search();
        let results = match found {
            Ok(results) => results,
            Err(error) if error.code() == NOT_FOUND => return Ok(Vec::new()),
            Err(error) => return Err(error).context("listing Deck's Keychain items"),
        };
        Ok(results
            .iter()
            .filter_map(|result| result.simplify_dict())
            .filter_map(|attributes| attributes.get("acct").cloned())
            .collect())
    }
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn cancelled_or_refused_access_is_distinct_from_a_locked_or_missing_store() {
            assert_eq!(read_error(USER_CANCELED, "KEY"), Absent::Denied);
            assert_eq!(read_error(NOT_FOUND, "KEY"), Absent::Missing);
            assert_eq!(read_error(AUTH_FAILED, "KEY"), Absent::Denied);
            assert!(matches!(read_error(-25308, "KEY"), Absent::Unavailable(_)));
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Absent;
    use crate::credentials::SERVICE;
    use anyhow::{Context as _, Result, bail};
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    fn tool() -> Command {
        let mut command = Command::new("secret-tool");
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        command
    }

    pub fn read(name: &str) -> Result<String, Absent> {
        let output = tool()
            .args(["lookup", "service", SERVICE, "name", name])
            .output()
            .map_err(|error| Absent::Unavailable(format!("running secret-tool: {error}")))?;
        if output.status.success() {
            let value = String::from_utf8(output.stdout)
                .map_err(|_| Absent::Unavailable(format!("the Secret Service item {name} is not UTF-8")))?;
            return Ok(value.strip_suffix('\n').map(str::to_owned).unwrap_or(value));
        }
        // `lookup` exits 1 with empty output both when there is no item and
        // when a matching item is locked but no prompter can unlock it. Probe
        // the item's metadata to distinguish those cases. A failed probe is
        // unavailable: only a successful empty search proves absence.
        if output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty() {
            classify_empty_lookup(name)
        } else {
            Err(Absent::Unavailable(format!("the Secret Service could not answer for {name}")))
        }
    }

    fn classify_empty_lookup(name: &str) -> Result<String, Absent> {
        let output = tool()
            .args(["search", "--all", "service", SERVICE, "name", name])
            .output()
            .map_err(|error| Absent::Unavailable(format!("running secret-tool: {error}")))?;
        empty_search(output.status.success(), &output.stdout, &output.stderr, name)
    }

    /// A locked item can be returned without its name attribute. Only an
    /// entirely empty successful search proves absence; diagnostic or partial
    /// output must never turn a store failure into advice to replace the key.
    fn empty_search(success: bool, stdout: &[u8], stderr: &[u8], name: &str) -> Result<String, Absent> {
        if success && stdout.is_empty() && stderr.is_empty() {
            Err(Absent::Missing)
        } else {
            Err(Absent::Unavailable(format!("the Secret Service could not answer for {name}")))
        }
    }

    pub fn set(name: &str, value: &str) -> Result<()> {
        let mut child = tool()
            .stdin(Stdio::piped())
            .args(["store", &format!("--label=Deck {name}"), "service", SERVICE, "name", name])
            .spawn()
            .context("running secret-tool")?;
        child.stdin.take().context("secret-tool stdin")?.write_all(value.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!("secret-tool store failed: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(())
    }

    pub fn delete(name: &str) -> Result<bool> {
        match read(name) {
            Ok(_) => {}
            Err(Absent::Missing) => return Ok(false),
            Err(Absent::Unavailable(_) | Absent::Denied) => {
                bail!("the Secret Service could not check {name} before deletion")
            }
        }
        let output =
            tool().args(["clear", "service", SERVICE, "name", name]).output().context("running secret-tool")?;
        if !output.status.success() {
            bail!("the Secret Service could not delete {name}");
        }
        Ok(true)
    }

    pub fn names() -> Result<Vec<String>> {
        let output = tool().args(["search", "--all", "service", SERVICE]).output().context("running secret-tool")?;
        search_names(output.status.success(), &output.stdout, &output.stderr)
    }

    /// libsecret prints attributes on stderr on Linux, and secrets on stdout.
    /// Accept attributes from either stream for compatibility, but never put
    /// command output into an error: it may contain a credential value.
    fn search_names(success: bool, stdout: &[u8], stderr: &[u8]) -> Result<Vec<String>> {
        if !success {
            bail!("the Secret Service could not list Deck credentials");
        }
        let mut names = Vec::new();
        for bytes in [stdout, stderr] {
            names.extend(
                String::from_utf8_lossy(bytes)
                    .lines()
                    .filter_map(|line| line.strip_prefix("attribute.name = "))
                    .map(|name| name.trim().to_string()),
            );
        }
        Ok(names)
    }

    #[cfg(test)]
    mod tests {
        use super::{Absent, empty_search, search_names};

        #[test]
        fn only_a_successful_completely_empty_search_proves_absence() {
            assert_eq!(empty_search(true, b"", b"", "KEY"), Err(Absent::Missing));
            for (success, stdout, stderr) in [
                (true, b"[/1]\nlabel = Deck KEY\n".as_slice(), b"".as_slice()),
                (true, b"".as_slice(), b"locked item diagnostic".as_slice()),
                (false, b"".as_slice(), b"".as_slice()),
            ] {
                assert!(matches!(empty_search(success, stdout, stderr, "KEY"), Err(Absent::Unavailable(_))));
            }
        }

        #[test]
        fn lists_attributes_from_stderr_without_returning_secrets() {
            let names = search_names(true, b"secret = synthetic-value\n", b"attribute.name = TEST_KEY\n").unwrap();
            assert_eq!(names, ["TEST_KEY"]);
            assert_eq!(search_names(true, b"attribute.name = OLD_KEY\n", b"").unwrap(), ["OLD_KEY"]);
        }

        #[test]
        fn unavailable_is_not_an_empty_store_and_errors_reveal_no_output() {
            let error = search_names(false, b"secret = synthetic-value", b"bus unavailable").unwrap_err();
            assert_eq!(error.to_string(), "the Secret Service could not list Deck credentials");
            assert!(search_names(false, b"", b"").is_err());
            assert!(search_names(false, b"attribute.name = PARTIAL_KEY\n", b"").is_err());
            assert!(search_names(true, b"", b"").unwrap().is_empty());
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::Absent;
    use anyhow::{Result, bail};

    pub fn read(_: &str) -> Result<String, Absent> {
        Err(Absent::Unavailable("this platform has no supported OS store; set DECK_CREDENTIALS".into()))
    }
    pub fn set(_: &str, _: &str) -> Result<()> {
        bail!("this platform has no supported OS store; set DECK_CREDENTIALS")
    }
    pub fn delete(_: &str) -> Result<bool> {
        bail!("this platform has no supported OS store; set DECK_CREDENTIALS")
    }
    pub fn names() -> Result<Vec<String>> {
        bail!("this platform has no supported OS store; set DECK_CREDENTIALS")
    }
}

pub fn read(name: &str) -> Result<String, Absent> {
    imp::read(name)
}

pub fn set(name: &str, value: &str) -> Result<()> {
    imp::set(name, value)
}

pub fn delete(name: &str) -> Result<bool> {
    imp::delete(name)
}

pub fn names() -> Result<Vec<String>> {
    imp::names()
}
