//! Reopening the production window connection against real daemon holders.
//! The optional Construct cases use its own scripted model, with no paid calls.
#![cfg(unix)]

#[path = "support/reattach.rs"]
mod support;

use deck::ui::session::{Entry, Status};
use deck_acp::ToolStatus;
use deck_acp::names::{CONSTRUCT_META, LEGACY_CONSTRUCT_META, LEGACY_TURN_METHOD, TURN_METHOD};
use deck_runtime::State;
use serde_json::json;
use support::{Fixture, eventually, journal_finished};

#[test]
fn fake_stream_reopen_rebuilds_then_continues_the_same_agent() {
    let mut f = Fixture::fake("stream");
    let mut first = f.open();
    first.prompt("ship it");
    first.until(|a| !a.text().is_empty());
    let pid = f.pid();
    first.detach();

    let mut next = f.open();
    assert_eq!(f.pid(), pid);
    assert_eq!(next.session.status, Status::Working);
    assert!(!next.text().is_empty());
    next.until(|a| a.session.ask.is_some());
    next.answer();
    next.idle();
    assert_eq!(next.text(), "You said \"ship it\". Checking the workspace first.\npermission: allow");
    next.assert_user_once();
}

#[test]
fn fake_waiting_reopen_renews_permission_and_rejects_stale_answers() {
    let mut f = Fixture::fake("waiting");
    let mut first = f.open();
    first.prompt("ship it");
    first.until(|a| a.session.ask.is_some());
    let old = first.session.ask.as_ref().unwrap().request.clone();
    let pid = f.pid();
    first.detach();

    let mut next = f.open();
    assert_eq!(f.pid(), pid);
    assert_eq!(next.session.status, Status::Waiting);
    let new = next.session.ask.as_ref().unwrap().request.clone();
    assert_ne!(new, old);
    assert!(next.client.as_ref().unwrap().answer(&old, Some("reject")).is_err());
    next.answer();
    assert!(next.client.as_ref().unwrap().answer(&new, Some("reject")).is_err());
    next.idle();
    assert_eq!(next.text(), "You said \"ship it\". Checking the workspace first.\npermission: allow");
    next.assert_user_once();
}

#[test]
fn fake_detached_completion_reopens_idle_and_stop_closes_before_signalling() {
    let mut f = Fixture::fake("complete");
    let mut first = f.open();
    first.prompt("ship it");
    first.until(|a| a.session.ask.is_some());
    first.answer();
    first.detach();
    eventually(|| journal_finished(&f.root));
    let pid = f.pid();

    let next = f.open();
    assert_eq!(f.pid(), pid);
    assert_eq!(next.session.status, Status::Ready);
    assert_eq!(next.text(), "You said \"ship it\". Checking the workspace first.\npermission: allow");
    next.assert_user_once();
    // Stop takes over even with a window still attached.
    f.daemon.stop(f.id.as_ref().unwrap()).unwrap();
    assert_eq!(std::fs::read_to_string(f.root.join("closed")).unwrap(), next.sid);
    assert!(matches!(f.daemon.task(f.id.as_ref().unwrap()).unwrap().state, State::Stopped { clean: true, .. }));
}

#[test]
fn daemon_crash_mid_turn_is_adopted_and_reopened_without_restarting_the_agent() {
    let mut f = Fixture::fake("adopt-stream");
    let mut first = f.open();
    first.prompt("ship it");
    first.until(|a| !a.text().is_empty());
    let before = f.daemon.task(f.id.as_ref().unwrap()).unwrap();
    f.crash_and_restart_daemon();
    first.detach();
    let mut next = f.open();
    let after = f.daemon.task(f.id.as_ref().unwrap()).unwrap();
    assert_eq!(before.pid, after.pid);
    assert_eq!(before.holder, after.holder);
    next.until(|a| a.session.ask.is_some());
    next.answer();
    next.idle();
    assert_eq!(next.text(), "You said \"ship it\". Checking the workspace first.\npermission: allow");
    next.assert_user_once();
}

#[test]
fn construct_stream_reopen_has_no_missing_or_duplicate_content() {
    let answer = "zero one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen";
    let Some(mut f) = Fixture::construct("real-stream", json!({ "steps": [{ "text": answer }] })) else { return };
    let mut first = f.open();
    first.prompt("ship it");
    first.until(|a| !a.text().is_empty());
    let pid = f.pid();
    first.detach();
    let mut next = f.open();
    assert_eq!(f.pid(), pid);
    assert_eq!(next.session.status, Status::Working);
    assert!(!next.text().is_empty());
    next.idle();
    assert_eq!(next.text(), answer);
    next.assert_user_once();
    assert_eq!(f.model_calls(), 1);
    f.assert_no_recovery();
}

#[test]
fn construct_waiting_reopen_continues_the_original_tool() {
    // The read lies outside the workspace, so Construct asks permission.
    let outside = std::env::temp_dir().join(format!("deck-outside-{}.txt", std::process::id()));
    let Some(mut f) = Fixture::construct(
        "real-wait",
        json!({ "steps": [
        { "call": "read", "args": { "path": outside } }, { "text": "done" }
    ] }),
    ) else {
        return;
    };
    std::fs::write(&outside, "permission proved\n").unwrap();
    let mut first = f.open();
    first.prompt("ship it");
    first.until(|a| a.session.ask.is_some());
    let old = first.session.ask.as_ref().unwrap().request.clone();
    let pid = f.pid();
    first.detach();
    let mut next = f.open();
    assert_eq!(f.pid(), pid);
    assert_eq!(next.session.status, Status::Waiting);
    assert_ne!(next.session.ask.as_ref().unwrap().request, old);
    assert!(next.client.as_ref().unwrap().answer(&old, Some("deny")).is_err());
    next.answer();
    next.idle();
    assert_eq!(next.text(), "done");
    assert_eq!(next.session.entries.iter().filter(|e| matches!(e, Entry::Tool { .. })).count(), 1);
    assert!(next.session.entries.iter().any(|e| matches!(e, Entry::Tool { status: ToolStatus::Done, .. })));
    next.assert_user_once();
    assert_eq!(f.model_calls(), 2);
    f.assert_no_recovery();
    std::fs::remove_file(outside).unwrap();
}

#[test]
fn construct_finishes_while_detached_and_reopens_idle() {
    let Some(mut f) = Fixture::construct("real-complete", json!({ "steps": [{ "text": "finished while detached" }] }))
    else {
        return;
    };
    let mut first = f.open();
    first.prompt("ship it");
    first.until(|a| !a.text().is_empty());
    let pid = f.pid();
    first.detach();
    // Read the agent's journal to establish completion without attaching.
    eventually(|| {
        let output = std::process::Command::new("python3").arg("-c").arg(
            "import glob,sqlite3,sys; p=glob.glob(sys.argv[1]+'/home/projects/*/journal.sqlite'); c=sqlite3.connect(p[0]); sys.exit(0 if c.execute(\"select count(*) from events where type='turn_end'\").fetchone()[0] else 1)"
        ).arg(&f.root).output().unwrap();
        output.status.success()
    });
    let next = f.open();
    assert_eq!(f.pid(), pid);
    assert_eq!(next.session.status, Status::Ready);
    assert_eq!(next.text(), "finished while detached");
    next.assert_user_once();
    assert_eq!(f.model_calls(), 1);
    f.assert_no_recovery();
}

#[expect(clippy::unwrap_used, reason = "raw protocol test setup and observations must succeed")]
fn raw_retired_reply_is_ignored(mut f: Fixture) {
    use deck_acp::rpc;
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let mut first = f.open();
    first.prompt("ship it");
    first.until(|a| a.session.ask.is_some());
    let old = first.session.ask.as_ref().unwrap().request.clone();
    let sid = first.sid.clone();
    first.detach();
    let path = deck_runtime::paths::Paths::for_data(&f.root.join("data")).unwrap().task(f.id.as_ref().unwrap());
    let mut socket = UnixStream::connect(path).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut input = BufReader::new(socket.try_clone().unwrap());
    let mut read = || {
        let mut line = String::new();
        assert!(input.read_line(&mut line).unwrap() > 0);
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    };
    socket.write_all(rpc::request("raw-init", "initialize", json!({ "protocolVersion": 1 })).as_bytes()).unwrap();
    loop {
        if read()["id"] == "raw-init" {
            break;
        }
    }
    // Bypass Client's local rejection and send the retired answer on the wire.
    socket
        .write_all(rpc::ok(&old, json!({ "outcome": { "outcome": "selected", "optionId": "deny" } })).as_bytes())
        .unwrap();
    socket
        .write_all(rpc::request("raw-load", "session/load", json!({ "sessionId": sid, "cwd": f.cwd })).as_bytes())
        .unwrap();
    let mut fresh = None;
    loop {
        let value = read();
        if value["method"] == "session/request_permission" {
            fresh = Some(value.clone());
        }
        if value["id"] == "raw-load" {
            let meta = &value["result"]["_meta"];
            let snapshot = &meta[CONSTRUCT_META]["turn"];
            let snapshot = if snapshot.is_null() { &meta[LEGACY_CONSTRUCT_META]["turn"] } else { snapshot };
            assert_eq!(snapshot["state"], "waiting");
            break;
        }
    }
    let fresh = fresh.unwrap();
    assert_ne!(fresh["id"], old);
    let allow =
        fresh["params"]["options"].as_array().unwrap().iter().find(|o| o["kind"] == "allow_once").unwrap()["optionId"]
            .clone();
    socket
        .write_all(rpc::ok(&fresh["id"], json!({ "outcome": { "outcome": "selected", "optionId": allow } })).as_bytes())
        .unwrap();
    loop {
        let value = read();
        assert!(value.get("method").is_some(), "an old prompt response crossed attachments: {value}");
        let turn = value["method"] == TURN_METHOD || value["method"] == LEGACY_TURN_METHOD;
        if turn && value["params"]["state"] == "idle" {
            break;
        }
    }
    socket.write_all(rpc::request("barrier", "unknown/barrier", json!({})).as_bytes()).unwrap();
    loop {
        let value = read();
        if value["id"] == "barrier" {
            break;
        }
        assert!(value.get("method").is_some(), "old response after idle: {value}");
    }
}

#[test]
fn fake_retires_permission_ids_on_the_wire_and_suppresses_old_prompt_responses() {
    raw_retired_reply_is_ignored(Fixture::fake("raw-stale"));
}

#[test]
fn construct_retires_permission_ids_on_the_wire_and_suppresses_old_prompt_responses() {
    let outside = std::env::temp_dir().join(format!("deck-outside-raw-{}.txt", std::process::id()));
    let Some(f) = Fixture::construct(
        "real-raw-stale",
        json!({ "steps": [
        { "call": "read", "args": { "path": outside } }, { "text": "done" }
    ] }),
    ) else {
        return;
    };
    std::fs::write(&outside, "secret\n").unwrap();
    raw_retired_reply_is_ignored(f);
    std::fs::remove_file(outside).unwrap();
}
