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

fn run(dir: &Path) -> RunIdentity {
    let identity = RunIdentity { task: dir.file_name().unwrap().to_str().unwrap().into(), run: "launch-uuid-1".into() };
    std::fs::write(dir.join("run.json"), serde_json::to_vec(&identity).unwrap()).unwrap();
    identity
}

#[test]
fn staged_stop_requires_current_durable_run_through_promotion() {
    let dir = Scratch::new();
    let current = run(&dir.0);
    let wrong = RunIdentity { task: current.task.clone(), run: "different".into() };
    std::fs::remove_file(dir.0.join("run.json")).unwrap();
    let mut store = Store::open(&dir.0).unwrap();
    assert!(store.prepare_stop("missing", current.clone(), vec![]).is_err());
    std::fs::write(dir.0.join("run.json"), serde_json::to_vec(&wrong).unwrap()).unwrap();
    assert!(store.prepare_stop("wrong", current.clone(), vec![]).is_err());
    std::fs::write(dir.0.join("run.json"), serde_json::to_vec(&current).unwrap()).unwrap();
    store.prepare_stop("matched", current.clone(), vec![]).unwrap();
    std::fs::write(dir.0.join("run.json"), serde_json::to_vec(&wrong).unwrap()).unwrap();
    assert!(store.begin_stop("matched", &current).is_err());
    std::fs::write(dir.0.join("run.json"), serde_json::to_vec(&current).unwrap()).unwrap();
    store.begin_stop("matched", &current).unwrap();
    std::fs::write(dir.0.join("run.json"), serde_json::to_vec(&wrong).unwrap()).unwrap();
    assert!(store.promote_stop("matched", &ConfirmedExit { run: current }).is_err());
    assert!(store.items().is_empty());
}

fn captured(id: &str) -> Captured {
    Captured { id: id.into(), question: question() }
}

#[test]
fn staged_stop_stays_hidden_until_matching_confirmed_exit_and_commits_atomically() {
    let dir = Scratch::new();
    let run = run(&dir.0);
    let other = RunIdentity { task: run.task.clone(), run: "another-launch".into() };
    let questions = vec![captured("q1"), captured("q2")];
    let mut store = Store::open(&dir.0).unwrap();
    let prepared = store.prepare_stop("txn-1", run.clone(), questions.clone()).unwrap();
    assert_eq!(prepared.phase, StopPhase::Prepared);
    assert_eq!(store.prepare_stop("txn-1", run.clone(), questions.clone()).unwrap(), prepared);
    assert!(store.prepare_stop("another", run.clone(), questions.clone()).is_err());
    assert!(store.prepare_stop("txn-1", run.clone(), vec![captured("different")]).is_err());
    assert!(store.items().is_empty(), "a prepared snapshot is not visible");
    assert!(store.answer("q1", Answer::AgentCancelled).is_err());
    assert!(store.promote_stop("txn-1", &ConfirmedExit { run: run.clone() }).is_err());
    drop(store);

    let mut store = Store::open(&dir.0).unwrap();
    assert_eq!(store.staged_stop().unwrap(), &prepared);
    assert!(store.begin_stop("txn-1", &other).is_err());
    let stopping = store.begin_stop("txn-1", &run).unwrap();
    assert_eq!(stopping.phase, StopPhase::Stopping);
    assert_eq!(store.begin_stop("txn-1", &run).unwrap(), stopping);
    assert!(store.items().is_empty(), "recording stop intent does not publish a question");
    assert!(store.abort_prepared_stop("txn-1", &run).is_err());
    assert!(store.promote_stop("txn-1", &ConfirmedExit { run: other }).is_err());
    drop(store);

    let mut store = Store::open(&dir.0).unwrap();
    assert_eq!(store.staged_stop().unwrap(), &stopping);
    assert!(store.items().is_empty());
    let committed = store.promote_stop("txn-1", &ConfirmedExit { run: run.clone() }).unwrap();
    assert_eq!(committed.len(), 2);
    assert_eq!(store.items(), committed.as_slice());
    assert_eq!(store.staged_stop().unwrap().phase, StopPhase::Committed);
    let first = store.answer("q1", Answer::Agent { option_id: "yes".into() }).unwrap();
    drop(store);

    let mut store = Store::open(&dir.0).unwrap();
    let again = store.promote_stop("txn-1", &ConfirmedExit { run }).unwrap();
    assert_eq!(again[0], first, "retry preserves the already recorded answer and key");
    assert_eq!(store.items().len(), 2, "retry cannot duplicate a question");
}

#[test]
fn abort_requires_the_right_evidence_and_never_publishes_the_snapshot() {
    let dir = Scratch::new();
    let run = run(&dir.0);
    let other = RunIdentity { task: run.task.clone(), run: "other".into() };
    let mut store = Store::open(&dir.0).unwrap();
    store.prepare_stop("txn-prepared", run.clone(), vec![captured("old")]).unwrap();
    let abort = store.abort_prepared_stop("txn-prepared", &run).unwrap();
    assert_eq!(abort.phase, StopPhase::Aborted);
    assert_eq!(store.abort_prepared_stop("txn-prepared", &run).unwrap(), abort);
    assert!(store.items().is_empty());
    assert!(store.capture("old", question()).is_err(), "aborted IDs cannot become unrelated captures");
    store.prepare_stop("txn-stopping", run.clone(), vec![captured("new")]).unwrap();
    store.begin_stop("txn-stopping", &run).unwrap();
    assert!(store.abort_stopping("txn-stopping", &ConfirmedAbort { run: other }).is_err());
    assert!(store.abort_prepared_stop("txn-stopping", &run).is_err());
    let stopped = store.abort_stopping("txn-stopping", &ConfirmedAbort { run: run.clone() }).unwrap();
    assert_eq!(stopped.phase, StopPhase::Aborted);
    assert_eq!(store.abort_stopping("txn-stopping", &ConfirmedAbort { run: run.clone() }).unwrap(), stopped);
    assert!(store.promote_stop("txn-stopping", &ConfirmedExit { run }).is_err());
    drop(store);
    let store = Store::open(&dir.0).unwrap();
    assert!(store.items().is_empty());
    assert_eq!(store.staged_stop().unwrap().phase, StopPhase::Aborted);
}

#[test]
fn stage_rejects_foreign_duplicate_and_expired_questions() {
    let dir = Scratch::new();
    let run = run(&dir.0);
    let mut store = Store::open(&dir.0).unwrap();
    assert!(store.prepare_stop("txn", run.clone(), vec![captured("q"), captured("q")]).is_err());
    let foreign = RunIdentity { task: "other".into(), run: "launch".into() };
    assert!(store.prepare_stop("txn", foreign, vec![]).is_err());
    let approval: Approval = serde_json::from_value(serde_json::json!({
        "id": "approval-1", "task": run.task, "project": "project-1", "gateway": "web",
        "operation": "connect", "destination": "example.com:443", "created_ms": 1,
        "expires_ms": 2, "policy": 1, "state": "pending"
    }))
    .unwrap();
    assert!(
        store
            .prepare_stop(
                "txn",
                run,
                vec![Captured { id: "boundary".into(), question: Question::Boundary { approval: Box::new(approval) } }]
            )
            .is_err()
    );
    assert!(store.staged_stop().is_none());
    assert!(store.items().is_empty());
}

#[test]
fn expiring_approval_cannot_pass_from_prepared_to_stop_intent() {
    let dir = Scratch::new();
    let run = run(&dir.0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let approval: Approval = serde_json::from_value(serde_json::json!({
        "id": "approval-1", "task": run.task, "project": "project-1", "gateway": "web",
        "operation": "connect", "destination": "example.com:443", "created_ms": now,
        "expires_ms": now + 300, "policy": 1, "state": "pending"
    }))
    .unwrap();
    let mut store = Store::open(&dir.0).unwrap();
    store
        .prepare_stop(
            "txn",
            run.clone(),
            vec![Captured { id: "boundary".into(), question: Question::Boundary { approval: Box::new(approval) } }],
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(350));
    assert!(store.begin_stop("txn", &run).is_err());
    assert_eq!(store.staged_stop().unwrap().phase, StopPhase::Prepared);
    assert!(store.items().is_empty());
    store.abort_prepared_stop("txn", &run).unwrap();
}

#[test]
fn stage_keeps_existing_answers_and_rejects_reusing_their_ids() {
    let dir = Scratch::new();
    let run = run(&dir.0);
    let mut store = Store::open(&dir.0).unwrap();
    store.capture("existing", question()).unwrap();
    let old = store.answer("existing", Answer::AgentCancelled).unwrap();
    assert!(store.prepare_stop("bad", run.clone(), vec![captured("existing")]).is_err());
    store.prepare_stop("txn", run.clone(), vec![captured("new")]).unwrap();
    assert!(store.start_delivery("existing").is_err(), "answers cannot change while the stop is staged");
    assert_eq!(store.items(), std::slice::from_ref(&old));
    store.begin_stop("txn", &run).unwrap();
    store.promote_stop("txn", &ConfirmedExit { run }).unwrap();
    assert_eq!(store.items()[0], old);
    assert_eq!(store.items().len(), 2);
    drop(store);
    let store = Store::open(&dir.0).unwrap();
    assert_eq!(store.items()[0], old);
    assert_eq!(store.items().len(), 2);
}

#[test]
fn an_inconsistent_committed_snapshot_is_refused_on_restart() {
    let dir = Scratch::new();
    let run = run(&dir.0);
    let mut store = Store::open(&dir.0).unwrap();
    store.prepare_stop("txn", run.clone(), vec![captured("q")]).unwrap();
    store.begin_stop("txn", &run).unwrap();
    drop(store);
    let file = dir.0.join(FILE);
    let mut snapshot: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    snapshot["stop"]["phase"] = serde_json::json!("committed");
    std::fs::write(file, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    assert!(Store::open(&dir.0).is_err(), "a committed question cannot be missing");
}

#[test]
fn an_empty_stop_still_requires_matching_exit_and_keeps_its_receipt() {
    let dir = Scratch::new();
    let run = run(&dir.0);
    let mut store = Store::open(&dir.0).unwrap();
    store.prepare_stop("empty", run.clone(), vec![]).unwrap();
    store.begin_stop("empty", &run).unwrap();
    assert!(store.promote_stop("empty", &ConfirmedExit { run: run.clone() }).unwrap().is_empty());
    drop(store);
    let mut store = Store::open(&dir.0).unwrap();
    assert_eq!(store.staged_stop().unwrap().phase, StopPhase::Committed);
    assert!(store.promote_stop("empty", &ConfirmedExit { run }).unwrap().is_empty());
}
