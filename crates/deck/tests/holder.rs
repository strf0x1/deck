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
    let spec = Spec { argv: f.argv.clone(), cwd: f.cwd.clone(), project: None, github: None, model: None };
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
    let turn = wait_for(&f, &id, 0).turn.unwrap();
    assert_eq!((turn.number, turn.state, turn.stop_reason.as_deref()), (1, Phase::Idle, Some("end_turn")));

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
    f.crash_and_restart_daemon();
    let turn = f.daemon.task(&id).unwrap().turn.unwrap();
    assert_eq!((turn.number, turn.state), (1, Phase::Waiting));
    // The holder outlived deckd; the next turn still reaches the new one.
    let mut window = f.open();
    window.until(|a| a.session.ask.is_some());
    window.answer();
    eventually(|| f.daemon.task(&id).unwrap().turn.is_some_and(|t| t.state == Phase::Idle));
}

#[test]
fn a_wait_times_out_cleanly_and_a_stopped_task_cannot_be_prompted() {
    let mut f = fixture("holder-wait");
    let id = start(&mut f);
    let waited = f.daemon.wait(&id, None, 100).unwrap();
    assert!(waited.timed_out);
    f.daemon.stop(&id).unwrap();
    let refused = f.daemon.prompt(&id, "hello", None).unwrap_err();
    assert!(format!("{refused:#}").contains("not running"), "{refused:#}");
    std::thread::sleep(Duration::from_millis(10));
}
