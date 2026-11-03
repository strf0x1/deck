//! Profiles as a compiled-in table (ADR 0004 §3).
//!
//! The design choice: profiles are Deck's list, not the guest's. The table is
//! part of the binary, so a task that names `registry` gets exactly these
//! destinations and nothing a guest or a project file added. An entry is a
//! name or one port on a name; expanding a profile never widens it.

/// `name=entry,entry` lines. A comment line starts with `#`.
const TABLE: &str = include_str!("profiles.txt");

/// Every entry of one profile, or `None` when Deck does not define it.
pub fn entries(profile: &str) -> Option<Vec<&'static str>> {
    TABLE
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case(profile))
        .map(|(_, list)| list.split(',').map(str::trim).filter(|entry| !entry.is_empty()).collect())
}

/// Every profile name, in table order.
pub fn names() -> Vec<&'static str> {
    TABLE
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(name, _)| name.trim())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::policy::Allowed;

    #[test]
    fn every_table_entry_is_a_parseable_destination() {
        let names = names();
        for wanted in ["repo", "registry", "package-mirrors", "os-mirrors", "docs"] {
            assert!(names.contains(&wanted), "{wanted} is missing from the table: {names:?}");
        }
        for name in names {
            let entries = entries(name).unwrap();
            assert!(!entries.is_empty(), "{name} has no destinations");
            for entry in entries {
                assert!(Allowed::parse(entry).is_some(), "{name} lists a malformed entry {entry:?}");
            }
        }
        assert!(entries("invented").is_none());
        assert_eq!(entries("REPO"), entries("repo"), "a name is case-insensitive");
    }
}
