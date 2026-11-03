//! The pure policy core: which model a task may use, what it may spend, and
//! which guest headers may cross the boundary.
//!
//! The design choice: the guest header rules are an allowlist, not a
//! denylist. A new credential header a future client invents is dropped by
//! default; only attribution headers survive, and the host always replaces
//! `Authorization` after the guest's copy was discarded. Budgets are exact
//! counts for one task, never wildcards or provider-wide limits, and the
//! ledger counts an admitted request before any provider is contacted.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Mutex, RwLock};

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

/// The task's model limits. A missing field is unlimited; a limit of zero
/// denies the first request. There are no wildcard or provider-wide budgets:
/// one ledger belongs to one task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub requests: Option<u64>,
    pub tokens: Option<u64>,
}

impl Budget {
    /// The budget in a task's host-side file, if it is readable and valid.
    /// A missing or malformed file leaves the process-start limits in place
    /// rather than silently widening them.
    pub fn load(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let object = value.as_object()?;
        // The writer always emits both keys. A partial snapshot is not a
        // budget, because the missing half would silently become unlimited.
        if !object.contains_key("requests") || !object.contains_key("tokens") {
            return None;
        }
        serde_json::from_value(value).ok()
    }
}

/// What one task has spent. The gateway owns the live counters and reports
/// the totals to `deckd`, which records them on the task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub requests: u64,
    pub tokens: u64,
}

/// Which limit refused a request, for the provider-facing denial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exhausted {
    Requests { used: u64, limit: u64 },
    Tokens { used: u64, limit: u64 },
}

impl Exhausted {
    pub fn message(self) -> String {
        match self {
            Self::Requests { used, limit } => {
                format!("the task's model request budget is spent ({used} of {limit}); no provider request was made")
            }
            Self::Tokens { used, limit } => {
                format!("the task's model token budget is spent ({used} of {limit}); no provider request was made")
            }
        }
    }
}

/// One task's budget and counters. Admission is the single point that both
/// checks the limits and increments the request count, so a request that is
/// denied never consumes anything and an admitted one cannot race a second
/// request past the same last slot.
pub struct Ledger {
    budget: RwLock<Budget>,
    used: Mutex<Usage>,
}

impl Ledger {
    pub fn new(budget: Budget, used: Usage) -> Self {
        Self { budget: RwLock::new(budget), used: Mutex::new(used) }
    }

    /// Replace the limits without touching the counters. The next admission
    /// uses the new values; in-flight requests already admitted are not
    /// retracted.
    pub fn set_budget(&self, budget: Budget) {
        *self.budget.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = budget;
    }

    pub fn budget(&self) -> Budget {
        *self.budget.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn usage(&self) -> Usage {
        *self.used.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Count one admitted request. A spent request or token budget denies it
    /// before the provider is contacted and leaves the counters unchanged.
    pub fn admit(&self) -> Result<Usage, Exhausted> {
        let budget = self.budget();
        let mut used = self.used.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(limit) = budget.requests
            && used.requests >= limit
        {
            return Err(Exhausted::Requests { used: used.requests, limit });
        }
        if let Some(limit) = budget.tokens
            && used.tokens >= limit
        {
            return Err(Exhausted::Tokens { used: used.tokens, limit });
        }
        used.requests = used.requests.saturating_add(1);
        Ok(*used)
    }

    /// Add what the provider reported for the response just streamed.
    pub fn charge(&self, tokens: u64) -> Usage {
        let mut used = self.used.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        used.tokens = used.tokens.saturating_add(tokens);
        *used
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

    #[test]
    fn requests_are_counted_on_admission_and_denied_when_spent() {
        let ledger = Ledger::new(Budget { requests: Some(2), tokens: None }, Usage::default());
        assert_eq!(ledger.admit().unwrap(), Usage { requests: 1, tokens: 0 });
        assert_eq!(ledger.admit().unwrap(), Usage { requests: 2, tokens: 0 });
        assert_eq!(ledger.admit(), Err(Exhausted::Requests { used: 2, limit: 2 }));
        assert_eq!(ledger.usage(), Usage { requests: 2, tokens: 0 }, "a denial consumes nothing");
    }

    #[test]
    fn tokens_are_charged_and_deny_the_next_request_once_spent() {
        let ledger = Ledger::new(Budget { requests: None, tokens: Some(10) }, Usage::default());
        assert_eq!(ledger.admit().unwrap().requests, 1);
        assert_eq!(ledger.charge(6), Usage { requests: 1, tokens: 6 });
        assert_eq!(ledger.admit().unwrap().requests, 2);
        assert_eq!(ledger.charge(4), Usage { requests: 2, tokens: 10 });
        assert_eq!(ledger.admit(), Err(Exhausted::Tokens { used: 10, limit: 10 }));
    }

    #[test]
    fn zero_limits_deny_and_missing_limits_are_unlimited() {
        let zero = Ledger::new(Budget { requests: Some(0), tokens: Some(0) }, Usage::default());
        assert_eq!(zero.admit(), Err(Exhausted::Requests { used: 0, limit: 0 }));
        let open = Ledger::new(Budget::default(), Usage::default());
        assert!(open.admit().is_ok());
        assert!(open.charge(u64::MAX).tokens == u64::MAX);
        assert!(open.charge(u64::MAX).tokens == u64::MAX, "token totals saturate instead of wrapping");
    }

    #[test]
    fn a_partial_or_invalid_budget_file_loads_nothing() {
        let dir = std::env::temp_dir().join(format!("deck-budget-policy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("budget.json");
        std::fs::write(&path, r#"{"requests":2}"#).unwrap();
        assert_eq!(Budget::load(&path), None, "a missing token limit must not become unlimited");
        std::fs::write(&path, r#"{"requests":2,"tokens":null}"#).unwrap();
        assert_eq!(Budget::load(&path), Some(Budget { requests: Some(2), tokens: None }));
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Budget::load(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ledgers_are_per_task_and_keep_prior_usage() {
        let first = Ledger::new(Budget { requests: Some(1), tokens: None }, Usage { requests: 1, tokens: 0 });
        assert_eq!(first.admit(), Err(Exhausted::Requests { used: 1, limit: 1 }));
        let second = Ledger::new(Budget { requests: Some(1), tokens: None }, Usage::default());
        assert!(second.admit().is_ok(), "another task has its own counters");
        first.set_budget(Budget { requests: Some(2), tokens: None });
        assert!(first.admit().is_ok(), "a raised limit applies to the next admission");
    }
}
