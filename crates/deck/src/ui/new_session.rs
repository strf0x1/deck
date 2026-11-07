//! What the New session buttons show: ready, creating a task, or the reason
//! the last create failed.
//!
//! The design choice: a failed create is not the session's failure. The
//! window stays on the task it had, so the reason is a header notice that
//! the next click clears, never `Inbound::Failed` on the live session.

/// The New session control's state (`attachment.rs` drives it).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NewSession {
    #[default]
    Idle,
    /// `deckd` is creating the task; further clicks are ignored.
    Creating,
    /// The last create failed; the text is for the user.
    Failed(String),
}

impl NewSession {
    pub fn is_creating(&self) -> bool {
        matches!(self, NewSession::Creating)
    }

    /// The header button's label.
    pub fn label(&self) -> &'static str {
        match self {
            NewSession::Creating => "Starting…",
            NewSession::Idle | NewSession::Failed(_) => "New session",
        }
    }

    /// The header notice, while the last create's failure stands.
    pub fn error(&self) -> Option<&str> {
        match self {
            NewSession::Failed(e) => Some(e),
            NewSession::Idle | NewSession::Creating => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_create_in_flight_blocks_another_and_only_a_failure_has_a_notice() {
        assert!(!NewSession::Idle.is_creating());
        assert!(NewSession::Creating.is_creating());
        assert!(!NewSession::Failed("x".into()).is_creating(), "a failure can be retried");
        assert_eq!(NewSession::Creating.label(), "Starting…");
        assert_eq!(NewSession::Failed("no daemon".into()).label(), "New session");
        assert_eq!(NewSession::Failed("no daemon".into()).error(), Some("no daemon"));
        assert_eq!(NewSession::Idle.error(), None);
    }
}
