//! The control channel under GPUI's headless test platform: command lines
//! are parsed as the socket would, `control::pump` dispatches them on a test
//! window, and the JSON replies are asserted on. The test scheduler forbids
//! wakeups from other threads, so lines are fed from the test thread; the
//! socket threads have their own unit test in `control::socket`. CI has no
//! compositor, so this stands in for driving the real window; AGENTS.md has
//! the live recipe.
#![cfg(unix)]
#![expect(clippy::unwrap_used, reason = "setup helpers outside #[test] fns; a failure is a test failure")]

use deck::control::{self, Request, protocol};
use deck::ui::board::PITCH;
use deck::ui::root::Deck;
use deck::ui::session::Status;
use deck_acp::{Inbound, Location, PermissionOption, ToolCall, ToolContent, ToolKind, ToolStatus, ToolUpdate};
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{AppContext as _, Focusable as _, TestAppContext, VisualTestContext};
use serde_json::{Value, json};
use std::sync::mpsc;
use std::time::Duration;

/// Open a Deck window with the control pump running, and its request sender.
fn open(cx: &mut TestAppContext) -> (gpui::Entity<Deck>, VisualTestContext, UnboundedSender<Request>) {
    let window = cx.update(|cx| cx.open_window(Default::default(), |_, cx| cx.new(Deck::detached)).unwrap());
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    let (tx, rx) = unbounded();
    vcx.update(|_, cx| control::pump(rx, window, cx));
    vcx.run_until_parked();
    (deck, vcx, tx)
}

/// Send one command line and run the window (advancing its clock) until it answers.
fn send(cx: &mut VisualTestContext, tx: &UnboundedSender<Request>, cmd: Value) -> Value {
    let (reply, answer) = mpsc::channel();
    let parsed = protocol::parse(&cmd.to_string()).unwrap();
    tx.unbounded_send(Request { cmd: parsed, reply }).unwrap();
    for _ in 0..2000 {
        cx.executor().advance_clock(Duration::from_millis(16));
        cx.run_until_parked();
        // Springs step by real time, so give each frame a real one too.
        std::thread::sleep(Duration::from_millis(16));
        if let Ok(v) = answer.try_recv() {
            assert_eq!(v["ok"], json!(true), "{cmd} failed: {v}");
            return v;
        }
    }
    panic!("no reply to {cmd}");
}

/// Send one command line and return its raw reply without asserting success.
fn send_raw(cx: &mut VisualTestContext, tx: &UnboundedSender<Request>, cmd: Value) -> Value {
    let (reply, answer) = mpsc::channel();
    let parsed = protocol::parse(&cmd.to_string()).unwrap();
    tx.unbounded_send(Request { cmd: parsed, reply }).unwrap();
    for _ in 0..2000 {
        cx.executor().advance_clock(Duration::from_millis(16));
        cx.run_until_parked();
        // Springs step by real time, so give each frame a real one too.
        std::thread::sleep(Duration::from_millis(16));
        if let Ok(v) = answer.try_recv() {
            return v;
        }
    }
    panic!("no reply to {cmd}");
}

#[gpui::test]
fn typing_and_answering_through_the_control_channel(cx: &mut TestAppContext) {
    let (deck, mut cx, d) = open(cx);
    deck.update(&mut cx, |d, cx| {
        d.on_inbound(Inbound::Ready { agent: "fake".into(), session_id: "s".into(), turn: None }, cx)
    });

    send(&mut cx, &d, json!({ "cmd": "type", "text": "Hi there" }));
    assert_eq!(send(&mut cx, &d, json!({ "cmd": "state" }))["draft"], json!("Hi there"));
    send(&mut cx, &d, json!({ "cmd": "key", "key": "enter" }));
    let s = send(&mut cx, &d, json!({ "cmd": "state" }));
    assert_eq!(s["draft"], json!(""));
    assert_eq!(s["session"]["status"], json!("working"));
    assert_eq!(s["session"]["entries"], json!([{ "you": "Hi there" }]));

    let options = ["allow", "reject"]
        .map(|k| PermissionOption { id: k.into(), name: k.into(), kind: format!("{k}_once") })
        .to_vec();
    deck.update(&mut cx, |d, cx| {
        d.on_inbound(Inbound::Permission { request: json!(9), title: "shell ls".into(), options }, cx)
    });
    assert_eq!(send(&mut cx, &d, json!({ "cmd": "state" }))["session"]["ask"]["title"], json!("shell ls"));
    send(&mut cx, &d, json!({ "cmd": "key", "key": "1" }));
    assert_eq!(send(&mut cx, &d, json!({ "cmd": "state" }))["session"]["ask"], Value::Null);
}

#[gpui::test]
fn state_reports_a_tool_calls_kind_diffs_and_locations(cx: &mut TestAppContext) {
    let (deck, mut cx, d) = open(cx);
    let edit = ToolCall {
        id: "e1".into(),
        title: "edit src/lib.rs".into(),
        kind: ToolKind::Edit,
        locations: vec![Location { path: "src/lib.rs".into(), line: Some(2) }],
        ..ToolCall::default()
    };
    let diff = ToolContent::Diff { path: "src/lib.rs".into(), old: Some("a".into()), new: "b".into() };
    let done = ToolUpdate {
        id: "e1".into(),
        status: Some(ToolStatus::Done),
        content: Some(vec![diff]),
        ..ToolUpdate::default()
    };
    deck.update(&mut cx, |d, cx| {
        d.on_inbound(Inbound::Tool(edit), cx);
        d.on_inbound(Inbound::ToolUpdate(done), cx);
    });
    assert_eq!(
        send(&mut cx, &d, json!({ "cmd": "state" }))["session"]["entries"],
        json!([{ "tool": "edit src/lib.rs", "id": "e1", "kind": "edit", "status": "done",
            "diffs": 1, "locations": ["src/lib.rs"] }])
    );
}

/// `state.header` names the selected task, its project and its status, after
/// a launch selected a task (issue #156).
#[gpui::test]
fn state_reports_the_headers_selected_task(cx: &mut TestAppContext) {
    use deck_runtime::{DaemonClient, Spec};
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-control-header-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let fake = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("deck-fake-agent");
    assert!(fake.is_file(), "build the fake agent beside deck: {}", fake.display());
    let argv = vec![fake.display().to_string()];
    let project_root = data.join("one");
    let lane = project_root.join("lane");
    std::fs::create_dir_all(&lane).unwrap();
    let opener = DaemonClient::connect_or_start(&data).unwrap();
    let project = opener.open_project(&project_root, argv.clone()).unwrap();
    let task =
        opener.create(Spec { argv: argv.clone(), cwd: lane.clone(), project: Some(project.id), github: None }).unwrap();
    // The header reads the record's title, not the checkout's name (#162).
    opener.title(&task.id, "name the row").unwrap();

    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, lane, cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    let (tx, rx) = unbounded();
    vcx.update(|_, cx| control::pump(rx, window, cx));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !deck.read_with(&vcx, |d, _| d.session().status == Status::Ready) {
        vcx.run_until_parked();
        assert!(std::time::Instant::now() < deadline, "the selected task never became ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    let s = send(&mut vcx, &tx, json!({ "cmd": "state" }));
    assert_eq!(s["header"], json!({ "title": "name the row", "project": "one", "detail": "ready" }));
    assert_eq!(s["main"], json!("task"), "a selected task is what the main area shows");

    let own = deck.read_with(&vcx, |d, _| d.task_id().map(str::to_owned));
    cx.quit();
    if let Some(id) = own {
        opener.stop(&id).unwrap();
    }
    drop(opener);
    let paths = deck_runtime::paths::Paths::for_data(&data).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while paths.control().exists() {
        assert!(std::time::Instant::now() < deadline, "test daemon did not exit after detaching");
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(paths.directory);
}

#[gpui::test]
fn dragging_and_the_rail_through_the_control_channel(cx: &mut TestAppContext) {
    let (deck, mut cx, d) = open(cx);
    let tasks = ["a", "b", "c"]
        .map(|id| {
            serde_json::from_value(json!({
                "id": id, "backend": "local",
                "spec": { "argv": ["x"], "cwd": format!("/src/{id}"), "project": "git:test" },
                "state": { "state": "running" }, "pid": null,
            }))
            .unwrap()
        })
        .to_vec();
    let project = deck_runtime::project::Project {
        id: "git:test".into(),
        name: "test".into(),
        checkouts: vec!["/src/a".into()],
        argv: vec!["x".into()],
        opened_ms: 1,
        hidden: false,
    };
    deck.update(&mut cx, |d, cx| {
        d.on_projects(vec![project], Some("git:test".into()), cx);
        d.on_tasks(tasks, Default::default(), cx);
    });

    send(&mut cx, &d, json!({ "cmd": "hover_rail" }));
    assert_eq!(send(&mut cx, &d, json!({ "cmd": "state" }))["rail"]["expanded"], json!(true));
    send(&mut cx, &d, json!({ "cmd": "settle" }));
    assert_eq!(
        send(&mut cx, &d, json!({ "cmd": "state" }))["sidebar"],
        json!([
            { "kind": "needs_header", "count": 0 },
            { "kind": "nothing" },
            { "kind": "projects_header" },
            { "kind": "project", "index": 0, "id": "git:test", "name": "test",
              "tasks": 3, "attention": false, "selected": true, "open": true },
            { "kind": "tasks" },
        ])
    );
    let to = 198. + PITCH * 2.2;
    send(
        &mut cx,
        &d,
        json!({ "cmd": "drag", "from": { "x": 148, "y": 198 }, "to": { "x": 148, "y": to }, "steps": 10 }),
    );
    let s = send(&mut cx, &d, json!({ "cmd": "state" }));
    assert_eq!(s["board"]["order"], json!([2, 3, 1]));
    assert_eq!(s["board"]["dragging"], Value::Null);
    assert_eq!(s["project"], json!("git:test"));
    assert_eq!(s["projects"], json!([{ "id": "git:test", "name": "test" }]));

    send(&mut cx, &d, json!({ "cmd": "select_rail", "index": 0 }));
    let s = send(&mut cx, &d, json!({ "cmd": "state" }));
    assert_eq!(s["rail"]["active"], json!(0));
    assert_eq!(s["main"], json!("project"), "selecting a project shows its view, not a task");
    assert!(cx.debug_bounds("project-view").is_some());
    let err = send_raw(&mut cx, &d, json!({ "cmd": "select_rail", "index": 1 }));
    assert_eq!(err["ok"], json!(false), "there is no rail item 1");
    send(&mut cx, &d, json!({ "cmd": "move", "x": 700, "y": 300 }));
    assert_eq!(send(&mut cx, &d, json!({ "cmd": "state" }))["rail"]["expanded"], json!(false));

    let g = send(&mut cx, &d, json!({ "cmd": "geometry" }));
    assert!(g["viewport"]["w"].as_f64().unwrap() > 0.);
}

#[gpui::test]
fn state_reports_the_needs_you_strip(cx: &mut TestAppContext) {
    let (deck, mut cx, d) = open(cx);
    let crashed = serde_json::from_value(json!({
        "id": "a", "backend": "local",
        "spec": { "argv": ["x"], "cwd": "/src/a", "project": "git:test" },
        "state": { "state": "stopped", "clean": false, "reason": "exit 1" }, "pid": null,
    }))
    .unwrap();
    let project = deck_runtime::project::Project {
        id: "git:test".into(),
        name: "test".into(),
        checkouts: vec!["/src/a".into()],
        argv: vec!["x".into()],
        opened_ms: 1,
        hidden: false,
    };
    deck.update(&mut cx, |d, cx| {
        d.on_projects(vec![project], Some("git:test".into()), cx);
        d.on_tasks(vec![crashed], Default::default(), cx);
    });
    assert_eq!(
        send(&mut cx, &d, json!({ "cmd": "state" }))["needs_you"],
        json!([{ "task": "a", "project": "git:test", "reason": "stopped unexpectedly" }])
    );
    // The row reads `project › title`, so the state names the project too.
    let sidebar = send(&mut cx, &d, json!({ "cmd": "state" }))["sidebar"].clone();
    let need = sidebar.as_array().unwrap().iter().find(|row| row["kind"] == "need").unwrap().clone();
    assert_eq!(need["project"], json!("test"), "the project's name, not its id");
    let label = format!("{} › {}", need["project"].as_str().unwrap(), need["title"].as_str().unwrap());
    assert!(label.starts_with("test › "), "the row reads project › title: {label}");
}
