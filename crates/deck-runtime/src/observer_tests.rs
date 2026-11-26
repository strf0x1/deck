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
    assert_eq!(step.events, vec![Event::Turn(Turn { number: 3, state: Phase::Idle, stop_reason: None })]);
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
            matches!(step.events.last(), Some(Event::Failed(message)) if message.contains("does not support idempotent"))
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
    assert_eq!(step.events, vec![Event::Session("s1".into())]);
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
    assert_eq!(step.events, vec![Event::Session("s1".into())]);
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
        "toolCall": { "title": "Read" }, "options": []
    } }))
    };
    o.agent(&ask(json!(1)));
    o.client(&line(json!({ "jsonrpc": "2.0", "id": "deck-9", "method": "initialize", "params": {} })));
    assert!(o.pending_questions().unwrap().is_empty());
    o.agent(&ask(json!(2)));
    o.client(&line(json!({ "jsonrpc": "2.0", "id": 1, "result": {} })));
    assert_eq!(o.pending_questions().unwrap()[0].request, json!(2));
}

#[test]
fn questions_too_large_to_preserve_remain_open() {
    let mut o = Observer::new("t");
    window_setup(&mut o);
    o.agent(&line(json!({ "jsonrpc": "2.0", "id": 1, "method": "session/request_permission", "params": {
        "toolCall": { "title": "x".repeat(MAX_TITLE_BYTES + 1) }, "options": []
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
            "toolCall": { "title": "Read" }, "options": []
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
