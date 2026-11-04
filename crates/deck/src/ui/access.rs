//! The access panel's state: this task's credentials as the daemon lists
//! them, and the revoke confirmation.
//!
//! The design choice: the panel is a mirror of the daemon's listing, never a
//! store. It holds names, fingerprints and revoked flags and nothing that
//! could be a value (ADR 0004 §4.2). A revoke is two steps, ask then
//! confirm, and what follows it always says that revoking here does not
//! cancel the key at the provider (§4.5). Pure state, so the flow is
//! testable without a window.

use deck_runtime::broker::Listed;

/// Said whenever a revoke is offered or done: the daemon stops using the
/// value, the provider still honours it.
pub const NOT_CANCELLED: &str = "Revoking stops Deck using this value. It does not cancel the key at the provider.";

#[derive(Default)]
pub struct Access {
    open: bool,
    credentials: Vec<Listed>,
    confirming: Option<String>,
    revoked: Option<String>,
    error: Option<String>,
    busy: bool,
}

impl Access {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open or close the panel; closing drops any half-made decision.
    pub fn toggle(&mut self) -> bool {
        self.open = !self.open;
        if !self.open {
            self.cancel();
            self.revoked = None;
            self.error = None;
        }
        self.open
    }

    pub fn credentials(&self) -> &[Listed] {
        &self.credentials
    }

    pub fn confirming(&self) -> Option<&str> {
        self.confirming.as_deref()
    }

    /// The name just revoked, whose provider-side rotation is the user's.
    pub fn revoked(&self) -> Option<&str> {
        self.revoked.as_deref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn busy(&self) -> bool {
        self.busy
    }

    /// A fresh listing, or why there is none. A listing that fails keeps the
    /// last one, so a daemon blip does not blank the panel.
    pub fn listed(&mut self, result: Result<Vec<Listed>, String>) {
        match result {
            Ok(list) => {
                self.credentials = list;
                self.error = None;
                // A name that is no longer listed or already revoked has
                // nothing left to confirm.
                if self.confirming.as_deref().is_some_and(|name| !self.revocable(name)) {
                    self.confirming = None;
                }
            }
            Err(why) => self.error = Some(why),
        }
    }

    fn revocable(&self, name: &str) -> bool {
        self.credentials.iter().any(|item| item.name == name && item.fingerprint.is_some() && !item.revoked)
    }

    /// Start a revoke of `name`; only a name with a current, unrevoked value
    /// can be. Returns whether the confirmation opened.
    pub fn ask_revoke(&mut self, name: &str) -> bool {
        if self.busy || !self.revocable(name) {
            return false;
        }
        self.confirming = Some(name.to_owned());
        self.revoked = None;
        self.error = None;
        true
    }

    pub fn cancel(&mut self) -> bool {
        self.confirming.take().is_some()
    }

    /// The confirmed name, now in flight. Nothing is revoked until the
    /// daemon says so.
    pub fn begin_revoke(&mut self) -> Option<String> {
        if self.busy {
            return None;
        }
        let name = self.confirming.clone()?;
        self.busy = true;
        Some(name)
    }

    /// The daemon's answer to a confirmed revoke: the new listing, or why it
    /// failed. On failure the confirmation stays so the user can retry.
    pub fn revoke_done(&mut self, result: Result<Vec<Listed>, String>) {
        self.busy = false;
        match result {
            Ok(list) => {
                self.revoked = self.confirming.take();
                self.credentials = list;
                self.error = None;
            }
            Err(why) => self.error = Some(why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, fingerprint: Option<&str>, revoked: bool) -> Listed {
        Listed { name: name.into(), fingerprint: fingerprint.map(Into::into), revoked, error: None }
    }

    fn panel() -> Access {
        let mut access = Access::new();
        access.listed(Ok(vec![
            item("OPENAI_API_KEY", Some("sha256:aaaaaaaaaaaa"), false),
            item("OLD", Some("sha256:bbbbbbbbbbbb"), true),
            item("EMPTY", None, false),
        ]));
        access
    }

    #[test]
    fn only_a_live_value_can_be_revoked() {
        let mut access = panel();
        assert!(!access.ask_revoke("OLD") && !access.ask_revoke("EMPTY") && !access.ask_revoke("NOPE"));
        assert!(access.confirming().is_none());
        assert!(access.ask_revoke("OPENAI_API_KEY"));
        assert_eq!(access.confirming(), Some("OPENAI_API_KEY"));
        assert!(access.cancel() && access.confirming().is_none());
    }

    #[test]
    fn a_confirmed_revoke_waits_for_the_daemon_and_then_reports_the_new_listing() {
        let mut access = panel();
        assert!(access.begin_revoke().is_none(), "nothing to confirm");
        access.ask_revoke("OPENAI_API_KEY");
        assert_eq!(access.begin_revoke().as_deref(), Some("OPENAI_API_KEY"));
        assert!(access.busy() && access.begin_revoke().is_none(), "one in flight at a time");
        assert!(!access.credentials()[0].revoked, "not revoked until the daemon says so");
        access.revoke_done(Ok(vec![item("OPENAI_API_KEY", Some("sha256:aaaaaaaaaaaa"), true)]));
        assert!(!access.busy() && access.confirming().is_none());
        assert_eq!(access.revoked(), Some("OPENAI_API_KEY"));
        assert!(access.credentials()[0].revoked);
    }

    #[test]
    fn a_failed_revoke_keeps_the_confirmation_and_says_why() {
        let mut access = panel();
        access.ask_revoke("OPENAI_API_KEY");
        access.begin_revoke();
        access.revoke_done(Err("deckd is gone".into()));
        assert_eq!(
            (access.confirming(), access.revoked(), access.error()),
            (Some("OPENAI_API_KEY"), None, Some("deckd is gone"))
        );
        assert!(!access.credentials()[0].revoked && !access.busy());
    }

    #[test]
    fn a_failed_listing_keeps_the_last_one_and_closing_clears_the_decision() {
        let mut access = panel();
        access.toggle();
        access.ask_revoke("OPENAI_API_KEY");
        access.listed(Err("no daemon".into()));
        assert_eq!((access.credentials().len(), access.error()), (3, Some("no daemon")));
        // The value rotated elsewhere and was revoked: nothing left to confirm.
        access.listed(Ok(vec![item("OPENAI_API_KEY", Some("sha256:aaaaaaaaaaaa"), true)]));
        assert!(access.confirming().is_none());
        access.ask_revoke("OPENAI_API_KEY");
        access.toggle();
        assert!(!access.is_open() && access.confirming().is_none() && access.error().is_none());
    }
}
