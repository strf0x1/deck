//! The pure policy core: which model a task may use and which guest headers
//! may cross the boundary.
//!
//! The design choice: the guest header rules are an allowlist, not a
//! denylist. A new credential header a future client invents is dropped by
//! default; only attribution headers survive, and the host always replaces
//! `Authorization` after the guest's copy was discarded.

/// Headers Construct may send that carry no secret and are useful upstream.
const ATTRIBUTION: [&str; 3] = ["http-referer", "x-title", "user-agent"];

/// Request headers the model gateway keeps. Everything else — `Authorization`,
/// `Proxy-Authorization`, `Cookie`, `x-api-key`, `api-key`, hop-by-hop
/// headers, and any header a guest invents — is dropped before the host
/// credential is added.
const RETAINED: [&str; 5] = ["content-type", "accept", "http-referer", "x-title", "user-agent"];

/// The task's models. An entry is the full name Construct sends in the
/// request body; there are no wildcards, because a wildcard would turn one
/// approved model into every model a provider offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Models {
    allowed: Vec<String>,
}

impl Models {
    pub fn new(allowed: impl IntoIterator<Item = String>) -> Self {
        Self { allowed: allowed.into_iter().collect() }
    }

    pub fn allows(&self, model: &str) -> bool {
        self.allowed.iter().any(|allowed| allowed == model)
    }

    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty()
    }

    pub fn names(&self) -> &[String] {
        &self.allowed
    }
}

/// Whether a guest request header may be forwarded upstream. `name` must
/// already be lowercase.
pub fn retained(name: &str) -> bool {
    RETAINED.contains(&name)
}

/// Whether a header is one of the non-secret attribution headers ADR 0004
/// names; used by tests and future gateway kinds to keep the rule explicit.
pub fn attribution(name: &str) -> bool {
    ATTRIBUTION.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_match_exactly_and_never_by_wildcard() {
        let models = Models::new(["openrouter/free".to_string()]);
        assert!(models.allows("openrouter/free"));
        assert!(!models.allows("openrouter/free-2"));
        assert!(!models.allows("*"));
        assert!(!models.allows(""));
        assert!(!Models::new([]).allows("openrouter/free"));
    }

    #[test]
    fn credential_headers_are_not_retained() {
        for name in ["authorization", "proxy-authorization", "cookie", "x-api-key", "api-key", "x-goog-api-key"] {
            assert!(!retained(name), "{name} must not cross the boundary");
        }
    }

    #[test]
    fn attribution_and_content_headers_survive() {
        for name in ["content-type", "accept", "http-referer", "x-title", "user-agent"] {
            assert!(retained(name), "{name} is safe to forward");
        }
        assert!(attribution("http-referer"));
        assert!(attribution("x-title"));
        assert!(!attribution("authorization"));
    }

    #[test]
    fn hop_by_hop_and_unknown_headers_are_dropped() {
        for name in ["connection", "keep-alive", "transfer-encoding", "te", "trailer", "upgrade", "x-invented"] {
            assert!(!retained(name), "{name} must not be forwarded");
        }
    }
}
