use super::*;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicU32, Ordering};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "deck-recovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn question() -> Question {
    Question::Agent {
        title: "Run cargo test?".into(),
        options: vec![
            OptionItem { id: "yes".into(), name: "Allow".into(), kind: "allow_once".into() },
            OptionItem { id: "no".into(), name: "Reject".into(), kind: "reject_once".into() },
        ],
    }
}

#[test]
fn question_first_answer_and_delivery_survive_reopening() {
    let dir = Scratch::new();
    let mut store = Store::open(&dir.0).unwrap();
    let first = store.capture("task-run-1-request-9", question()).unwrap();
    assert_eq!(std::fs::metadata(dir.0.join(FILE)).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(store.capture("task-run-1-request-9", question()).unwrap(), first);
    assert!(
        store.capture("task-run-1-request-9", Question::Agent { title: "different".into(), options: vec![] }).is_err()
    );
    assert!(Store::open(&dir.0).is_err(), "another writer cannot open the task");
    drop(store);

    let mut store = Store::open(&dir.0).unwrap();
    assert_eq!(store.items(), &[first]);
    assert!(store.answer("task-run-1-request-9", Answer::Agent { option_id: "missing".into() }).is_err());
    let answered = store.answer("task-run-1-request-9", Answer::Agent { option_id: "yes".into() }).unwrap();
    let Delivery::Answered { answer, key } = &answered.delivery else { panic!("answer was not persisted") };
    assert_eq!(answer, &Answer::Agent { option_id: "yes".into() });
    assert_eq!(key.len(), 36);
    assert_eq!(store.answer("task-run-1-request-9", Answer::Agent { option_id: "no".into() }).unwrap(), answered);
    drop(store);

    let mut store = Store::open(&dir.0).unwrap();
    assert_eq!(store.items(), std::slice::from_ref(&answered));
    let sending = store.start_delivery("task-run-1-request-9").unwrap();
    assert_eq!(sending.delivery, Delivery::Sending { answer: answer.clone(), key: key.clone() });
    drop(store);
    let mut store = Store::open(&dir.0).unwrap();
    assert_eq!(store.items(), &[sending]);
    let uncertain = store.uncertain("task-run-1-request-9").unwrap();
    assert_eq!(uncertain.delivery, Delivery::Uncertain { answer: answer.clone(), key: key.clone() });
    assert!(store.delivered("task-run-1-request-9").is_err());
    drop(store);
    assert_eq!(Store::open(&dir.0).unwrap().items(), &[uncertain]);
}

#[test]
fn cancelled_agent_question_is_a_distinct_answer() {
    let dir = Scratch::new();
    let mut store = Store::open(&dir.0).unwrap();
    store.capture("cancelled", question()).unwrap();
    let item = store.answer("cancelled", Answer::AgentCancelled).unwrap();
    assert!(matches!(item.delivery, Delivery::Answered { answer: Answer::AgentCancelled, .. }));
    drop(store);
    assert_eq!(Store::open(&dir.0).unwrap().items(), &[item]);
}

#[test]
fn boundary_answer_does_not_create_a_grant_and_delivery_is_recorded() {
    let dir = Scratch::new();
    let approval: Approval = serde_json::from_value(serde_json::json!({
        "id": "approval-1", "task": "task-1", "project": "project-1", "gateway": "web",
        "operation": "connect", "destination": "example.com:443", "created_ms": 1,
        "expires_ms": 2, "policy": 1, "state": "pending"
    }))
    .unwrap();
    let mut store = Store::open(&dir.0).unwrap();
    store.capture("boundary:approval-1", Question::Boundary { approval: Box::new(approval) }).unwrap();
    assert!(store.answer("boundary:approval-1", Answer::Agent { option_id: "yes".into() }).is_err());
    let answered = store.answer("boundary:approval-1", Answer::Boundary { decision: Decision::Once }).unwrap();
    let Delivery::Answered { answer, key } = answered.delivery else { panic!("missing answer") };
    store.start_delivery("boundary:approval-1").unwrap();
    let delivered = store.delivered("boundary:approval-1").unwrap();
    assert_eq!(delivered.delivery, Delivery::Delivered { answer, key });
    assert!(!dir.0.join("grants.json").exists());
    drop(store);
    assert_eq!(Store::open(&dir.0).unwrap().items(), &[delivered]);
}

#[test]
fn invalid_and_oversized_questions_are_not_persisted() {
    let dir = Scratch::new();
    let mut store = Store::open(&dir.0).unwrap();
    assert!(store.capture("", question()).is_err());
    let long = Question::Agent {
        title: "x".repeat(MAX_ITEM),
        options: vec![OptionItem { id: "yes".into(), name: "Allow".into(), kind: "allow_once".into() }],
    };
    assert!(store.capture("oversized", long).is_err());
    assert!(!dir.0.join(FILE).exists());
    assert!(store.items().is_empty());
}

#[test]
fn an_uncertain_answer_can_be_retried_with_its_key_or_reviewed() {
    let dir = Scratch::new();
    let mut store = Store::open(&dir.0).unwrap();
    store.capture("q1", question()).unwrap();
    let answered = store.answer("q1", Answer::Agent { option_id: "yes".into() }).unwrap();
    let Delivery::Answered { key, .. } = &answered.delivery else { panic!("not answered") };
    store.start_delivery("q1").unwrap();
    store.uncertain("q1").unwrap();
    drop(store);

    let mut store = Store::open(&dir.0).unwrap();
    let retry = store.start_delivery("q1").unwrap();
    let Delivery::Sending { key: same, .. } = &retry.delivery else { panic!("not sending") };
    assert_eq!(key, same, "a retry must reuse the persisted key");
    let reviewed = store.review("q1").unwrap();
    assert!(matches!(reviewed.delivery, Delivery::Reviewed { key: ref saved, .. } if saved == key));
    assert!(store.review("q1").is_err(), "a reviewed answer is terminal");
}
