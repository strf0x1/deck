use super::*;
use serde_json::json;

fn task(id: &str, project: &str, kind: TaskKind, created_by: Option<&str>) -> Task {
    let mut task: Task = serde_json::from_value(json!({
        "id": id, "backend": "local", "spec": { "argv": ["a"], "cwd": "/w", "project": project },
        "state": { "state": "running" }, "pid": null,
    }))
    .unwrap();
    task.spec.kind = kind;
    task.spec.created_by = created_by.map(str::to_owned);
    task
}

fn policy() -> Policy {
    Policy::new(vec!["git:a".into()]).unwrap()
}

#[test]
fn the_coordinator_sees_its_project_set_and_never_itself() {
    let policy = policy();
    assert!(visible(&policy, "c", &task("w", "git:a", TaskKind::Task, Some("c"))));
    assert!(visible(&policy, "c", &task("u", "git:a", TaskKind::Task, None)), "a user's task is readable");
    assert!(!visible(&policy, "c", &task("x", "git:b", TaskKind::Task, Some("c"))), "outside the set");
    assert!(!visible(&policy, "c", &task("c", "git:a", TaskKind::Coordinator, None)), "itself");
}

#[test]
fn only_its_own_workers_may_be_driven_and_never_a_coordinator() {
    assert!(may_drive("c", &task("w", "git:a", TaskKind::Task, Some("c"))).is_ok());
    assert_eq!(may_drive("c", &task("u", "git:a", TaskKind::Task, None)).unwrap_err().code, Code::Scope);
    assert_eq!(may_drive("c", &task("o", "git:a", TaskKind::Task, Some("other"))).unwrap_err().code, Code::Scope);
    assert_eq!(may_drive("c", &task("k", "git:a", TaskKind::Coordinator, None)).unwrap_err().code, Code::Refused);
}

#[test]
fn a_stopped_task_is_promptable_but_a_destroyed_task_is_not() {
    let mut worker = task("w", "git:a", TaskKind::Task, Some("c"));
    assert!(promptable(&worker).is_ok());
    worker.state = State::Stopped { clean: true, reason: "stop".into() };
    assert!(promptable(&worker).is_ok());
    worker.state = State::Destroyed;
    assert_eq!(promptable(&worker).unwrap_err().code, Code::Refused);
}

#[test]
fn a_project_outside_the_set_is_scope_and_the_refusal_names_the_set() {
    assert!(project_in_scope(&policy(), "git:a").is_ok());
    let refusal = project_in_scope(&policy(), "git:0000000000000000000000000000000000000000").unwrap_err();
    assert_eq!(refusal.code, Code::Scope);
    assert_eq!(
        refusal.message,
        "project git:0000…0000 is not in this coordinator's project set; its projects are git:a"
    );
}

#[test]
fn create_defaults_to_the_only_project_and_asks_when_there_are_several() {
    assert_eq!(create_project(&policy(), None).unwrap(), "git:a");
    assert_eq!(create_project(&policy(), Some("git:a")).unwrap(), "git:a");
    assert_eq!(create_project(&policy(), Some("textkit")).unwrap_err().code, Code::Scope);
    let two = Policy::new(vec!["git:a".into(), "git:b".into()]).unwrap();
    let refusal = create_project(&two, None).unwrap_err();
    assert_eq!(
        (refusal.code, refusal.message.as_str()),
        (Code::BadRequest, "name a project; its projects are git:a, git:b")
    );
    assert_eq!(create_project(&two, Some("git:b")).unwrap(), "git:b");
}

#[test]
fn create_checks_the_command_the_budget_then_the_caps() {
    let argv = vec!["agent".to_string()];
    let mut policy = policy();
    policy.worker_budget = Budget { requests: None, tokens: Some(1000) };
    let now = 10 * HOUR_MS;
    let ok = admit_create(&policy, Some(&argv), None, &[], None, now).unwrap();
    assert_eq!(ok, policy.worker_budget, "the policy's budget by default");
    let lower = Requested { requests: Some(5), tokens: Some(10) };
    assert_eq!(
        admit_create(&policy, Some(&argv), Some(lower), &[], None, now).unwrap(),
        Budget { requests: Some(5), tokens: Some(10) }
    );
    let higher = Requested { requests: None, tokens: Some(1001) };
    assert_eq!(admit_create(&policy, Some(&argv), Some(higher), &[], None, now).unwrap_err().code, Code::Refused);
    assert_eq!(admit_create(&policy, None, None, &[], None, now).unwrap_err().code, Code::Refused, "no command");
    assert_eq!(admit_create(&policy, Some(&[]), None, &[], None, now).unwrap_err().code, Code::Refused);

    // The fifth live worker at cap 4; stopped workers do not count.
    let live: Vec<_> = (0..4).map(|_| (State::Running, 0)).collect();
    let refusal = admit_create(&policy, Some(&argv), None, &live, None, now).unwrap_err();
    assert_eq!((refusal.code, refusal.cap), (Code::Cap, Some((Cap::Concurrent, 4))));
    assert_eq!(refusal.message, "this coordinator already has 4 live workers (cap 4)");
    let mut ended = live.clone();
    ended[0].0 = State::Stopped { clean: true, reason: "stop".into() };
    assert!(admit_create(&policy, Some(&argv), None, &ended, None, now).is_ok());

    // The rate counts every worker created in the rolling hour.
    policy.max_concurrent = 100;
    policy.creates_per_hour = 2;
    let recent = vec![(State::Destroyed, now - 1000), (State::Destroyed, now - HOUR_MS + 1)];
    let refusal = admit_create(&policy, Some(&argv), None, &recent, None, now).unwrap_err();
    assert_eq!((refusal.code, refusal.cap), (Code::Cap, Some((Cap::Rate, 2))));
    let old = vec![(State::Destroyed, now - 1000), (State::Destroyed, now - HOUR_MS)];
    assert!(admit_create(&policy, Some(&argv), None, &old, None, now).is_ok(), "an hour ago has rolled off");
}

#[test]
fn a_local_checkout_takes_one_worker_at_a_time() {
    let argv = vec!["agent".to_string()];
    let policy = policy();
    assert!(admit_create(&policy, Some(&argv), None, &[], Some(0), 0).is_ok(), "an empty checkout");
    let refusal = admit_create(&policy, Some(&argv), None, &[], Some(1), 0).unwrap_err();
    assert_eq!((refusal.code, refusal.cap), (Code::Cap, None), "a wait-and-retry refusal no Limit raises");
    assert!(refusal.message.contains("one runs at a time"), "{}", refusal.message);
    assert!(admit_create(&policy, Some(&argv), None, &[], None, 0).is_ok(), "a VM worker has its own copy");
}

#[test]
fn operations_are_closed_and_bounded_again() {
    let parse = |value: serde_json::Value| serde_json::from_value::<Operation>(value);
    assert!(parse(json!({ "operation": "destroy", "id": "t" })).is_err());
    assert!(parse(json!({ "operation": "stop", "id": "t", "force": true })).is_err());
    let wait = parse(json!({ "operation": "wait", "id": "t", "wait_ms": 300001 })).unwrap();
    assert_eq!(wait.validate().unwrap_err().code, Code::BadRequest);
    let create = parse(json!({ "operation": "create", "project": "p", "brief": "b", "budget": {} })).unwrap();
    assert!(parse(json!({ "operation": "create", "brief": "b" })).is_ok(), "project is optional since 1.5.0");
    assert_eq!(create.validate().unwrap_err().code, Code::BadRequest);
    let prompt = parse(json!({ "operation": "prompt", "id": "t", "text": "go" })).unwrap();
    assert_eq!((prompt.name(), prompt.target()), ("prompt", Some("t")));
}

#[test]
fn an_outcome_is_a_reply_or_a_named_refusal() {
    let refused = Outcome::Refused(Refusal::new(Code::Busy, "the task's turn is working"));
    assert_eq!(
        serde_json::to_value(&refused).unwrap(),
        json!({ "refused": { "code": "deck_control_busy", "message": "the task's turn is working" } })
    );
    assert_eq!(serde_json::to_value(Outcome::Reply(json!({ "id": "w" }))).unwrap(), json!({ "reply": { "id": "w" } }));
}
