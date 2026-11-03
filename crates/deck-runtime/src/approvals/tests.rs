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
    assert!(boundary.pending("t").unwrap().is_empty());
    assert_eq!(boundary.decide(&first.id, Decision::Task).unwrap().state, State::Expired);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_allow_once_is_consumed_exactly_once() {
    let dir = scratch("once");
    let boundary = Boundary::with_settings(&dir, fast()).unwrap();
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    assert_eq!(boundary.decide(&item.id, Decision::Once).unwrap().state, State::Allowed(Scope::Once));
    assert_eq!(boundary.submit(incoming("t", "/w", "m")).unwrap().state, State::Allowed(Scope::Once));
    assert_eq!(boundary.submit(incoming("t", "/w", "m")).unwrap().state, State::Pending);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_task_grant_applies_to_matching_requests_only() {
    let dir = scratch("task");
    let boundary = Boundary::with_settings(&dir, fast()).unwrap();
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
    let boundary = Boundary::with_settings(&dir, fast()).unwrap();
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    boundary.decide(&item.id, Decision::Project).unwrap();
    drop(boundary);
    let reopened = Boundary::with_settings(&dir, fast()).unwrap();
    assert_eq!(reopened.submit(incoming("other", "/w", "m")).unwrap().state, State::Allowed(Scope::Project));
    assert_eq!(reopened.submit(incoming("other", "/elsewhere", "m")).unwrap().state, State::Pending);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_lease_holds_the_request_until_the_decision() {
    let dir = scratch("hold");
    let boundary = Boundary::with_settings(&dir, fast()).unwrap();
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
    let boundary = Boundary::with_settings(&dir, fast()).unwrap();
    let started = std::time::Instant::now();
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    assert_eq!(item.state, State::Pending);
    assert!(started.elapsed() < Duration::from_millis(500));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_first_decision_wins() {
    let dir = scratch("first");
    let boundary = Boundary::with_settings(&dir, fast()).unwrap();
    let item = boundary.submit(incoming("t", "/w", "m")).unwrap();
    assert_eq!(boundary.decide(&item.id, Decision::Deny).unwrap().state, State::Denied);
    assert_eq!(boundary.decide(&item.id, Decision::Task).unwrap().state, State::Denied);
    assert!(boundary.grants("t").unwrap().is_empty());
    fs::remove_dir_all(dir).unwrap();
}
