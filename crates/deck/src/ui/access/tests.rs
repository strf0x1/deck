//! Confirmation, stale replies and failure behavior of the access panel.

use super::*;

fn grant(scope: Scope) -> Grant {
    Grant {
        credential: None,
        id: "g".into(),
        task: Some("t".into()),
        project: None,
        gateway: "web".into(),
        operation: "connect".into(),
        destination: "example.com:443".into(),
        scope,
        expires_ms: Some(3_600_000),
        reviewed: None,
    }
}

fn panel() -> Access {
    let mut access = Access::new();
    access.toggle();
    let ticket = access.begin_grants();
    access.grants_listed(ticket, Ok(vec![grant(Scope::Task)]));
    access
}

#[test]
fn revoking_requires_confirmation_and_waits_for_a_successful_reply() {
    let mut access = panel();
    assert!(access.begin_revoke().is_none());
    assert!(!access.ask_grant("unknown"));
    assert!(access.ask_grant("g"));
    assert!(access.cancel());
    assert!(access.begin_revoke().is_none());
    access.ask_grant("g");
    assert_eq!(access.begin_revoke(), Some(Revoke::Grant(grant(Scope::Task))));
    assert!(access.begin_revoke().is_none());
    assert_eq!(access.grants().len(), 1, "no optimistic removal");
    access.revoke_done(Ok(()));
    assert!(access.grants().is_empty());
    assert_eq!(access.revoked(), Some(&Revoke::Grant(grant(Scope::Task))));
}

#[test]
fn failure_preserves_the_grant_and_confirmation_for_retry() {
    let mut access = panel();
    access.ask_grant("g");
    access.begin_revoke();
    access.revoke_done(Err("daemon disconnected".into()));
    assert_eq!(access.grants(), &[grant(Scope::Task)]);
    assert!(access.confirming().is_some());
    assert_eq!(access.revoke_error(), Some("daemon disconnected"));
    assert!(!access.busy());
}

#[test]
fn an_old_listing_cannot_restore_a_revoked_grant() {
    let mut access = panel();
    let old = access.begin_grants();
    access.ask_grant("g");
    access.begin_revoke();
    access.revoke_done(Ok(()));
    access.grants_listed(old, Ok(vec![grant(Scope::Task)]));
    assert!(access.grants().is_empty());
}

#[test]
fn closing_during_a_revoke_preserves_its_in_flight_target() {
    let mut access = panel();
    access.ask_grant("g");
    access.begin_revoke();
    access.toggle();
    assert!(access.confirming().is_none());
    access.revoke_done(Ok(()));
    assert_eq!(access.revoked(), Some(&Revoke::Grant(grant(Scope::Task))));
    assert!(!access.busy());
}

#[test]
fn credentials_still_require_a_live_value_and_keep_the_provider_warning() {
    let mut access = panel();
    let ticket = access.begin_credentials();
    access.credentials_listed(
        ticket,
        Ok(vec![Listed {
            name: "KEY".into(),
            fingerprint: Some("sha256:a".into()),
            revoked: false,
            error: None,
            paused: false,
        }]),
    );
    assert!(!access.ask_credential("missing"));
    assert!(access.ask_credential("KEY"));
    assert!(access.confirming().unwrap().explanation().contains(NOT_CANCELLED));
    access.begin_revoke();
    access.revoke_done(Err("no daemon".into()));
    assert!(!access.credentials()[0].revoked);
    assert_eq!(access.confirming(), Some(&Revoke::Credential("KEY".into())));
}

#[test]
fn a_retry_runs_once_and_reports_nothing_to_retry() {
    let mut access = panel();
    let ticket = access.begin_credentials();
    access.credentials_listed(
        ticket,
        Ok(vec![
            Listed {
                name: "PAUSED".into(),
                fingerprint: None,
                revoked: false,
                error: Some("paused".into()),
                paused: true,
            },
            Listed {
                name: "ACTIVE".into(),
                fingerprint: Some("sha256:a".into()),
                revoked: false,
                error: None,
                paused: false,
            },
        ]),
    );
    assert!(!access.begin_retry("ACTIVE"), "an active credential is not retryable");
    assert!(!access.begin_retry("UNKNOWN"));
    assert!(access.begin_retry("PAUSED"));
    assert!(!access.begin_retry("PAUSED"), "one retry at a time");
    assert_eq!(access.retrying(), Some("PAUSED"));
    access.retry_done(Ok(false));
    assert_eq!(access.retrying(), None);
    assert_eq!(access.retry_message(), Some("Nothing to retry"));
    assert!(access.begin_retry("PAUSED"), "a finished retry can start another");
    access.retry_done(Err("daemon disconnected".into()));
    assert_eq!(access.retry_message(), Some("daemon disconnected"));
}

#[test]
fn a_stale_listing_cannot_start_a_retry() {
    let mut access = panel();
    let ticket = access.begin_credentials();
    access.credentials_listed(
        ticket,
        Ok(vec![Listed {
            name: "PAUSED".into(),
            fingerprint: None,
            revoked: false,
            error: Some("paused".into()),
            paused: true,
        }]),
    );
    let stale = access.begin_credentials();
    access.credentials_listed(stale, Ok(vec![]));
    assert!(!access.begin_retry("PAUSED"), "the name is no longer listed as paused");
}

#[test]
fn a_stale_approval_mode_reply_cannot_replace_a_newer_one() {
    let mut access = panel();
    let old = access.begin_approvals();
    let fresh = access.begin_approvals();
    access.approvals_listed(fresh, Ok(Some(Approvals::Ask)));
    access.approvals_listed(old, Ok(Some(Approvals::OwnBranch)));
    assert_eq!(access.approvals(), Some(Approvals::Ask));
    access.approvals_listed(fresh, Err("daemon disconnected".into()));
    assert_eq!(access.approvals(), Some(Approvals::Ask), "an error keeps the last known mode");
    assert_eq!(access.approvals_error(), Some("daemon disconnected"));
}

#[test]
fn scopes_show_expiry_and_project_revocation_explains_the_wider_effect() {
    assert_eq!(grant_scope(&grant(Scope::Task), 0), "Task · expires in 1h 0m");
    assert_eq!(grant_scope(&grant(Scope::Task), 3_600_000), "Task · expired");
    let target = Revoke::Grant(grant(Scope::Project));
    assert!(target.explanation().contains("every task in this project"));
    assert!(target.explanation().contains("Completed operations cannot be undone"));
}
