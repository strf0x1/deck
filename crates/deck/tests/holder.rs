//! The holder as the task's ACP client (ADR 0011 §3, deck#150): prompting a
//! task through `deckd` with and without a window, its recorded turn, the
//! prompt log and notices, and that the holder never initializes a stream a
//! window already set up. Real `deckd`, holders and fake agent.
#![cfg(unix)]

#[path = "support/reattach.rs"]
#[expect(dead_code, reason = "the reattach fixture is shared; these tests use part of it")]
mod support;

use deck::ui::session::Status;
use deck_runtime::{Phase, PromptReply, Spec, Task};
use std::time::Duration;
use support::{Fixture, eventually};

/// A fake-agent fixture whose agent logs each `initialize` by client name.
fn fixture(name: &str) -> Fixture {
    let mut f = Fixture::fake(name);
    let log = f.root.join("inits");
    f.argv.insert(1, format!("DECK_FAKE_INIT_LOG={}", log.display()));
    f
}

fn inits(f: &Fixture) -> Vec<String> {
    std::fs::read_to_string(f.root.join("inits")).unwrap_or_default().lines().map(str::to_owned).collect()
}

/// Start the task through `deckd` alone, as a coordinator's `create` will.
#[expect(clippy::unwrap_used, reason = "test fixture actions must succeed")]
fn start(f: &mut Fixture) -> String {
    let spec = Spec {
        argv: f.argv.clone(),
        cwd: f.cwd.clone(),
        project: None,
        github: None,
        model: None,
        ..Default::default()
    };
    let id = f.daemon.run(spec).unwrap().task.id;
    f.id = Some(id.clone());
    id
}

#[expect(clippy::unwrap_used, reason = "test fixture actions must succeed")]
fn wait_for(f: &Fixture, id: &str, after: u64) -> Task {
    let waited = f.daemon.wait(id, Some(after), 15_000).unwrap();
    assert!(!waited.timed_out, "turn after {after} did not stop working: {:?}", waited.task.turn);
    waited.task
}

/// The window consumes ACP directly; the holder records it on another thread.
#[expect(clippy::unwrap_used, reason = "test process observations must succeed")]
fn wait_idle(f: &Fixture, id: &str) -> Task {
    eventually(|| f.daemon.task(id).unwrap().turn.is_some_and(|turn| turn.state == Phase::Idle));
    f.daemon.task(id).unwrap()
}

#[test]
fn with_no_window_a_prompt_runs_a_turn_that_the_record_follows() {
    let mut f = fixture("holder-alone");
    let id = start(&mut f);
    let mut notices = f.daemon.subscribe().unwrap();
    assert_eq!(f.daemon.prompt(&id, "ship it", Some("coord-1")).unwrap(), PromptReply::Started { turn: 1 });
    assert_eq!(f.daemon.prompt(&id, "again", None).unwrap(), PromptReply::Busy, "one turn at a time");

    // The fake asks permission; nobody can answer it without a window.
    let task = wait_for(&f, &id, 0);
    let turn = task.turn.unwrap();
    assert_eq!((turn.number, turn.state), (1, Phase::Waiting));
    assert!(task.session_id.is_some(), "the holder's session is recorded");
    eventually(|| f.daemon.task(&id).unwrap().permissions.is_some_and(|p| p.pending_count == 1));
    let permissions = f.daemon.task(&id).unwrap().permissions.unwrap();
    assert_eq!(permissions.pending_count, 1);
    assert_eq!(permissions.pending[0].subject, "shell git push origin deck/spike");
    assert_eq!(permissions.pending[0].kind, deck_runtime::permission::Kind::Execute);
    assert_eq!(inits(&f), ["deck-holder"]);

    let prompts = f.daemon.prompts(&id).unwrap();
    assert_eq!(prompts.len(), 1);
    assert_eq!(
        (prompts[0].turn, prompts[0].from.as_deref(), prompts[0].text.as_str()),
        (1, Some("coord-1"), "ship it")
    );
    let mut announced = None;
    let mut working = false;
    eventually(|| {
        while let Ok(notice) = notices.try_recv() {
            working |= notice.turn.as_ref().is_some_and(|t| t.state == Phase::Working);
            if let Some(prompt) = notice.prompt {
                announced = Some(prompt);
            }
        }
        announced.is_some() && working
    });
    assert_eq!(announced.unwrap().from.as_deref(), Some("coord-1"));
}

#[test]
fn a_window_attaching_mid_turn_is_re_asked_and_the_holder_records_the_end() {
    let mut f = fixture("holder-attach");
    let id = start(&mut f);
    assert_eq!(f.daemon.prompt(&id, "ship it", None).unwrap(), PromptReply::Started { turn: 1 });
    wait_for(&f, &id, 0);

    let mut window = f.open();
    window.until(|a| a.session.ask.is_some());
    window.answer();
    window.idle();
    let turn = wait_idle(&f, &id).turn.unwrap();
    assert_eq!((turn.number, turn.state, turn.stop_reason.as_deref()), (1, Phase::Idle, Some("end_turn")));

    let permissions = f.daemon.task(&id).unwrap().permissions.unwrap();
    assert_eq!(permissions.pending_count, 0);
    assert!(permissions.resolved.iter().any(|r| r.verdict == deck_runtime::permission::Verdict::Allowed));
    let stored = std::fs::read_to_string(f.root.join("data/tasks").join(&id).join("observation.json")).unwrap();
    assert!(!stored.contains("git push") && !stored.contains("optionId") && !stored.contains("Destination"));

    // The window set its own attachment up; the holder prompts on it as is.
    assert_eq!(f.daemon.prompt(&id, "and the docs", None).unwrap(), PromptReply::Started { turn: 2 });
    window.until(|a| a.session.status == Status::Working || a.session.ask.is_some());
    assert_eq!(inits(&f), ["deck-holder", "deck"]);
}

#[test]
fn a_user_started_task_is_prompted_after_its_window_closes_without_another_initialize() {
    let mut f = fixture("holder-after-window");
    let mut window = f.open();
    let id = f.id.clone().unwrap();
    window.prompt("first");
    window.until(|a| a.session.ask.is_some());
    window.answer();
    window.idle();
    window.detach();

    assert_eq!(f.daemon.prompt(&id, "second", None).unwrap(), PromptReply::Started { turn: 2 });
    let turn = wait_for(&f, &id, 1).turn.unwrap();
    assert_eq!((turn.number, turn.state), (2, Phase::Waiting));
    assert_eq!(inits(&f), ["deck"], "the holder used the window's session");
}

#[test]
fn a_restarted_deckd_keeps_the_last_turn() {
    let mut f = fixture("holder-adopt");
    let id = start(&mut f);
    f.daemon.prompt(&id, "ship it", None).unwrap();
    wait_for(&f, &id, 0);
    eventually(|| f.daemon.task(&id).unwrap().permissions.is_some_and(|p| p.pending_count == 1));
    let before = f.daemon.task(&id).unwrap().permissions.unwrap();
    f.crash_and_restart_daemon();
    assert_eq!(f.daemon.task(&id).unwrap().permissions.as_ref(), Some(&before));
    let turn = f.daemon.task(&id).unwrap().turn.unwrap();
    assert_eq!((turn.number, turn.state), (1, Phase::Waiting));
    // The holder outlived deckd; the next turn still reaches the new one.
    let mut window = f.open();
    window.until(|a| a.session.ask.is_some());
    window.answer();
    eventually(|| f.daemon.task(&id).unwrap().turn.is_some_and(|t| t.state == Phase::Idle));
}

#[test]
fn a_prompt_resumes_a_stopped_task_and_its_session() {
    let mut f = fixture("holder-wait");
    let id = start(&mut f);
    let waited = f.daemon.wait(&id, None, 100).unwrap();
    assert!(waited.timed_out);
    assert_eq!(f.daemon.prompt(&id, "first", None).unwrap(), PromptReply::Started { turn: 1 });
    wait_for(&f, &id, 0);
    let mut window = f.open();
    window.until(|a| a.session.ask.is_some());
    window.answer();
    window.idle();
    window.detach();
    let session = f.daemon.task(&id).unwrap().session_id.unwrap();
    f.daemon.stop(&id).unwrap();
    assert_eq!(f.daemon.prompt(&id, "second", None).unwrap(), PromptReply::Started { turn: 2 });
    let task = wait_for(&f, &id, 1);
    assert_eq!(task.session_id.as_deref(), Some(session.as_str()));
    assert_eq!(task.turn.unwrap().state, Phase::Waiting);
    let prompts = f.daemon.prompts(&id).unwrap();
    assert_eq!(prompts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["first", "second"]);
}

#[test]
fn concurrent_prompts_share_one_resume() {
    let mut f = fixture("holder-concurrent-resume");
    let id = start(&mut f);
    f.daemon.stop(&id).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|text| {
            let client = deck_runtime::DaemonClient::connect(&f.root.join("data")).unwrap();
            let id = id.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                client.prompt(&id, text, None).unwrap()
            })
        })
        .collect();
    barrier.wait();
    let replies: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
    assert_eq!(replies.iter().filter(|reply| **reply == PromptReply::Started { turn: 1 }).count(), 1);
    assert_eq!(replies.iter().filter(|reply| **reply == PromptReply::Busy).count(), 1);
    assert_eq!(f.daemon.prompts(&id).unwrap().len(), 1);
}

#[test]
fn a_coordinator_reads_the_question_and_observed_answer_through_control() {
    use serde_json::{Value, json};
    use std::io::{BufRead as _, Write as _};
    use std::os::unix::net::UnixStream;
    let mut f = fixture("holder-control-permissions");
    let id = start(&mut f);
    let worker = f.daemon.task(&id).unwrap();
    let coordinator = f
        .daemon
        .create(Spec {
            argv: f.argv.clone(),
            cwd: f.cwd.clone(),
            project: worker.spec.project.clone(),
            kind: deck_runtime::TaskKind::Coordinator,
            ..Default::default()
        })
        .unwrap();
    let data = f.root.join("data");
    let call = |request: Value| {
        let paths = deck_runtime::paths::Paths::for_data(&data).unwrap();
        let mut socket = UnixStream::connect(paths.control()).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        writeln!(socket, "{}", json!({"op":"control","task":coordinator.id,"request":request})).unwrap();
        let mut line = String::new();
        std::io::BufReader::new(socket).read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        reply["ok"].clone()
    };
    f.daemon.prompt(&id, "ship it", None).unwrap();
    wait_for(&f, &id, 0);
    eventually(|| f.daemon.task(&id).unwrap().permissions.is_some_and(|p| p.pending_count == 1));
    let pending = call(json!({"operation":"task","id":id}));
    assert_eq!(
        pending["reply"]["task"]["permissions"]["pending"][0]["subject"], "shell git push origin deck/spike",
        "{pending}"
    );
    let mut window = f.open();
    window.until(|a| a.session.ask.is_some());
    window.answer();
    window.idle();
    wait_idle(&f, &id);
    let finished = call(json!({"operation":"wait","id":id,"after_turn":0,"wait_ms":1000}));
    let permissions = &finished["reply"]["task"]["permissions"];
    assert_eq!(permissions["pending_count"], 0, "{finished}");
    assert!(permissions["resolved"].as_array().unwrap().iter().any(|r| r["verdict"] == "allowed"), "{finished}");
    assert!(!permissions.to_string().contains("git push"));
    f.daemon.destroy(&coordinator.id).unwrap();
}
