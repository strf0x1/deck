//! Just enough of git's bundle v2 format to know what a guest sent (ADR 0007
//! §2).
//!
//! The design choice: the host reads the header itself instead of running
//! `git` on untrusted input to discover what is inside. A bundle names its
//! refs in one `# v2 git bundle` header; the gateway refuses a body that is not
//! exactly one ref, or whose ref is not the branch the operation may write,
//! before git is invoked at all. The object data is not parsed here — `git`
//! validates the pack when the host fetches it with a refspec naming only the
//! one allowed ref, so a bundle carrying extra objects cannot publish them.

/// v2 is the current format and the only one the guest build produces.
const MAGIC: &[u8] = b"# v2 git bundle\n";
/// A bundle header is a handful of lines plus attribute lines; a guest that
/// writes more is not sending a commit this task can afford to inspect.
pub const MAX_HEADER: usize = 64 * 1024;
/// One published commit is a source tree, not an archive.
pub const MAX_BUNDLE: usize = 16 * 1024 * 1024;

/// What a bundle's header declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    /// The refs the bundle carries, in header order.
    pub refs: Vec<(String, String)>,
    /// `Some(sha)` for a bundle that carries an explicit prerequisite.
    pub prerequisite: Option<String>,
}

impl Bundle {
    /// Parse a bundle v2 body. The header is the magic line, then one
    /// `<sha> <ref>` line per contained ref, then a blank line, then the pack;
    /// `#`-prefixed capability lines (git's `--header-only` form) are read
    /// too. Anything else before the pack is not a header this gateway
    /// understands, and the bundle is refused rather than guessed at.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_BUNDLE {
            return Err(format!("the bundle exceeds {MAX_BUNDLE} bytes"));
        }
        let Some(rest) = bytes.strip_prefix(MAGIC) else {
            return Err("the body is not a git bundle v2".to_string());
        };
        let mut refs = Vec::new();
        let mut prerequisite = None;
        let mut rest = rest;
        while let Some(offset) = rest.iter().position(|&byte| byte == b'\n') {
            let line = &rest[..offset];
            rest = &rest[offset + 1..];
            if line.is_empty() {
                break;
            }
            let Ok(line) = std::str::from_utf8(line) else { break };
            if let Some(capability) = line.strip_prefix("# ") {
                if capability.starts_with("object-format=") {
                    if capability != "object-format=sha1" {
                        return Err("the bundle does not use sha1 objects; this gateway takes sha1".to_string());
                    }
                    continue;
                }
                let mut words = capability.split_whitespace();
                match (words.next(), words.next(), words.next(), words.next()) {
                    (Some("ref"), Some(sha), Some(name), None) => refs.push((name.to_string(), sha.to_string())),
                    (Some("prerequisite"), Some(sha), Some(_), None) => prerequisite = Some(sha.to_string()),
                    _ => {}
                }
                continue;
            }
            // The unpacked form: `<sha> <ref>` per line.
            match line.split_once(' ') {
                Some((sha, name)) if is_sha(sha) && name.starts_with("refs/") => {
                    refs.push((name.to_string(), sha.to_string()))
                }
                _ => return Err(format!("{line:?} is not a bundle header line")),
            }
        }
        Ok(Self { refs, prerequisite })
    }

    /// The one branch the bundle may publish, or why it may not. A bundle with
    /// two refs, a tag, or a name other than `refs/heads/<branch>` fails here,
    /// so a push cannot widen what the policy allowed.
    pub fn branch(&self, allowed: &str) -> Result<String, String> {
        let wanted = format!("refs/heads/{allowed}");
        match self.refs.as_slice() {
            [] => Err("the bundle carries no ref".to_string()),
            [(name, _)] if *name == wanted => Ok(allowed.to_string()),
            [(name, _)] => Err(format!("the bundle publishes {name}, not {wanted}")),
            refs => {
                Err(format!("the bundle carries {} refs; one commit to one branch is all a publish takes", refs.len()))
            }
        }
    }
}

/// A commit as a bundle names it: forty lowercase hex digits.
pub fn is_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape `git bundle create` writes: the magic, one `<sha> <ref>`
    /// line per ref, a blank line, then the pack.
    fn bundle(refs: &[&str]) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        for (index, name) in refs.iter().enumerate() {
            out.extend_from_slice(format!("{:040} {name}\n", index + 1).as_bytes());
        }
        out.extend_from_slice(b"\nPACK\x00\x00\x00\x02");
        out
    }

    #[test]
    fn a_capability_header_is_read_too() {
        let raw = b"# v2 git bundle\n# object-format=sha1\n# ref 66df49954fb3bef9ca4118458c1b52368c3c693c refs/heads/deck/t-1\n\nPACK";
        let parsed = Bundle::parse(raw).unwrap();
        assert_eq!(parsed.branch("deck/t-1").unwrap(), "deck/t-1");
        let sha256 = b"# v2 git bundle\n# object-format=sha256\n# ref 66df49954fb3bef9ca4118458c1b52368c3c693c refs/heads/x\n\nPACK";
        assert!(Bundle::parse(sha256).is_err());
    }

    #[test]
    fn a_line_that_is_not_a_header_ref_is_refused() {
        let raw = b"# v2 git bundle\nnot a header line\n\nPACK";
        assert!(Bundle::parse(raw).is_err());
        let short = b"# v2 git bundle\nabc refs/heads/x\n\nPACK";
        assert!(Bundle::parse(short).is_err());
    }

    #[test]
    fn one_branch_is_read_from_the_header() {
        let parsed = Bundle::parse(&bundle(&["refs/heads/deck/t-1"])).unwrap();
        assert_eq!(parsed.refs.len(), 1);
        assert_eq!(parsed.branch("deck/t-1").unwrap(), "deck/t-1");
        assert_eq!(parsed.prerequisite, None);
    }

    #[test]
    fn a_second_ref_or_another_name_is_refused() {
        let two = Bundle::parse(&bundle(&["refs/heads/deck/t-1", "refs/heads/main"])).unwrap();
        assert!(two.branch("deck/t-1").unwrap_err().contains("2 refs"));
        let other = Bundle::parse(&bundle(&["refs/heads/main"])).unwrap();
        assert!(other.branch("deck/t-1").unwrap_err().contains("not refs/heads/deck/t-1"));
        let tag = Bundle::parse(&bundle(&["refs/tags/v1"])).unwrap();
        assert!(tag.branch("deck/t-1").is_err(), "a tag is not a branch");
        assert!(Bundle::parse(&bundle(&[])).unwrap().branch("deck/t-1").is_err());
    }

    #[test]
    fn a_not_a_bundle_or_an_oversized_one_is_refused() {
        assert!(Bundle::parse(b"PACK\x00\x00\x00\x02").is_err());
        assert!(Bundle::parse(b"# v3 git bundle\n").is_err());
        assert!(Bundle::parse(&vec![b'x'; MAX_BUNDLE + 1]).is_err());
    }

    #[test]
    fn a_prerequisite_is_read_so_the_fetch_can_use_it() {
        let text = format!(
            "# v2 git bundle\n# ref {sha} refs/heads/deck/t-1\n# prerequisite {sha} HEAD\n\nPACK",
            sha = "a".repeat(40)
        );
        let parsed = Bundle::parse(text.as_bytes()).unwrap();
        assert_eq!(parsed.prerequisite.as_deref(), Some("a".repeat(40).as_str()));
    }
}
