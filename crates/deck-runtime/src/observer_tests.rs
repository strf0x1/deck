use super::*;

fn line(value: Value) -> Vec<u8> {
    format!("{value}\n").into_bytes()
}

fn parsed(line: &str) -> Value {
    serde_json::from_str(line).unwrap()
}

fn setup(session: Option<&str>) -> Setup {
    Setup { cwd: "/w".into(), session: session.map(str::to_owned) }
}

fn init_reply(id: Value, live: bool) -> Vec<u8> {
    let meta = if live { json!({ CONSTRUCT_META: { "liveAttach": true } }) } else { json!({}) };
    line(
        json!({ "jsonrpc": "2.0", "id": id, "result": { "agentCapabilities": { "loadSession": true, "_meta": meta } } }),
    )
}

fn turn(number: u64, state: &str) -> Vec<u8> {
    line(json!({ "jsonrpc": "2.0", "method": TURN_METHOD,
        "params": { "sessionId": "s1", "turn": number, "state": state } }))
}

/// A window initializes and loads `s1`, which reports turn 3 idle.
fn window_setup(o: &mut Observer) {
    o.client(&line(json!({ "jsonrpc": "2.0", "id": "deck-1", "method": "initialize", "params": {} })));
    assert!(o.agent(&init_reply(json!("deck-1"), true)).forward);
    o.client(&line(
        json!({ "jsonrpc": "2.0", "id": "deck-2", "method": "session/load", "params": { "sessionId": "s1" } }),
    ));
    let step = o.agent(&line(json!({ "jsonrpc": "2.0", "id": "deck-2", "result": {
        "_meta": { CONSTRUCT_META: { "turn": { "sessionId": "s1", "turn": 3, "state": "idle" } } } } })));
    assert!(step.forward);
    assert_eq!(step.events.last(), Some(&Event::Permissions));
}

#[test]
fn a_window_session_is_used_without_a_second_initialize() {
    let mut o = Observer::new("t");
    window_setup(&mut o);
    let step = o.prompt("go", setup(Some("s1"))).unwrap();
    assert_eq!(step.write.len(), 1);
    let prompt = parsed(&step.write[0]);
    assert_eq!(prompt["method"], "session/prompt");
    assert_eq!(prompt["params"]["sessionId"], "s1");
    assert!(prompt["id"].as_str().unwrap().starts_with("holder-t-"));
    // The turn's start is what reports the prompt, not the response.
    let started = o.agent(&turn(4, "working"));
    assert!(started.forward, "turn notifications reach the window too");
    assert_eq!(
        started.events,
        vec![Event::Turn(Turn { number: 4, state: Phase::Working, stop_reason: None }), Event::Prompted(4)]
    );
    let reply = o.agent(&line(json!({ "jsonrpc": "2.0", "id": prompt["id"], "result": { "stopReason": "end_turn" } })));
    assert!(!reply.forward, "the holder's own response never reaches the window");
}

#[test]
fn a_keyed_prompt_negotiates_capability_and_sends_the_same_key() {
    let mut o = Observer::new("t");
    let init = parsed(&o.prompt_with_key("answer", setup(Some("s1")), Some("saved-key")).unwrap().write[0]);
    let load = parsed(
        &o.agent(&line(json!({ "jsonrpc": "2.0", "id": init["id"], "result": {
        "agentCapabilities": { "_meta": { CONSTRUCT_META: { "liveAttach": true, "idempotentPrompts": true } } }
    } })))
        .write[0],
    );
    let step = o.agent(&line(json!({ "jsonrpc": "2.0", "id": load["id"], "result": {} })));
    let prompt = parsed(&step.write[0]);
    assert_eq!(prompt["method"], "session/prompt");
    assert_eq!(prompt["params"]["_meta"][CONSTRUCT_META][IDEMPOTENCY_KEY], "saved-key");
    let response =
        o.agent(&line(json!({ "jsonrpc": "2.0", "id": prompt["id"], "result": { "stopReason": "end_turn" } })));
    assert_eq!(response.events, [Event::Settled], "a cached reply must finish without a new turn");
    assert!(o.prompt_with_key("next", setup(None), Some("next-key")).is_ok());
}

#[test]
fn a_keyed_prompt_is_not_sent_without_the_capability() {
    for meta in [
        json!({}),
        json!({ LEGACY_CONSTRUCT_META: { IDEMPOTENT_PROMPTS: true } }),
        json!({ CONSTRUCT_META: { IDEMPOTENT_PROMPTS: "true" } }),
    ] {
        let mut o = Observer::new("t");
        let init = parsed(&o.prompt_with_key("answer", setup(Some("s1")), Some("saved-key")).unwrap().write[0]);
        let load = parsed(
            &o.agent(&line(json!({ "jsonrpc": "2.0", "id": init["id"], "result": {
            "agentCapabilities": { "_meta": meta }
        } })))
            .write[0],
        );
        let step = o.agent(&line(json!({ "jsonrpc": "2.0", "id": load["id"], "result": {} })));
        assert!(step.write.is_empty());
        assert!(
            step.events.iter().any(
                |event| matches!(event, Event::Failed(message) if message.contains("does not support idempotent"))
            )
        );
        assert!(o.prompt("unkeyed remains available", setup(None)).is_ok());
    }
}

#[test]
fn a_keyed_non_live_prompt_waits_for_its_response() {
    let mut o = Observer::new("t");
    let init = parsed(&o.prompt_with_key("answer", setup(None), Some("k")).unwrap().write[0]);
    let new = parsed(
        &o.agent(&line(json!({ "jsonrpc": "2.0", "id": init["id"], "result": {
        "agentCapabilities": { "_meta": { CONSTRUCT_META: { IDEMPOTENT_PROMPTS: true } } }
    } })))
        .write[0],
    );
    let step = o.agent(&line(json!({ "jsonrpc": "2.0", "id": new["id"], "result": { "sessionId": "s1" } })));
    assert!(!step.events.iter().any(|event| matches!(event, Event::Prompted(_))));
    let prompt = parsed(&step.write[0]);
    let done = o.agent(&line(json!({ "jsonrpc": "2.0", "id": prompt["id"], "result": { "stopReason": "end_turn" } })));
    assert!(done.events.contains(&Event::Settled));
}

#[test]
fn with_no_window_the_holder_initializes_and_creates_a_session() {
    let mut o = Observer::new("t");
    let step = o.prompt("brief", setup(None)).unwrap();
    let init = parsed(&step.write[0]);
    assert_eq!(init["method"], "initialize");
    let step = o.agent(&init_reply(init["id"].clone(), true));
    assert!(!step.forward);
    let new = parsed(&step.write[0]);
    assert_eq!(new["method"], "session/new");
    assert_eq!(new["params"]["cwd"], "/w");
    let step = o.agent(&line(json!({ "jsonrpc": "2.0", "id": new["id"], "result": { "sessionId": "s1" } })));
    assert!(!step.forward);
    assert_eq!(step.events, vec![Event::Session("s1".into()), Event::Permissions]);
    let prompt = parsed(&step.write[0]);
    assert_eq!(prompt["method"], "session/prompt");
    assert_eq!(prompt["params"]["prompt"][0]["text"], "brief");
    assert_eq!(o.agent(&turn(1, "working")).events.last(), Some(&Event::Prompted(1)));
}

#[test]
fn a_recorded_session_is_loaded_and_its_replay_still_reaches_a_window() {
    let mut o = Observer::new("t");
    let init = parsed(&o.prompt("next", setup(Some("s1"))).unwrap().write[0]);
    let load = parsed(&o.agent(&init_reply(init["id"].clone(), true)).write[0]);
    assert_eq!(load["method"], "session/load");
    assert_eq!(load["params"]["sessionId"], "s1");
    let replay = line(json!({ "jsonrpc": "2.0", "method": "session/update",
        "params": { "sessionId": "s1", "update": { "sessionUpdate": "agent_message_chunk" } } }));
    assert!(o.agent(&replay).forward);
    let step = o.agent(&line(json!({ "jsonrpc": "2.0", "id": load["id"], "result": {} })));
    assert_eq!(step.events, vec![Event::Session("s1".into()), Event::Permissions]);
    assert_eq!(parsed(&step.write[0])["method"], "session/prompt");
}

#[test]
fn a_running_turn_or_open_question_is_busy() {
    let mut o = Observer::new("t");
    window_setup(&mut o);
    o.agent(&turn(4, "working"));
    assert_eq!(o.prompt("x", setup(None)).unwrap_err(), Refused::Busy);
    o.agent(&turn(4, "waiting"));
    let ask = line(json!({ "jsonrpc": "2.0", "id": 900, "method": "session/request_permission",
        "params": { "sessionId": "s1" } }));
    let step = o.agent(&ask);
    assert!(step.forward && step.write.is_empty(), "the holder never answers a question");
    assert_eq!(o.prompt("x", setup(None)).unwrap_err(), Refused::Busy);
    o.client(&line(json!({ "jsonrpc": "2.0", "id": 900, "result": { "outcome": { "outcome": "selected" } } })));
    o.agent(&turn(4, "idle"));
    assert!(o.prompt("x", setup(None)).is_ok());
    assert_eq!(o.prompt("again", setup(None)).unwrap_err(), Refused::Busy, "one prompt starting at a time");
}

#[test]
fn a_second_admitted_prompt_invalidates_a_waiting_snapshot() {
    let mut o = Observer::new("t");
    window_setup(&mut o);
    o.client(&line(json!({ "jsonrpc": "2.0", "id": "first", "method": "session/prompt", "params": {} })));
    o.agent(&turn(4, "waiting"));
    o.agent(&line(json!({ "jsonrpc": "2.0", "id": 900, "method": "session/request_permission", "params": {
        "sessionId": "s1", "toolCall": { "title": "Read" }, "options": []
    } })));
    assert!(o.fence_snapshot().is_some());
    o.client(&line(json!({ "jsonrpc": "2.0", "id": "second", "method": "session/prompt", "params": {} })));
    assert!(o.fence_snapshot().is_none(), "a later prompt may not belong to the observed question");
}

#[test]
fn pending_permissions_keep_the_offered_choices_until_each_answer() {
    let mut o = Observer::new("t");
    window_setup(&mut o);
    o.agent(&turn(4, "waiting"));
    let ask = |id: Value, title: &str| {
        line(json!({ "jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
            "sessionId": "s1", "toolCall": { "title": title },
            "options": [{ "optionId": "yes", "name": "Allow once", "kind": "allow_once" },
                        { "optionId": "no", "name": "Reject", "kind": "reject_once" }]
        } }))
    };
    o.agent(&ask(json!(1), "Read a file"));
    o.agent(&ask(json!("two"), "Run a command"));
    let questions = o.pending_questions().unwrap();
    assert_eq!(questions.len(), 2);
    assert_eq!(questions[0].request, json!(1));
    assert_eq!(questions[0].title, "Read a file");
    assert_eq!(
        questions[0].options[0],
        PermissionChoice { id: "yes".into(), name: "Allow once".into(), kind: "allow_once".into() }
    );
    assert_eq!(questions[1].request, json!("two"));
    o.client(&line(json!({ "jsonrpc": "2.0", "id": 1, "result": {} })));
    assert_eq!(o.pending_questions().unwrap().iter().map(|q| q.title.as_str()).collect::<Vec<_>>(), ["Run a command"]);
    o.client(&line(json!({ "jsonrpc": "2.0", "id": "unknown", "result": {} })));
    assert_eq!(o.pending_questions().unwrap().len(), 1);
    assert_eq!(o.prompt("x", setup(None)).unwrap_err(), Refused::Busy);
    o.client(&line(json!({ "jsonrpc": "2.0", "id": "two", "result": {} })));
    assert!(o.pending_questions().unwrap().is_empty());
}

#[test]
fn reattachment_replaces_retired_question_ids() {
    let mut o = Observer::new("t");
    window_setup(&mut o);
    let ask = |id| {
        line(json!({ "jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
        "sessionId": "s1", "toolCall": { "title": "Read" }, "options": []
    } }))
    };
    o.agent(&ask(json!(1)));
    o.client(&line(json!({ "jsonrpc": "2.0", "id": "deck-9", "method": "initialize", "params": {} })));
    assert!(o.pending_questions().unwrap().is_empty());
    window_setup(&mut o);
    o.agent(&ask(json!(2)));
    o.client(&line(json!({ "jsonrpc": "2.0", "id": 1, "result": {} })));
    assert_eq!(o.pending_questions().unwrap()[0].request, json!(2));
}

#[test]
fn questions_too_large_to_preserve_remain_open() {
    let mut o = Observer::new("t");
    window_setup(&mut o);
    o.agent(&line(json!({ "jsonrpc": "2.0", "id": 1, "method": "session/request_permission", "params": {
        "sessionId": "s1", "toolCall": { "title": "x".repeat(MAX_TITLE_BYTES + 1) }, "options": []
    } })));
    assert!(o.pending_questions().is_none());
    assert_eq!(o.prompt("x", setup(None)).unwrap_err(), Refused::Busy);
    o.client(&line(json!({ "jsonrpc": "2.0", "id": 1, "result": {} })));
    assert!(o.pending_questions().unwrap().is_empty());
}

#[test]
fn too_many_questions_cannot_produce_a_partial_snapshot() {
    let mut o = Observer::new("t");
    window_setup(&mut o);
    for id in 0..=MAX_QUESTIONS {
        o.agent(&line(json!({ "jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
            "sessionId": "s1", "toolCall": { "title": "Read" }, "options": []
        } })));
    }
    assert!(o.pending_questions().is_none());
    o.client(&line(json!({ "jsonrpc": "2.0", "id": 0, "result": {} })));
    assert!(o.pending_questions().is_none(), "the overflow question was not retained in full");
    o.client(&line(json!({ "jsonrpc": "2.0", "id": MAX_QUESTIONS, "result": {} })));
    assert_eq!(o.pending_questions().unwrap().len(), MAX_QUESTIONS - 1);
}

#[test]
fn a_window_initialize_supersedes_the_session_but_not_the_reading() {
    let mut o = Observer::new("t");
    window_setup(&mut o);
    o.client(&line(json!({ "jsonrpc": "2.0", "id": "deck-9", "method": "initialize", "params": {} })));
    o.agent(&init_reply(json!("deck-9"), true));
    // Not set up on the new attachment yet: a prompt now loads, never re-initializes.
    let step = o.prompt("x", setup(Some("s1"))).unwrap();
    assert_eq!(parsed(&step.write[0])["method"], "session/load");
}

#[test]
fn an_agent_without_turn_notifications_ends_a_turn_with_its_response() {
    let mut o = Observer::new("t");
    let init = parsed(&o.prompt("go", setup(None)).unwrap().write[0]);
    let new = parsed(&o.agent(&init_reply(init["id"].clone(), false)).write[0]);
    let step = o.agent(&line(json!({ "jsonrpc": "2.0", "id": new["id"], "result": { "sessionId": "s1" } })));
    assert_eq!(
        step.events,
        vec![
            Event::Session("s1".into()),
            Event::Turn(Turn { number: 1, state: Phase::Working, stop_reason: None }),
            Event::Prompted(1),
            Event::Permissions,
        ]
    );
    let prompt = parsed(&step.write[0]);
    let done =
        o.agent(&line(json!({ "jsonrpc": "2.0", "id": prompt["id"], "result": { "stopReason": "max_tokens" } })));
    assert_eq!(
        done.events,
        vec![Event::Turn(Turn { number: 1, state: Phase::Idle, stop_reason: Some("max_tokens".into()) })]
    );
}

#[test]
fn a_window_prompt_without_turn_notifications_is_tracked_too() {
    let mut o = Observer::new("t");
    o.client(&line(json!({ "jsonrpc": "2.0", "id": "deck-1", "method": "initialize", "params": {} })));
    o.agent(&init_reply(json!("deck-1"), false));
    o.client(&line(json!({ "jsonrpc": "2.0", "id": "deck-2", "method": "session/new", "params": {} })));
    o.agent(&line(json!({ "jsonrpc": "2.0", "id": "deck-2", "result": { "sessionId": "s1" } })));
    o.client(&line(json!({ "jsonrpc": "2.0", "id": "deck-3", "method": "session/prompt", "params": {} })));
    assert_eq!(o.turn().map(|t| t.state), Some(Phase::Working));
    let reply = o.agent(&line(json!({ "jsonrpc": "2.0", "id": "deck-3", "result": { "stopReason": "end_turn" } })));
    assert!(reply.forward, "the window's own response reaches it");
    assert_eq!(o.turn().map(|t| t.state), Some(Phase::Idle));
}

#[test]
fn a_refused_prompt_fails_and_frees_the_holder() {
    let mut o = Observer::new("t");
    window_setup(&mut o);
    let prompt = parsed(&o.prompt("x", setup(None)).unwrap().write[0]);
    let step =
        o.agent(&line(json!({ "jsonrpc": "2.0", "id": prompt["id"], "error": { "code": -32602, "message": "busy" } })));
    assert!(!step.forward);
    assert!(matches!(step.events.as_slice(), [Event::Failed(why)] if why.contains("busy")));
    assert!(o.prompt("again", setup(None)).is_ok());
}

#[test]
fn another_sessions_turn_and_stray_output_are_forwarded_and_ignored() {
    let mut o = Observer::new("t");
    window_setup(&mut o);
    let foreign = line(json!({ "jsonrpc": "2.0", "method": TURN_METHOD,
        "params": { "sessionId": "other", "turn": 9, "state": "working" } }));
    let step = o.agent(&foreign);
    assert!(step.forward && step.events.is_empty());
    assert!(o.agent(b"not json\n").forward);
}

fn permission(o: &mut Observer, id: Value, session: &str, title: &str, options: Value) -> Step {
    o.agent(&line(json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission","params":{
        "sessionId":session,"toolCall":{"title":title,"kind":"execute","toolCallId":"private-tool","rawInput":{"secret":"private-plan"}},
        "options":options}})))
}

fn choices() -> Value {
    json!([{"optionId":"yes","kind":"allow_once","name":"private allow label"},
        {"optionId":"no","kind":"reject_always"}])
}

fn answer(o: &mut Observer, id: Value, outcome: Value) -> Vec<Event> {
    o.client(&line(json!({"jsonrpc":"2.0","id":id,"result":{"outcome":outcome}})))
}

#[test]
fn simultaneous_questions_publish_subject_free_observed_verdicts_within_one_turn() {
    let mut o = Observer::new("run");
    window_setup(&mut o);
    o.agent(&turn(4, "waiting"));
    for id in 1..=3 {
        permission(&mut o, json!(id), "s1", &format!("private-subject-{id}"), choices());
    }
    let before = o.permissions();
    let events = answer(&mut o, json!(2), json!({"outcome":"selected","optionId":"yes"}));
    assert_eq!(events, vec![Event::Permissions]);
    assert_eq!(o.turn().unwrap().state, Phase::Waiting);
    assert_eq!(o.permissions().pending_count, 2);
    assert_ne!(o.permissions().cursor, before.cursor);
    answer(&mut o, json!(1), json!({"outcome":"selected","optionId":"no"}));
    answer(&mut o, json!(3), json!({"outcome":"cancelled"}));
    let view = serde_json::to_value(o.permissions()).unwrap();
    assert_eq!(
        view["resolved"].as_array().unwrap().iter().map(|v| v["verdict"].as_str().unwrap()).collect::<Vec<_>>(),
        ["allowed", "denied", "cancelled"]
    );
    let text = view.to_string();
    for forbidden in
        ["private-subject", "private-plan", "private-tool", "private allow label", "optionId", "subject", "kind"]
    {
        assert!(!text.contains(forbidden), "{text}");
    }
    assert!(answer(&mut o, json!(2), json!({"outcome":"selected","optionId":"no"})).is_empty());
}

#[test]
fn unknown_ambiguous_and_error_answers_never_imply_permission() {
    let mut o = Observer::new("run");
    window_setup(&mut o);
    o.agent(&turn(4, "waiting"));
    for (id, options) in [
        (1, json!([{"optionId":"x","kind":"allow_custom"}])),
        (2, json!([{"optionId":"x","kind":"allow_once"},{"optionId":"x","kind":"reject_once"}])),
        (3, choices()),
        (4, choices()),
    ] {
        permission(&mut o, json!(id), "s1", "Run it", options);
        if id == 4 {
            o.client(&line(json!({"jsonrpc":"2.0","id":id,"error":{"code":-1,"message":"error"}})));
        } else {
            answer(&mut o, json!(id), json!({"outcome":"selected","optionId":"x"}));
        }
    }
    assert!(o.permissions().resolved.iter().all(|r| r.verdict == crate::permission::Verdict::Unknown));
    assert_eq!(o.permissions().resolved.len(), 4);
}

#[test]
fn replay_preserves_new_questions_and_reused_wire_ids_cannot_consume_them() {
    let mut o = Observer::new("run");
    window_setup(&mut o);
    o.agent(&turn(4, "waiting"));
    permission(&mut o, json!(1), "s1", "Old title", choices());
    o.client(&line(json!({"jsonrpc":"2.0","id":"attach","method":"initialize","params":{}})));
    assert_eq!(o.permissions().resolved[0].source, crate::permission::Source::Retired);
    o.agent(&init_reply(json!("attach"), true));
    o.client(&line(json!({"jsonrpc":"2.0","id":"load","method":"session/load","params":{"sessionId":"s1"}})));
    permission(&mut o, json!(1), "s1", "Reused identity", choices());
    permission(&mut o, json!(2), "s1", "Fresh identity", choices());
    o.agent(&line(
        json!({"jsonrpc":"2.0","id":"load","result":{"_meta":{CONSTRUCT_META:{"turn":{"turn":4,"state":"waiting"}}}}}),
    ));
    assert!(o.pending_questions().is_none(), "replayed questions cannot reuse the old recovery snapshot");
    answer(&mut o, json!(1), json!({"outcome":"selected","optionId":"yes"}));
    assert_eq!(o.permissions().pending_count, 2, "ambiguous old bytes cannot consume replay");
    answer(&mut o, json!(2), json!({"outcome":"selected","optionId":"yes"}));
    assert_eq!(o.permissions().pending_count, 1);
    assert_eq!(o.permissions().resolved.last().unwrap().verdict, crate::permission::Verdict::Allowed);
    o.agent(&turn(3, "idle"));
    assert_eq!(o.permissions().pending_count, 1, "old turn cannot retire current question");
    o.agent(&turn(4, "working"));
    assert_eq!(o.permissions().pending_count, 0);
    assert_eq!(o.permissions().resolved.last().unwrap().source, crate::permission::Source::Retired);
}

#[test]
fn foreign_sessions_failed_loads_and_superseded_loads_do_not_replace_current_observations() {
    let mut o = Observer::new("run");
    window_setup(&mut o);
    o.agent(&turn(4, "waiting"));
    permission(&mut o, json!(1), "s1", "Current", choices());
    permission(&mut o, json!(2), "foreign", "Foreign", choices());
    assert_eq!(o.permissions().pending_count, 1);
    let before = o.permissions();
    o.client(&line(json!({"jsonrpc":"2.0","id":"load2","method":"session/load","params":{"sessionId":"s2"}})));
    permission(&mut o, json!(3), "s2", "Staged", choices());
    assert_eq!(o.permissions(), before, "target session stays private until load succeeds");
    o.agent(&line(json!({"jsonrpc":"2.0","id":"load2","error":{"code":-1,"message":"no session"}})));
    assert_eq!(o.permissions(), before);
    for (id, session) in [("load3", "s3"), ("load4", "s4")] {
        o.client(&line(json!({"jsonrpc":"2.0","id":id,"method":"session/load","params":{"sessionId":session}})));
    }
    o.agent(&line(json!({"jsonrpc":"2.0","id":"load3","result":{}})));
    assert_eq!(o.session(), Some("s1"));
    permission(&mut o, json!(4), "s4", "Selected", choices());
    o.agent(&line(
        json!({"jsonrpc":"2.0","id":"load4","result":{"_meta":{CONSTRUCT_META:{"turn":{"turn":1,"state":"waiting"}}}}}),
    ));
    assert_eq!(o.session(), Some("s4"));
    assert_eq!(o.permissions().pending[0].subject, "Selected");
    assert!(o.permissions().resolved.is_empty());
    assert_eq!(o.turn().unwrap().number, 1);
}

#[test]
fn preview_and_result_retention_are_bounded_and_unicode_safe() {
    let mut o = Observer::new("run");
    window_setup(&mut o);
    o.agent(&turn(4, "waiting"));
    for id in 0..20 {
        permission(&mut o, json!(id), "s1", &format!("{}\n", "é".repeat(200)), choices());
    }
    let view = o.permissions();
    assert_eq!((view.pending_count, view.pending.len()), (20, 16));
    assert_eq!(view.pending[0].subject.len(), 256);
    assert!(view.pending[0].truncated);
    assert!(!view.pending[0].subject.contains('\n'));
    for id in 0..20 {
        answer(&mut o, json!(id), json!({"outcome":"selected","optionId":"yes"}));
    }
    assert_eq!(o.permissions().resolved.len(), 16);
    assert_eq!(o.permissions().pending_count, 0);
}

#[test]
fn conflicting_duplicate_requests_and_failed_same_session_replays_are_conservative() {
    let mut o = Observer::new("run");
    window_setup(&mut o);
    o.agent(&turn(4, "waiting"));
    permission(&mut o, json!(1), "s1", "Current", choices());
    permission(&mut o, json!(1), "s1", "Changed", json!([{"optionId":"yes","kind":"reject_once"}]));
    answer(&mut o, json!(1), json!({"outcome":"selected","optionId":"yes"}));
    assert_eq!(o.permissions().pending_count, 1);
    assert!(o.permissions().resolved.is_empty());
    let before = o.permissions();
    o.client(&line(json!({"jsonrpc":"2.0","id":"load2","method":"session/load","params":{"sessionId":"s1"}})));
    permission(&mut o, json!(2), "s1", "Failed replay", choices());
    assert_eq!(o.permissions(), before);
    o.agent(&line(json!({"jsonrpc":"2.0","id":"load2","error":{"code":-1,"message":"no"}})));
    assert_eq!(o.permissions(), before);
}

#[test]
fn authoritative_retirement_during_load_cannot_be_resurrected_by_replay_or_late_answers() {
    let mut o = Observer::new("run");
    window_setup(&mut o);
    o.agent(&turn(4, "waiting"));
    permission(&mut o, json!(1), "s1", "Original", choices());
    o.client(&line(json!({"jsonrpc":"2.0","id":"load2","method":"session/load","params":{"sessionId":"s1"}})));
    permission(&mut o, json!(2), "s1", "Replay", choices());
    o.agent(&turn(4, "working"));
    let retired = o.permissions().cursor;
    answer(&mut o, json!(1), json!({"outcome":"selected","optionId":"yes"}));
    answer(&mut o, json!(2), json!({"outcome":"selected","optionId":"yes"}));
    o.agent(&line(
        json!({"jsonrpc":"2.0","id":"load2","result":{"_meta":{CONSTRUCT_META:{"turn":{"turn":4,"state":"waiting"}}}}}),
    ));
    let view = o.permissions();
    assert_eq!(view.pending_count, 0);
    assert!(view.resolved.iter().all(|r| r.verdict == crate::permission::Verdict::Unknown));
    assert_ne!(view.cursor, retired, "stage commit has a fresh cursor");
}

#[test]
fn a_replay_reusing_an_active_id_cannot_classify_either_questions_answer() {
    let mut o = Observer::new("run");
    window_setup(&mut o);
    o.agent(&turn(4, "waiting"));
    permission(&mut o, json!(1), "s1", "Original", choices());
    o.client(&line(json!({"jsonrpc":"2.0","id":"load2","method":"session/load","params":{"sessionId":"s1"}})));
    permission(&mut o, json!(1), "s1", "Replayed", json!([{"optionId":"yes","kind":"reject_once"}]));
    answer(&mut o, json!(1), json!({"outcome":"selected","optionId":"yes"}));
    assert!(o.permissions().resolved.is_empty());
    o.agent(&line(
        json!({"jsonrpc":"2.0","id":"load2","result":{"_meta":{CONSTRUCT_META:{"turn":{"turn":4,"state":"waiting"}}}}}),
    ));
    assert_eq!(o.permissions().pending_count, 1);
    assert!(o.permissions().resolved.iter().all(|r| r.verdict == crate::permission::Verdict::Unknown));
}

#[test]
fn replay_and_current_answers_keep_their_observed_resolution_order() {
    let mut o = Observer::new("run");
    window_setup(&mut o);
    o.agent(&turn(4, "waiting"));
    permission(&mut o, json!(1), "s1", "Original", choices());
    o.client(&line(json!({"jsonrpc":"2.0","id":"load2","method":"session/load","params":{"sessionId":"s1"}})));
    permission(&mut o, json!(2), "s1", "Replayed", choices());
    answer(&mut o, json!(2), json!({"outcome":"selected","optionId":"yes"}));
    answer(&mut o, json!(1), json!({"outcome":"selected","optionId":"no"}));
    o.agent(&line(
        json!({"jsonrpc":"2.0","id":"load2","result":{"_meta":{CONSTRUCT_META:{"turn":{"turn":4,"state":"waiting"}}}}}),
    ));
    assert_eq!(
        o.permissions().resolved.iter().map(|r| r.verdict).collect::<Vec<_>>(),
        [crate::permission::Verdict::Allowed, crate::permission::Verdict::Denied]
    );
}
