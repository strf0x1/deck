//! Cancellation is sticky, not a secret cache. A host retry can clear only a
//! completed denial; it cannot duplicate a pending OS read or grant task access.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn cancellation_stops_automatic_reads_until_an_explicit_retry() {
    let calls = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(Store::scripted(Duration::from_secs(2), {
        let calls = calls.clone();
        move |_| match calls.fetch_add(1, Ordering::SeqCst) {
            0 => Err(Absent::Denied),
            attempt => Ok(format!("rotated-{attempt}")),
        }
    }));
    assert_eq!(store.read("KEY"), Err(Absent::Denied));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            std::thread::spawn(move || store.read("KEY"))
        })
        .collect();
    for reader in readers {
        assert_eq!(reader.join().unwrap(), Err(Absent::Denied));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!store.retry("OTHER").unwrap());
    assert!(store.retry("lowercase").is_err());
    let retrying: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            std::thread::spawn(move || store.retry("KEY").unwrap())
        })
        .collect();
    assert_eq!(retrying.into_iter().map(|thread| usize::from(thread.join().unwrap())).sum::<usize>(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "retry permits a read but does not start one");
    assert_eq!(store.read("KEY").unwrap(), "rotated-1");
    assert_eq!(store.read("KEY").unwrap(), "rotated-2", "successful values are never cached");
}

#[test]
fn retry_does_not_replace_an_outstanding_read_after_a_timeout() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (answer, blocked) = std::sync::mpsc::channel();
    let blocked = Mutex::new(blocked);
    let store = Store::scripted(Duration::from_millis(50), {
        let calls = calls.clone();
        move |_| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                blocked.lock().unwrap().recv().unwrap();
            }
            Ok("value".into())
        }
    });
    assert!(matches!(store.read("KEY"), Err(Absent::Unavailable(_))));
    assert!(!store.retry("KEY").unwrap());
    assert!(matches!(store.read("KEY"), Err(Absent::Unavailable(_))));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    answer.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !store.reads.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(store.read("KEY").unwrap(), "value");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn outage_and_missing_items_recover_without_an_explicit_retry() {
    for first in [Absent::Missing, Absent::Unavailable("locked or offline".into())] {
        let calls = Arc::new(AtomicUsize::new(0));
        let store = Store::scripted(Duration::from_secs(2), {
            let first = first.clone();
            let calls = calls.clone();
            move |_| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 { Err(first.clone()) } else { Ok("recovered".into()) }
            }
        });
        assert_eq!(store.read("KEY"), Err(first));
        assert_eq!(store.read("KEY").unwrap(), "recovered");
    }
}

#[test]
fn broker_lists_and_fetches_do_not_reopen_a_cancelled_read() {
    use crate::broker::{Broker, Refusal};
    let data = std::env::temp_dir().join(format!("deck-broker-denial-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let store = Store::scripted(Duration::from_secs(2), {
        let calls = calls.clone();
        move |_| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 { Err(Absent::Denied) } else { Ok("value".into()) }
        }
    });
    let broker = Broker::open(&data, store).unwrap();
    let names = vec!["KEY".into()];
    assert!(matches!(broker.fetch("t", "KEY", &[]), Err(Refusal::Refused)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let Err(error) = broker.fetch("t", "KEY", &names) else { panic!("must be unavailable") };
    assert_eq!(error.code(), "deck_credential_unavailable");
    assert!(error.reply("KEY").contains("deckd credential retry KEY"));
    assert!(broker.list_names(&names)[0].error.as_ref().unwrap().contains("retry KEY"));
    assert!(broker.revoke("KEY").is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(broker.retry("KEY").unwrap());
    assert!(matches!(broker.fetch("other", "KEY", &[]), Err(Refusal::Refused)));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "retry does not widen a task's credential set");
    assert_eq!(broker.fetch("t", "KEY", &names).unwrap().value, "value");
    std::fs::remove_dir_all(data).unwrap();
}

#[test]
fn cancellation_answered_after_a_caller_times_out_stays_paused() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (answer, blocked) = std::sync::mpsc::channel();
    let blocked = Mutex::new(blocked);
    let store = Store::scripted(Duration::from_millis(50), {
        let calls = calls.clone();
        move |_| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                blocked.lock().unwrap().recv().unwrap();
            }
            Err(Absent::Denied)
        }
    });
    assert!(matches!(store.read("KEY"), Err(Absent::Unavailable(_))));
    answer.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let reads = store.reads.lock().unwrap();
        let done = reads.get("KEY").is_some_and(|flight| flight.outcome.lock().unwrap().is_some());
        if done {
            break;
        }
        assert!(Instant::now() < deadline);
        drop(reads);
        std::thread::yield_now();
    }
    assert_eq!(store.read("KEY"), Err(Absent::Denied));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(store.retry("KEY").unwrap());
    assert_eq!(store.read("KEY"), Err(Absent::Denied));
    assert_eq!(store.read("KEY"), Err(Absent::Denied));
    assert_eq!(calls.load(Ordering::SeqCst), 2, "a second refusal also requires explicit retry");
}
