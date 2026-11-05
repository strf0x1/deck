//! Queue behaviour: deduplication, expiry, decision scopes, leases and the
//! bounded hold, with shortened timings.

use super::*;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("deck-approvals-{name}-{}-{}", std::process::id(), now_ms()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn incoming(task: &str, project: &str, destination: &str) -> Incoming {
    Incoming {
        task: task.into(),
        project: project.into(),
        session: None,
        gateway: "model".into(),
        operation: "chat_completions".into(),
        destination: destination.into(),
        credential: Some("OPENAI_API_KEY".into()),
        details: Details { method: Some("POST".into()), path: Some("/v1/chat/completions".into()), size: Some(0) },
        reason: None,
    }
}

fn fast() -> Settings {
    Settings {
        hold: Duration::from_secs(2),
        pending: Duration::from_millis(80),
        once: Duration::from_millis(200),
        task: super::TASK,
    }
}

/// Tests that are not about expiry must not lose their item to a slow runner.
/// Five minutes is production; five seconds is plenty for a test, and the
/// 80 ms `fast` window stays reserved for the expiry test.
fn steady() -> Settings {
    Settings {
        hold: Duration::from_secs(2),
        pending: Duration::from_secs(5),
        once: Duration::from_millis(200),
        task: super::TASK,
    }
}

fn held_card(boundary: &Boundary, count: usize) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let inner = boundary.lock();
        if let Some((id, _)) = inner
            .holds
            .iter()
            .find(|(id, held)| **held == count && inner.items.get(*id).is_some_and(|item| item.state.pending()))
        {
            return id.clone();
        }
        assert!(std::time::Instant::now() < deadline, "requests did not share a held card");
        drop(inner);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn pending_requests_dedupe_and_expire() {
    let dir = scratch("pending");
    let boundary = Boundary::with_settings(&dir, fast()).unwrap();
    let first = boundary.submit(incoming("t", "/w", "m")).unwrap();
    let second = boundary.submit(incoming("t", "/w", "m")).unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(first.state, State::Pending);
    assert_eq!(boundary.pending("t").unwrap().len(), 1);
    std::thread::sleep(Duration::from_millis(120));
    // Decide before pruning: a slow runner may already be past the short
    // post-expiry retention window that `pending()` would enforce.
    assert_eq!(boundary.decide(&first.id, Decision::Task).unwrap().state, State::Expired);
    assert!(boundary.pending("t").unwrap().is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_allow_once_is_consumed_exactly_once() {
    let dir = scratch("once");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    assert_eq!(boundary.decide(&item.id, Decision::Once).unwrap().state, State::Allowed(Scope::Once));
    assert_eq!(boundary.submit(incoming("t", "/w", "m")).unwrap().state, State::Allowed(Scope::Once));
    assert_eq!(boundary.submit(incoming("t", "/w", "m")).unwrap().state, State::Pending);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_deduplicated_once_decision_admits_only_one_held_request() {
    let dir = scratch("concurrent-once");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let (lease, _rx) = boundary.watch("t");
    let requests: Vec<_> = (0..4)
        .map(|_| {
            let boundary = boundary.clone();
            std::thread::spawn(move || boundary.submit(incoming("t", "/w", "m")).unwrap())
        })
        .collect();
    let id = held_card(&boundary, requests.len());
    boundary.decide(&id, Decision::Once).unwrap();
    let outcomes: Vec<_> = requests.into_iter().map(|request| request.join().unwrap()).collect();
    assert_eq!(outcomes.iter().filter(|item| item.state == State::Allowed(Scope::Once)).count(), 1);
    let pending: Vec<_> = outcomes.iter().filter(|item| item.state == State::Pending).collect();
    assert_eq!(pending.len(), 3);
    assert_ne!(pending[0].id, id, "unadmitted requests need a new decision");
    assert!(pending.iter().all(|item| item.id == pending[0].id));
    assert_eq!(boundary.status("t", &id).unwrap().unwrap().state, State::Allowed(Scope::Once));
    assert!(boundary.grants("t").unwrap().is_empty());
    drop(lease);
    // Repeating a decision or reading its status never replenishes it.
    boundary.decide(&id, Decision::Once).unwrap();
    assert_eq!(boundary.submit(incoming("t", "/w", "m")).unwrap().state, State::Pending);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn remaining_held_requests_can_use_their_new_card() {
    let dir = scratch("once-requeue");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let (_lease, _rx) = boundary.watch("t");
    let requests: Vec<_> = (0..3)
        .map(|_| {
            let boundary = boundary.clone();
            std::thread::spawn(move || boundary.submit(incoming("t", "/w", "m")).unwrap())
        })
        .collect();
    let first = held_card(&boundary, 3);
    boundary.decide(&first, Decision::Once).unwrap();
    let next = held_card(&boundary, 2);
    assert_ne!(first, next);
    boundary.decide(&next, Decision::Task).unwrap();
    let outcomes: Vec<_> = requests.into_iter().map(|request| request.join().unwrap()).collect();
    assert_eq!(outcomes.iter().filter(|item| item.state == State::Allowed(Scope::Once)).count(), 1);
    assert_eq!(outcomes.iter().filter(|item| item.state == State::Allowed(Scope::Task)).count(), 2);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn once_after_a_hold_timeout_still_admits_only_one_retry() {
    let dir = scratch("once-hold-timeout");
    let settings = Settings { hold: Duration::from_millis(10), ..steady() };
    let boundary = Boundary::with_settings(&dir, settings).unwrap();
    let (_lease, _rx) = boundary.watch("t");
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    assert_eq!(item.state, State::Pending);
    boundary.decide(&item.id, Decision::Once).unwrap();
    assert_eq!(boundary.submit(incoming("t", "/w", "m")).unwrap().state, State::Allowed(Scope::Once));
    assert_eq!(boundary.submit(incoming("t", "/w", "m")).unwrap().state, State::Pending);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_unclaimed_once_expires_without_being_replenished() {
    let dir = scratch("once-expiry");
    let settings = Settings { once: Duration::ZERO, ..steady() };
    let boundary = Boundary::with_settings(&dir, settings).unwrap();
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    boundary.decide(&item.id, Decision::Once).unwrap();
    assert_eq!(boundary.submit(incoming("t", "/w", "m")).unwrap().state, State::Pending);
    boundary.decide(&item.id, Decision::Once).unwrap();
    assert_eq!(boundary.submit(incoming("t", "/w", "m")).unwrap().state, State::Pending);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn clearing_a_task_wakes_its_held_request_without_granting() {
    let dir = scratch("clear-held");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let (_lease, _rx) = boundary.watch("t");
    let asked = boundary.clone();
    let request = std::thread::spawn(move || asked.submit(incoming("t", "/w", "m")));
    held_card(&boundary, 1);
    boundary.clear_task("t").unwrap();
    assert!(request.join().unwrap().is_err());
    assert!(boundary.pending("t").unwrap().is_empty());
    assert!(boundary.grants("t").unwrap().is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn revoking_a_grant_does_not_persist_other_once_allowances() {
    let dir = scratch("once-revoke");
    let settings = Settings { once: Duration::from_secs(5), ..steady() };
    let boundary = Boundary::with_settings(&dir, settings).unwrap();
    for destination in ["first", "second"] {
        let item = boundary.submit(incoming("t", "/w", destination)).unwrap();
        boundary.decide(&item.id, Decision::Once).unwrap();
    }
    let first = boundary.grants("t").unwrap().into_iter().find(|grant| grant.destination == "first").unwrap();
    boundary.revoke("t", &first.id).unwrap();
    assert_eq!(boundary.grants("t").unwrap().len(), 1);
    assert!(!task_grants(&dir, "t").exists(), "once grants never reach the task's policy file");
    drop(boundary);
    let restarted = Boundary::with_settings(&dir, steady()).unwrap();
    assert_eq!(restarted.submit(incoming("t", "/w", "second")).unwrap().state, State::Pending);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_task_grant_applies_to_matching_requests_only() {
    let dir = scratch("task");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    boundary.decide(&item.id, Decision::Task).unwrap();
    assert_eq!(boundary.grants("t").unwrap().len(), 1);
    assert_eq!(boundary.submit(incoming("t", "/w", "m")).unwrap().state, State::Allowed(Scope::Task));
    assert_eq!(boundary.submit(incoming("t", "/w", "other")).unwrap().state, State::Pending);
    boundary.clear_task("t").unwrap();
    assert!(boundary.grants("t").unwrap().is_empty());
    assert_eq!(boundary.submit(incoming("t", "/w", "m")).unwrap().state, State::Pending);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn project_rules_survive_a_restart_and_stay_with_their_project() {
    let dir = scratch("project");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    boundary.decide(&item.id, Decision::Project).unwrap();
    drop(boundary);
    let reopened = Boundary::with_settings(&dir, steady()).unwrap();
    assert_eq!(reopened.submit(incoming("other", "/w", "m")).unwrap().state, State::Allowed(Scope::Project));
    assert_eq!(reopened.submit(incoming("other", "/elsewhere", "m")).unwrap().state, State::Pending);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_failed_revocation_audit_reports_failure_but_keeps_the_policy_narrowed() {
    let dir = scratch("revoke-audit-failure");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let request = incoming("t", "/w", "m");
    let item = boundary.submit(request.clone()).unwrap();
    boundary.decide(&item.id, Decision::Task).unwrap();
    let grant = boundary.grants("t").unwrap().remove(0);
    fs::create_dir_all(dir.join("approvals/audit.jsonl")).unwrap();
    assert!(boundary.revoke("t", &grant.id).is_err(), "no successful reply without the audit");
    assert_eq!(boundary.submit(request.clone()).unwrap().state, State::Pending);
    drop(boundary);
    let reopened = Boundary::with_settings(&dir, steady()).unwrap();
    assert_eq!(reopened.submit(request).unwrap().state, State::Pending);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_lease_holds_the_request_until_the_decision() {
    let dir = scratch("hold");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let (lease, _rx) = boundary.watch("t");
    let asked = boundary.clone();
    let started = std::time::Instant::now();
    let thread = std::thread::spawn(move || asked.submit(incoming("t", "/w", "m")).unwrap());
    let id = loop {
        if let Some(item) = boundary.pending("t").unwrap().first() {
            break item.id.clone();
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(started.elapsed() < Duration::from_secs(1), "the request should still be waiting");
    boundary.decide(&id, Decision::Task).unwrap();
    let outcome = thread.join().unwrap();
    assert_eq!(outcome.state, State::Allowed(Scope::Task));
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(lease);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn without_a_lease_the_request_is_answered_pending_at_once() {
    let dir = scratch("nolease");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let started = std::time::Instant::now();
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    assert_eq!(item.state, State::Pending);
    assert!(started.elapsed() < Duration::from_millis(500));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_status_wait_is_visible_and_a_decision_wakes_it() {
    let dir = scratch("waiting");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    assert_eq!(item.waiters, 0);
    let asked = boundary.clone();
    let id = item.id.clone();
    let waiter = std::thread::spawn(move || asked.wait("t", &id, Duration::from_secs(2)).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        if boundary.pending("t").unwrap()[0].waiters == 1 {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the wait was never visible");
        std::thread::sleep(Duration::from_millis(5));
    }
    let decided = boundary.decide(&item.id, Decision::Deny).unwrap();
    assert_eq!(decided.waiters, 1, "the decision knows a wait was open");
    let outcome = waiter.join().unwrap();
    assert_eq!(outcome.state, State::Denied);
    assert_eq!(outcome.waiters, 0);
    assert!(boundary.status("t", &item.id).unwrap().unwrap().state == State::Denied);
    assert!(boundary.status("other", &item.id).unwrap().is_none());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_wait_on_a_decided_item_returns_at_once_and_never_reopens_it() {
    let dir = scratch("wait-decided");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    boundary.decide(&item.id, Decision::Task).unwrap();
    let started = std::time::Instant::now();
    let outcome = boundary.wait("t", &item.id, Duration::from_secs(2)).unwrap();
    assert!(matches!(outcome.state, State::Allowed(Scope::Task)));
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(boundary.wait("t", "missing", Duration::from_secs(1)).is_err());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_wait_times_out_on_its_own_deadline_and_leaves_the_item_pending() {
    let dir = scratch("wait-timeout");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    let started = std::time::Instant::now();
    let outcome = boundary.wait("t", &item.id, Duration::from_millis(50)).unwrap();
    assert_eq!(outcome.state, State::Pending);
    assert_eq!(outcome.waiters, 0);
    assert!(started.elapsed() >= Duration::from_millis(50));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn withdrawal_closes_a_pending_item_and_grants_nothing() {
    let dir = scratch("withdraw");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    let withdrawn = boundary.withdraw("t", &item.id).unwrap();
    assert_eq!(withdrawn.state, State::Withdrawn);
    assert!(boundary.pending("t").unwrap().is_empty());
    assert!(boundary.grants("t").unwrap().is_empty());
    assert_eq!(boundary.withdraw("t", &item.id).unwrap().state, State::Withdrawn);
    assert!(boundary.withdraw("other", &item.id).is_err());
    assert_eq!(boundary.submit(incoming("t", "/w", "m")).unwrap().state, State::Pending);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_new_watch_sees_items_that_were_already_pending() {
    let dir = scratch("watch-snapshot");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    let (_lease, mut rx) = boundary.watch("t");
    let snapshot = rx.try_recv().unwrap();
    assert_eq!(snapshot.id, item.id);
    assert_eq!(snapshot.state, State::Pending);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_first_decision_wins() {
    let dir = scratch("first");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    assert_eq!(boundary.decide(&item.id, Decision::Deny).unwrap().state, State::Denied);
    assert_eq!(boundary.decide(&item.id, Decision::Task).unwrap().state, State::Denied);
    assert!(boundary.grants("t").unwrap().is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_profile_decision_is_project_rules_the_gateway_can_read() {
    let dir = scratch("profile");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let destinations = vec!["deb.debian.org:443".to_string(), "security.debian.org:443".to_string()];
    assert_eq!(boundary.profile(Some("/w"), &destinations, false).unwrap(), 2);
    // A second approval replaces rather than duplicates.
    assert_eq!(boundary.profile(Some("/w"), &destinations, false).unwrap(), 0);
    let file = fs::read_to_string(dir.join("approvals/projects.json")).unwrap();
    let rules: Vec<Grant> = serde_json::from_str::<serde_json::Value>(&file)
        .map(|value| serde_json::from_value(value["rules"].clone()).unwrap())
        .unwrap();
    assert_eq!(rules.len(), 2, "one rule per destination, not per approval: {rules:?}");
    for rule in &rules {
        assert_eq!((rule.gateway.as_str(), rule.operation.as_str()), ("web", "connect"));
        assert_eq!(rule.project.as_deref(), Some("/w"));
        assert_eq!(rule.scope, Scope::Project);
    }
    // Another project's approval is a different set of rules.
    boundary.profile(Some("/other"), &destinations[..1], false).unwrap();
    assert_eq!(boundary.profile(Some("/w"), &destinations, true).unwrap(), 2);
    let left = Boundary::open(&dir).unwrap();
    assert!(left.profile(Some("/w"), &destinations, true).unwrap() == 0, "revoking twice changes nothing");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_profile_decision_needs_a_project_and_a_name() {
    let dir = scratch("profile-names");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    assert!(boundary.profile(None, &[], false).is_err(), "a rule belongs to a project");
    assert!(boundary.profile(Some(""), &[], false).is_err());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_allow_for_task_has_an_expiry_while_a_project_rule_does_not() {
    let dir = scratch("task-expiry");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    let allowed = boundary.decide(&item.id, Decision::Task).unwrap();
    assert_eq!(allowed.state, State::Allowed(Scope::Task));
    let grant = boundary.grants("t").unwrap().into_iter().next().unwrap();
    assert!(grant.expires_ms.is_some(), "a task grant is not unbounded (ADR 0004 §5)");
    let project = boundary.submit(incoming("t", "/w", "other")).unwrap();
    boundary.decide(&project.id, Decision::Project).unwrap();
    let file = fs::read_to_string(dir.join("approvals/projects.json")).unwrap();
    assert!(!file.contains("expires_ms"), "a project rule has no expiry: {file}");
    fs::remove_dir_all(dir).unwrap();
}

fn github(task: &str, operation: &str, destination: &str) -> Incoming {
    Incoming {
        gateway: "github".into(),
        operation: operation.into(),
        credential: None,
        details: Details::default(),
        ..incoming(task, "/w", destination)
    }
}

#[test]
fn a_github_profile_covers_every_branch_and_pull_request_of_its_repository() {
    let dir = scratch("github-profile");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let operations = vec!["publish".to_string(), "update_pr".to_string()];
    assert_eq!(boundary.github_profile(Some("/w"), "Strf0x1/Deck", &operations, false).unwrap(), 2);
    let allowed = |task, operation, destination| boundary.submit(github(task, operation, destination)).unwrap().state;
    assert_eq!(allowed("t-1", "publish", "strf0x1/deck#deck/t-1"), State::Allowed(Scope::Project));
    assert_eq!(allowed("t-2", "update_pr", "strf0x1/deck#pr12"), State::Allowed(Scope::Project));
    // Another operation, another repository, or a repository that merely
    // shares the prefix are not covered.
    assert_eq!(allowed("t-1", "create_pr", "strf0x1/deck#deck/t-1"), State::Pending);
    assert_eq!(allowed("t-1", "publish", "strf0x1/deck-other#deck/t-1"), State::Pending);
    assert_eq!(allowed("t-1", "publish", "other/deck#deck/t-1"), State::Pending);
    for invalid in ["deck", "https://github.com/o/r", "o/r#main", "o/../r"] {
        assert!(boundary.github_profile(Some("/w"), invalid, &operations, false).is_err(), "{invalid}");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn always_allow_on_a_github_card_trusts_the_repository() {
    let dir = scratch("github-always");
    let boundary = Boundary::with_settings(&dir, steady()).unwrap();
    let item = boundary.submit(github("t-1", "publish", "o/r#deck/t-1")).unwrap();
    boundary.decide(&item.id, Decision::Project).unwrap();
    assert_eq!(
        boundary.submit(github("t-2", "publish", "o/r#deck/t-2")).unwrap().state,
        State::Allowed(Scope::Project)
    );
    // A task grant stays as narrow as the card it answered.
    let item = boundary.submit(github("t-1", "create_pr", "o/r#deck/t-1")).unwrap();
    boundary.decide(&item.id, Decision::Task).unwrap();
    assert_eq!(boundary.submit(github("t-1", "create_pr", "o/r#other")).unwrap().state, State::Pending);
    fs::remove_dir_all(dir).unwrap();
}
