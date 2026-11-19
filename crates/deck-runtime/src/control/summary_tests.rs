use super::*;
use crate::lifecycle::Publication;
use crate::{Turn, lifecycle::TaskKind};

fn task(state: State, turn: Option<(u64, Phase)>) -> Task {
    let mut task: Task = serde_json::from_value(json!({
        "id": "w", "backend": "local", "spec": { "argv": ["a"], "cwd": "/w", "project": "git:a" },
        "state": { "state": "running" }, "pid": null,
    }))
    .unwrap();
    task.state = state;
    task.turn = turn.map(|(number, state)| Turn { number, state, stop_reason: None });
    task.spec.created_by = Some("c".into());
    task.spec.kind = TaskKind::Task;
    task
}

#[test]
fn a_listing_stops_at_the_byte_limit_and_keeps_the_newest_tasks_and_projects() {
    let projects = vec![json!({ "id": "git:abc", "name": "App" })];
    // Escapes expand inside JSON, so count the serialized response.
    let summary = |id: &str| json!({ "id": id, "last_message": "\\".repeat(350_000) });
    let tasks = [summary("newest"), summary("second"), json!({ "id": "oldest" })];
    let reply = listing(projects.clone(), tasks.into_iter(), false).unwrap();
    assert!(reply.to_string().len() <= 1_048_576);
    assert_eq!(reply["projects"], json!(projects));
    assert_eq!(reply["truncated"], true);
    assert_eq!(reply["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(reply["tasks"][0]["id"], "newest");
}

fn validator() -> jsonschema::Validator {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let lock = std::fs::read_to_string(root.join("CONTRACT.lock")).unwrap();
    let line = lock.lines().find(|line| line.starts_with("vendored")).unwrap();
    let dir = root.join(line.split_once('=').unwrap().1.trim().trim_matches('"'));
    let read = |name: &str| -> Value {
        serde_json::from_str(&std::fs::read_to_string(dir.join("schema").join(name)).unwrap()).unwrap()
    };
    let schema = read("control-task-summary.schema.json");
    let integration = read("control-integration.schema.json");
    let id = integration["$id"].as_str().unwrap().to_owned();
    let registry = jsonschema::Registry::new().add(&id, integration).unwrap().prepare().unwrap();
    jsonschema::options().with_registry(&registry).build(&schema).unwrap()
}

#[test]
fn a_summary_is_the_contracts_shape() {
    let schema = validator();
    let mut worker = task(State::Running, Some((2, Phase::Idle)));
    worker.turn.as_mut().unwrap().stop_reason = Some("end_turn".into());
    worker.title = Some("Fix the flaky login test".into());
    worker.publication = Some(Publication::Branch { branch: "deck/t-w".into(), commit: "a".repeat(40) });
    let facts = Facts {
        pending_approvals: 0,
        pull_request: Some(json!({ "number": 4, "state": "open", "draft": true, "merged": false,
            "html_url": "https://github.com/o/r/pull/4", "checks": [] })),
        message: Some(LastMessage { text: "Done.".into(), truncated: true }),
    };
    let value = summary(&worker, &facts);
    schema.validate(&value).unwrap();
    assert_eq!(value["turn"], json!({ "number": 2, "state": "idle", "stopReason": "end_turn" }));
    assert_eq!(value["attention"], "finished");
    assert_eq!(value["created_by"], "c");
    assert_eq!(value["last_message"], "Done.");
    assert_eq!(value["last_message_truncated"], true);
    assert_eq!(value["pull_request"]["html_url"], "https://github.com/o/r/pull/4");
    assert!(value["pull_request"].get("checks").is_none());

    let fresh = summary(&task(State::Created, None), &Facts::default());
    schema.validate(&fresh).unwrap();
    assert_eq!(
        (fresh["turn"].clone(), fresh["title"].clone(), fresh["last_message"].clone()),
        (Value::Null, Value::Null, Value::Null)
    );
    let failed = task(State::Failed { failure: Failure::StartFailed("no agent".into()) }, None);
    let failed = summary(&failed, &Facts::default());
    schema.validate(&failed).unwrap();
    assert_eq!((failed["state"].as_str(), failed["attention"].as_str()), (Some("failed"), Some("failed")));
}

#[test]
fn attention_follows_the_windows_order() {
    let running = task(State::Running, Some((1, Phase::Working)));
    assert_eq!(attention(&running, 0), None);
    assert_eq!(attention(&running, 1), Some("approval"));
    assert_eq!(attention(&task(State::Running, Some((1, Phase::Waiting))), 0), Some("question"));
    let mut seen = task(State::Running, Some((1, Phase::Idle)));
    assert_eq!(attention(&seen, 0), Some("finished"));
    seen.seen = 1;
    assert_eq!(attention(&seen, 0), None, "the user saw that turn");
}

#[test]
fn a_wait_ends_on_a_terminal_state_a_later_idle_turn_or_new_attention() {
    let at = |task: &Task, pending| Snapshot::of(task, pending);
    let created = at(&task(State::Created, None), 0);
    assert!(!woke(&created, &created, 0), "a task that has not started waits");
    let working = at(&task(State::Running, Some((1, Phase::Working))), 0);
    assert!(!woke(&created, &working, 0));
    assert!(woke(&created, &at(&task(State::Running, Some((1, Phase::Idle))), 0), 0), "the brief's turn ended");
    assert!(!woke(&working, &at(&task(State::Running, Some((1, Phase::Idle))), 0), 1), "not after turn 1");
    assert!(woke(&working, &at(&task(State::Running, Some((1, Phase::Waiting))), 0), 1), "a question appeared");
    let asking = at(&task(State::Running, Some((1, Phase::Waiting))), 0);
    assert!(!woke(&asking, &asking, 1), "a question already open does not wake it");
    assert!(woke(&working, &at(&task(State::Running, Some((1, Phase::Working))), 1), 1), "a new approval");
    let failed = task(State::Failed { failure: Failure::StartFailed("x".into()) }, None);
    assert!(woke(&created, &at(&failed, 0), 0), "a failed start wakes it");
    assert!(woke(&working, &at(&task(State::Destroyed, None), 0), 5));
}
