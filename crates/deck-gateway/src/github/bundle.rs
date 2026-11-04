//! Just enough of git's bundle v2 format to know what a guest sent (ADR 0007
//! §2).
//!
//! The design choice: the host reads the header itself instead of running
//! `git` on untrusted input to discover what is inside, and it reads exactly
//! the lines `git bundle create` writes — the `# v2 git bundle` magic, one
//! `-<sha> [comment]` line per prerequisite, one `<sha> <ref>` line per ref,
//! then a blank line — and refuses anything else rather than guessing. The
//! gateway refuses a body that is not exactly one ref, or whose ref is not the
//! branch the operation may write, before git is invoked at all. The object
//! data is not parsed here: `git` validates the pack when the host fetches it
//! with a refspec naming only the one allowed ref, so a bundle carrying extra
//! objects cannot publish them.

/// v2 is the format `git bundle create` writes for sha1 repositories, and the
/// only one this gateway takes.
const MAGIC: &[u8] = b"# v2 git bundle\n";
/// One published commit is a source tree, not an archive.
pub const MAX_BUNDLE: usize = 16 * 1024 * 1024;

/// What a bundle's header declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    /// The refs the bundle carries, in header order, as `(name, sha)`.
    pub refs: Vec<(String, String)>,
    /// The commits the receiving repository must already have: an
    /// incremental bundle (`base..branch`) names the published tip here.
    pub prerequisites: Vec<String>,
}

impl Bundle {
    /// Parse a bundle v2 body's header. Any line that is not a prerequisite,
    /// a ref or the blank line that ends the header is refused.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_BUNDLE {
            return Err(format!("the bundle exceeds {MAX_BUNDLE} bytes"));
        }
        let Some(mut rest) = bytes.strip_prefix(MAGIC) else {
            return Err("the body is not a git bundle v2".to_string());
        };
        let mut refs = Vec::new();
        let mut prerequisites = Vec::new();
        loop {
            let Some(offset) = rest.iter().position(|&byte| byte == b'\n') else {
                return Err("the bundle header has no end".to_string());
            };
            let line = &rest[..offset];
            rest = &rest[offset + 1..];
            if line.is_empty() {
                break;
            }
            let Ok(line) = std::str::from_utf8(line) else {
                return Err("a bundle header line is not UTF-8".to_string());
            };
            if let Some(prerequisite) = line.strip_prefix('-') {
                // `-<sha>` optionally followed by a space and the commit's
                // subject, which is free text.
                let sha = prerequisite.split_once(' ').map_or(prerequisite, |(sha, _)| sha);
                if !is_sha(sha) {
                    return Err(format!("{line:?} is not a bundle prerequisite"));
                }
                prerequisites.push(sha.to_string());
                continue;
            }
            match line.split_once(' ') {
                Some((sha, name)) if is_sha(sha) && name.starts_with("refs/") && !name.contains(' ') => {
                    refs.push((name.to_string(), sha.to_string()))
                }
                _ => return Err(format!("{line:?} is not a bundle header line")),
            }
        }
        Ok(Self { refs, prerequisites })
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

/// A commit as a sha1 bundle names it: forty hex digits.
pub fn is_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape `git bundle create` writes: the magic, prerequisites, one
    /// `<sha> <ref>` line per ref, a blank line, then the pack.
    fn bundle(prerequisites: &[&str], refs: &[&str]) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        for line in prerequisites {
            out.extend_from_slice(format!("{line}\n").as_bytes());
        }
        for (index, name) in refs.iter().enumerate() {
            out.extend_from_slice(format!("{:040} {name}\n", index + 1).as_bytes());
        }
        out.extend_from_slice(b"\nPACK\x00\x00\x00\x02");
        out
    }

    #[test]
    fn one_branch_is_read_from_the_header() {
        let parsed = Bundle::parse(&bundle(&[], &["refs/heads/deck/t-1"])).unwrap();
        assert_eq!(parsed.refs.len(), 1);
        assert_eq!(parsed.branch("deck/t-1").unwrap(), "deck/t-1");
        assert!(parsed.prerequisites.is_empty());
    }

    #[test]
    fn an_incremental_bundle_names_its_prerequisites() {
        let sha = "a".repeat(40);
        let with_subject = format!("-{sha} Publish the first commit");
        let bare = format!("-{}", "b".repeat(40));
        let parsed = Bundle::parse(&bundle(&[&with_subject, &bare], &["refs/heads/deck/t-1"])).unwrap();
        assert_eq!(parsed.prerequisites, vec![sha, "b".repeat(40)]);
        assert_eq!(parsed.branch("deck/t-1").unwrap(), "deck/t-1");
        assert!(Bundle::parse(&bundle(&["-abc subject"], &["refs/heads/x"])).is_err());
    }

    #[test]
    fn a_line_git_does_not_write_is_refused() {
        let sha = "6".repeat(40);
        for raw in [
            b"# v2 git bundle\nnot a header line\n\nPACK".to_vec(),
            b"# v2 git bundle\nabc refs/heads/x\n\nPACK".to_vec(),
            format!("# v2 git bundle\n{sha} HEAD\n\nPACK").into_bytes(),
            // The v3 capability form is not something a sha1 v2 bundle has.
            format!("# v2 git bundle\n# ref {sha} refs/heads/deck/t-1\n\nPACK").into_bytes(),
            [format!("# v2 git bundle\n{sha} refs/heads/deck/t-1\n").as_bytes(), b"\xff refs/heads/main\n\nPACK"]
                .concat(),
            // A header that never ends is not a bundle.
            format!("# v2 git bundle\n{sha} refs/heads/deck/t-1\n").into_bytes(),
        ] {
            assert!(Bundle::parse(&raw).is_err(), "{:?}", String::from_utf8_lossy(&raw));
        }
    }

    #[test]
    fn a_second_ref_or_another_name_is_refused() {
        let two = Bundle::parse(&bundle(&[], &["refs/heads/deck/t-1", "refs/heads/main"])).unwrap();
        assert!(two.branch("deck/t-1").unwrap_err().contains("2 refs"));
        let other = Bundle::parse(&bundle(&[], &["refs/heads/main"])).unwrap();
        assert!(other.branch("deck/t-1").unwrap_err().contains("not refs/heads/deck/t-1"));
        let tag = Bundle::parse(&bundle(&[], &["refs/tags/v1"])).unwrap();
        assert!(tag.branch("deck/t-1").is_err(), "a tag is not a branch");
        assert!(Bundle::parse(&bundle(&[], &[])).unwrap().branch("deck/t-1").is_err());
    }

    #[test]
    fn a_not_a_bundle_or_an_oversized_one_is_refused() {
        assert!(Bundle::parse(b"PACK\x00\x00\x00\x02").is_err());
        assert!(Bundle::parse(b"# v3 git bundle\n").is_err());
        assert!(Bundle::parse(&vec![b'x'; MAX_BUNDLE + 1]).is_err());
    }
}
