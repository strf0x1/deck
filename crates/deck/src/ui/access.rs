//! The access panel's daemon-owned listings and two-step revoke state.
//!
//! The design choice: a confirmation names either a credential or an exact
//! grant. The in-flight target is kept separately, so closing the panel or a
//! late listing cannot change what a revoke result refers to. Request tickets
//! discard listings started before a revoke or a newer refresh.

use deck_runtime::approvals::{Grant, Scope};
use deck_runtime::broker::Listed;
use deck_runtime::github::Approvals;

pub const NOT_CANCELLED: &str = "Revoking stops Deck using this value. It does not cancel the key at the provider.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revoke {
    Credential(String),
    Grant(Grant),
}

impl Revoke {
    pub fn label(&self) -> String {
        match self {
            Self::Credential(name) => name.clone(),
            Self::Grant(grant) => format!("{} to {}", grant.operation, grant.destination),
        }
    }

    pub fn explanation(&self) -> String {
        match self {
            Self::Credential(_) => format!("Requests using this value stop now. {NOT_CANCELLED}"),
            Self::Grant(grant) => {
                let scope = if grant.scope == Scope::Project {
                    "This removes the saved rule and matching task grants for every task in this project."
                } else {
                    "This task will need approval again for matching requests."
                };
                format!(
                    "{scope} Existing web connections authorized by these grants close. Completed operations cannot be undone."
                )
            }
        }
    }
}

#[derive(Default)]
pub struct Access {
    open: bool,
    credentials: Vec<Listed>,
    grants: Vec<Grant>,
    confirming: Option<Revoke>,
    in_flight: Option<Revoke>,
    revoked: Option<Revoke>,
    credentials_request: u64,
    grants_request: u64,
    credentials_loading: bool,
    grants_loading: bool,
    credentials_error: Option<String>,
    grants_error: Option<String>,
    revoke_error: Option<String>,
    retrying: Option<String>,
    retry_message: Option<String>,
    approvals: Option<Approvals>,
    approvals_error: Option<String>,
    approvals_request: u64,
}

impl Access {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn credentials(&self) -> &[Listed] {
        &self.credentials
    }
    pub fn grants(&self) -> &[Grant] {
        &self.grants
    }
    pub fn confirming(&self) -> Option<&Revoke> {
        self.confirming.as_ref()
    }
    pub fn revoked(&self) -> Option<&Revoke> {
        self.revoked.as_ref()
    }
    pub fn busy(&self) -> bool {
        self.in_flight.is_some()
    }
    pub fn credentials_loading(&self) -> bool {
        self.credentials_loading
    }
    pub fn grants_loading(&self) -> bool {
        self.grants_loading
    }
    pub fn credentials_error(&self) -> Option<&str> {
        self.credentials_error.as_deref()
    }
    pub fn grants_error(&self) -> Option<&str> {
        self.grants_error.as_deref()
    }
    pub fn revoke_error(&self) -> Option<&str> {
        self.revoke_error.as_deref()
    }

    /// The credential whose retry is in flight, if any.
    pub fn retrying(&self) -> Option<&str> {
        self.retrying.as_deref()
    }

    /// What the last retry reply said: a failure, or that there was nothing
    /// to retry.
    pub fn retry_message(&self) -> Option<&str> {
        self.retry_message.as_deref()
    }

    pub fn approvals(&self) -> Option<Approvals> {
        self.approvals
    }

    pub fn approvals_error(&self) -> Option<&str> {
        self.approvals_error.as_deref()
    }

    pub fn toggle(&mut self) -> bool {
        self.open = !self.open;
        if !self.open {
            self.confirming = None;
            self.revoked = None;
            self.retry_message = None;
            self.invalidate();
        }
        self.open
    }

    fn invalidate(&mut self) {
        self.credentials_request += 1;
        self.grants_request += 1;
        self.approvals_request += 1;
    }

    pub fn begin_approvals(&mut self) -> u64 {
        self.approvals_request += 1;
        self.approvals_request
    }

    pub fn approvals_listed(&mut self, ticket: u64, result: Result<Option<Approvals>, String>) {
        if ticket != self.approvals_request {
            return;
        }
        match result {
            Ok(mode) => {
                self.approvals = mode;
                self.approvals_error = None;
            }
            Err(why) => self.approvals_error = Some(why),
        }
    }

    pub fn begin_credentials(&mut self) -> u64 {
        self.credentials_loading = true;
        self.credentials_request += 1;
        self.credentials_request
    }

    pub fn begin_grants(&mut self) -> u64 {
        self.grants_loading = true;
        self.grants_request += 1;
        self.grants_request
    }

    pub fn credentials_listed(&mut self, ticket: u64, result: Result<Vec<Listed>, String>) {
        if ticket != self.credentials_request {
            return;
        }
        self.credentials_loading = false;
        match result {
            Ok(list) => {
                self.credentials = list;
                self.credentials_error = None;
                if !self.busy()
                    && let Some(Revoke::Credential(name)) = &self.confirming
                    && !self
                        .credentials
                        .iter()
                        .any(|item| item.name == *name && item.fingerprint.is_some() && !item.revoked)
                {
                    self.confirming = None;
                }
            }
            Err(why) => self.credentials_error = Some(why),
        }
    }

    pub fn grants_listed(&mut self, ticket: u64, result: Result<Vec<Grant>, String>) {
        if ticket != self.grants_request {
            return;
        }
        self.grants_loading = false;
        match result {
            Ok(list) => {
                self.grants = list;
                self.grants_error = None;
                if !self.busy()
                    && let Some(Revoke::Grant(grant)) = &self.confirming
                    && !self.grants.iter().any(|listed| listed.id == grant.id)
                {
                    self.confirming = None;
                }
            }
            Err(why) => self.grants_error = Some(why),
        }
    }

    pub fn ask_credential(&mut self, name: &str) -> bool {
        if self.busy()
            || !self.credentials.iter().any(|item| item.name == name && item.fingerprint.is_some() && !item.revoked)
        {
            return false;
        }
        self.confirm(Revoke::Credential(name.into()));
        true
    }

    pub fn ask_grant(&mut self, id: &str) -> bool {
        if self.busy() {
            return false;
        }
        let Some(grant) = self.grants.iter().find(|grant| grant.id == id).cloned() else {
            return false;
        };
        self.confirm(Revoke::Grant(grant));
        true
    }

    fn confirm(&mut self, target: Revoke) {
        self.confirming = Some(target);
        self.revoked = None;
        self.revoke_error = None;
    }

    pub fn cancel(&mut self) -> bool {
        if self.busy() {
            return self.confirming.is_some();
        }
        self.confirming.take().is_some()
    }

    pub fn begin_revoke(&mut self) -> Option<Revoke> {
        if self.busy() {
            return None;
        }
        let target = self.confirming.clone()?;
        self.in_flight = Some(target.clone());
        self.invalidate();
        Some(target)
    }

    /// A successful reply is authority to remove the grant. Credential state
    /// comes from its subsequent listing, since rotation may have happened.
    pub fn revoke_done(&mut self, result: Result<(), String>) {
        let Some(target) = self.in_flight.take() else {
            return;
        };
        match result {
            Ok(()) => {
                if let Revoke::Grant(grant) = &target {
                    self.grants.retain(|listed| listed.id != grant.id);
                }
                self.revoked = Some(target);
                self.confirming = None;
                self.revoke_error = None;
            }
            Err(why) => {
                self.confirming = self.open.then_some(target);
                self.revoke_error = Some(why);
            }
        }
    }

    /// Start a retry for a paused credential. False when one is already in
    /// flight or the listing no longer shows that name as paused.
    pub fn begin_retry(&mut self, name: &str) -> bool {
        if self.retrying.is_some() || !self.credentials.iter().any(|item| item.name == name && item.paused) {
            return false;
        }
        self.retrying = Some(name.into());
        self.retry_message = None;
        true
    }

    /// A successful retry clears the pause; the subsequent listing shows the
    /// new state. `false` means the daemon had no completed denial to clear.
    pub fn retry_done(&mut self, result: Result<bool, String>) {
        if self.retrying.take().is_none() {
            return;
        }
        self.retry_message = match result {
            Ok(true) => None,
            Ok(false) => Some("Nothing to retry".into()),
            Err(why) => Some(why),
        };
    }
}

/// Time remaining at the most recent panel refresh. Expiry is enforced by
/// the daemon and gateway, regardless of whether a window is open.
pub fn grant_scope(grant: &Grant, now_ms: u64) -> String {
    let scope = match grant.scope {
        Scope::Once => "Once",
        Scope::Timed => "5 minutes",
        Scope::Task => "Task",
        Scope::Project => "Project",
        Scope::Default => "Default",
    };
    match grant.expires_ms {
        None => match grant.scope {
            Scope::Project => "Project · no expiry".into(),
            Scope::Task => "Task · until stopped".into(),
            Scope::Once | Scope::Timed | Scope::Default => scope.into(),
        },
        Some(expiry) if expiry <= now_ms => format!("{scope} · expired"),
        Some(expiry) => {
            let minutes = (expiry - now_ms).div_ceil(60_000);
            if minutes >= 60 {
                format!("{scope} · expires in {}h {}m", minutes / 60, minutes % 60)
            } else {
                format!("{scope} · expires in {minutes}m")
            }
        }
    }
}

#[cfg(test)]
mod tests;
