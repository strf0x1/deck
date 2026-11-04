//! Why a GitHub operation did not happen, sorted by what the agent should do
//! next (ADR 0007 §7).
//!
//! The design choice: the client and the publisher say which kind of failure
//! each one is where they know it, and this module alone turns a kind into a
//! status, a code and `retryable`. A policy refusal, a branch that moved, a
//! malformed request and an outage then never read the same to the agent,
//! and every body has the shape the boundary denials already use.

use serde_json::{Value, json};

/// The most of GitHub's or git's own text a reply carries.
const MAX_DETAIL: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The policy says no: a branch, base or pull request the task may not
    /// touch, or a publish that would rewrite the branch. Do something else.
    Refused,
    /// The branch moved after it was read. Read it again, rebuild, retry.
    Conflict,
    /// The request itself is malformed: fix it.
    Invalid,
    /// GitHub has no such pull request or ref.
    NotFound,
    /// GitHub refused the request with this status (422 validation, a
    /// protected-branch hook), and its reason is the message.
    Rejected(u16),
    /// The host's credential is missing, or GitHub refused it. Not the
    /// agent's to fix.
    Credential,
    /// GitHub or git could not be reached or could not answer. Retry later.
    Unavailable,
}

impl Kind {
    pub fn status(self) -> u16 {
        match self {
            Self::Refused => 403,
            Self::Conflict => 409,
            Self::Invalid => 400,
            Self::NotFound => 404,
            Self::Rejected(status) => status,
            Self::Credential | Self::Unavailable => 502,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::Refused => "deck_github_refused",
            Self::Conflict => "deck_github_conflict",
            Self::Invalid => "deck_bad_request",
            Self::NotFound => "deck_github_not_found",
            Self::Rejected(_) => "deck_github_rejected",
            Self::Credential => "deck_github_credential",
            Self::Unavailable => "deck_github_failed",
        }
    }

    /// Whether the same request can succeed later without the agent changing
    /// it. A conflict is retryable only after the agent rebuilds, so it says
    /// so in `current` rather than here.
    pub fn retryable(self) -> bool {
        matches!(self, Self::Conflict | Self::Unavailable)
    }

    /// What a GitHub REST status means for the agent.
    pub fn of_status(status: u16) -> Self {
        match status {
            404 => Self::NotFound,
            401 | 403 => Self::Credential,
            409 | 422 => Self::Rejected(status),
            _ => Self::Unavailable,
        }
    }

    /// What a failed git transport command means, from its (redacted) stderr.
    pub fn of_git(stderr: &str) -> Self {
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("authentication failed")
            || lower.contains("returned error: 401")
            || lower.contains("returned error: 403")
            || lower.contains("permission to ")
        {
            Self::Credential
        } else if lower.contains("remote rejected") || lower.contains("declined") {
            Self::Rejected(422)
        } else {
            Self::Unavailable
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub kind: Kind,
    pub message: String,
    /// The commit the branch is at, when a rewrite or a conflict is the
    /// reason: what the agent builds on next.
    pub current: Option<String>,
}

impl Problem {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self { kind, message: bound(message.into()), current: None }
    }

    pub fn refused(message: impl Into<String>) -> Self {
        Self::new(Kind::Refused, message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(Kind::Invalid, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(Kind::Unavailable, message)
    }

    pub fn credential(message: impl Into<String>) -> Self {
        Self::new(Kind::Credential, message)
    }

    pub fn at(mut self, current: impl Into<String>) -> Self {
        self.current = Some(current.into());
        self
    }

    /// The reply body: `error.{message,type,code}`, `retryable`, and
    /// `current` when there is one.
    pub fn body(&self) -> Value {
        let code = self.kind.code();
        let mut body = json!({
            "error": { "message": self.message, "type": code, "code": code },
            "retryable": self.kind.retryable(),
        });
        if let Some(current) = &self.current {
            body["current"] = json!(current);
        }
        body
    }
}

/// GitHub's and git's text is passed through for its reason, but bounded.
fn bound(mut message: String) -> String {
    if message.len() > MAX_DETAIL {
        let mut end = MAX_DETAIL;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push('…');
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_has_its_own_status_and_code() {
        let kinds = [
            Kind::Refused,
            Kind::Conflict,
            Kind::Invalid,
            Kind::NotFound,
            Kind::Rejected(422),
            Kind::Credential,
            Kind::Unavailable,
        ];
        let codes: std::collections::HashSet<_> = kinds.iter().map(|kind| kind.code()).collect();
        assert_eq!(codes.len(), kinds.len());
        assert_eq!(Kind::Refused.status(), 403);
        assert_eq!(Kind::Conflict.status(), 409);
        assert!(Kind::Conflict.retryable() && Kind::Unavailable.retryable());
        assert!(!Kind::Refused.retryable() && !Kind::Credential.retryable());
    }

    #[test]
    fn a_body_carries_retryable_and_the_current_commit() {
        let body = Problem::refused("no").at("a".repeat(40)).body();
        assert_eq!(body["error"]["code"], "deck_github_refused");
        assert_eq!(body["retryable"], false);
        assert_eq!(body["current"], "a".repeat(40));
        assert!(Problem::invalid("x").body().get("current").is_none());
    }

    #[test]
    fn github_and_git_failures_are_sorted() {
        assert_eq!(Kind::of_status(404), Kind::NotFound);
        assert_eq!(Kind::of_status(422), Kind::Rejected(422));
        assert_eq!(Kind::of_status(401), Kind::Credential);
        assert_eq!(Kind::of_status(503), Kind::Unavailable);
        assert_eq!(Kind::of_git("fatal: Authentication failed for 'https://github.com/o/r/'"), Kind::Credential);
        assert_eq!(Kind::of_git(" ! [remote rejected] x -> x (protected branch hook declined)"), Kind::Rejected(422));
        assert_eq!(Kind::of_git("fatal: unable to access: Could not resolve host"), Kind::Unavailable);
    }

    #[test]
    fn a_long_reason_is_cut_on_a_character() {
        let message = Problem::unavailable("é".repeat(MAX_DETAIL)).message;
        assert!(message.len() <= MAX_DETAIL + '…'.len_utf8() && message.ends_with('…'));
    }
}
