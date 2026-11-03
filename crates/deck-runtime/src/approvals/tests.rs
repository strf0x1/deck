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
    Settings { hold: Duration::from_secs(2), pending: Duration::from_millis(80), once: Duration::from_millis(200) }
}

/// Tests that are not about expiry must not lose their item to a slow runner.
/// Five minutes is production; five seconds is plenty for a test, and the
/// 80 ms `fast` window stays reserved for the expiry test.
fn steady() -> Settings {
    Settings { hold: Duration::from_secs(2), pending: Duration::from_secs(5), once: Duration::from_millis(200) }
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
