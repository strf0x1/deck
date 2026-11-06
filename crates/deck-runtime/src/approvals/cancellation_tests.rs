//! Cancellation during a hold leaves the shared card and its Once allowance
//! available to a live operation. Flags make cancellation independent of thread order.

use crate::approvals::{Boundary, Decision, Details, Incoming, Scope, Settings, State};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

fn boundary(name: &str) -> (Arc<Boundary>, PathBuf) {
    let directory = std::env::temp_dir().join(format!("deck-cancel-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let settings = Settings {
        hold: Duration::from_secs(5),
        pending: Duration::from_secs(10),
        once: Duration::from_secs(5),
        ..Settings::default()
    };
    (Boundary::with_settings(&directory, settings).unwrap(), directory)
}

fn request() -> Incoming {
    Incoming {
        task: "t".into(),
        project: "/project".into(),
        session: None,
        gateway: "model".into(),
        operation: "chat_completions".into(),
        destination: "other".into(),
        credential: Some("OPENAI_API_KEY".into()),
        details: Details::default(),
        reason: None,
        coverage: None,
        approvals: None,
    }
}

fn held(boundary: &Boundary, count: usize) -> String {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let inner = boundary.lock();
        if let Some((id, _)) = inner.holds.iter().find(|(_, actual)| **actual == count) {
            return id.clone();
        }
        assert!(Instant::now() < deadline, "the callers did not enter their shared hold");
        drop(inner);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_cancelled_hold_leaves_the_once_decision_for_its_live_peer() {
    let (boundary, directory) = boundary("live-peer");
    let (_lease, _events) = boundary.watch("t");
    let connected = Arc::new(AtomicBool::new(true));
    let (done, cancelled) = mpsc::channel();
    let abandoned = {
        let boundary = boundary.clone();
        let connected = connected.clone();
        std::thread::spawn(move || {
            done.send(boundary.submit_while(request(), || connected.load(Ordering::Acquire))).unwrap();
        })
    };
    let live = {
        let boundary = boundary.clone();
        std::thread::spawn(move || boundary.submit(request()).unwrap())
    };
    let id = held(&boundary, 2);
    connected.store(false, Ordering::Release);
    assert!(cancelled.recv_timeout(Duration::from_secs(1)).unwrap().is_err());
    abandoned.join().unwrap();
    assert_eq!(held(&boundary, 1), id);
    boundary.decide(&id, Decision::Once).unwrap();
    assert_eq!(live.join().unwrap().state, State::Allowed(Scope::Once));
    assert!(boundary.grants("t").unwrap().is_empty());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn cancellation_before_a_decision_cannot_consume_its_once_allowance() {
    let (boundary, directory) = boundary("claim-race");
    let (lease, _events) = boundary.watch("t");
    let connected = Arc::new(AtomicBool::new(true));
    let (done, cancelled) = mpsc::channel();
    let abandoned = {
        let boundary = boundary.clone();
        let connected = connected.clone();
        std::thread::spawn(move || {
            done.send(boundary.submit_while(request(), || connected.load(Ordering::Acquire))).unwrap();
        })
    };
    let id = held(&boundary, 1);
    // Whichever thread wins the queue lock, cancellation precedes the claim.
    connected.store(false, Ordering::Release);
    boundary.decide(&id, Decision::Once).unwrap();
    assert!(cancelled.recv_timeout(Duration::from_secs(1)).unwrap().is_err());
    abandoned.join().unwrap();
    assert_eq!(boundary.submit(request()).unwrap().state, State::Allowed(Scope::Once));
    drop(lease);
    assert_eq!(boundary.submit(request()).unwrap().state, State::Pending);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_already_cancelled_caller_cannot_consume_a_late_once_decision() {
    let (boundary, directory) = boundary("late-claim");
    let item = boundary.submit(request()).unwrap();
    boundary.decide(&item.id, Decision::Once).unwrap();
    assert!(boundary.submit_while(request(), || false).is_err());
    assert_eq!(boundary.submit(request()).unwrap().state, State::Allowed(Scope::Once));
    assert_eq!(boundary.submit(request()).unwrap().state, State::Pending);
    std::fs::remove_dir_all(directory).unwrap();
}
