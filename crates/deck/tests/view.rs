//! The Deck view under GPUI's headless test platform: real layout, real
//! input dispatch, no window server. This is how an agent proves a UI change
//! without looking at the screen.
#![expect(clippy::unwrap_used, reason = "setup helpers outside #[test] fns; a failure is a test failure")]

use deck::ui::board::PITCH;
use deck::ui::root::Deck;
use deck::ui::session::{Entry, Status};
use deck_acp::{Inbound, PermissionOption};
use deck_runtime::approvals::{Approval, Details};
use gpui::{AppContext as _, Focusable as _, Modifiers, MouseButton, TestAppContext, VisualTestContext, point, px};
use serde_json::json;

fn open(cx: &mut TestAppContext) -> (gpui::Entity<Deck>, VisualTestContext) {
    let window = cx.update(|cx| cx.open_window(Default::default(), |_, cx| cx.new(Deck::detached)).unwrap());
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    (deck, vcx)
}

#[gpui::test]
fn typing_a_prompt_and_answering_a_permission_by_key(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    deck.update(&mut cx, |d, cx| {
        d.on_inbound(Inbound::Ready { agent: "construct".into(), session_id: "s".into(), turn: None }, cx)
    });

    cx.simulate_keystrokes("h i backspace o enter");
    deck.read_with(&cx, |d, _| {
        assert_eq!(d.session().entries, vec![Entry::You("ho".into())]);
        assert_eq!(d.session().status, Status::Working);
        assert_eq!(d.draft(), "");
    });

    let options = ["allow", "reject"]
        .map(|k| PermissionOption { id: k.into(), name: k.into(), kind: format!("{k}_once") })
        .to_vec();
    deck.update(&mut cx, |d, cx| {
        d.on_inbound(Inbound::Permission { request: json!(9), title: "shell rm -rf x".into(), options }, cx)
    });
    // While a question is open, typing doesn't reach the prompt; digits answer.
    cx.simulate_keystrokes("z 2");
    deck.read_with(&cx, |d, _| {
        assert!(d.session().ask.is_none(), "the answer closes the question");
        assert_eq!(d.draft(), "");
    });
}

#[gpui::test]
fn loading_construct_history_preserves_terminal_tool_states(cx: &mut TestAppContext) {
    use deck_acp::{ToolStatus, conn::Conn};

    let (deck, mut cx) = open(cx);
    let (mut conn, _) = Conn::load("/w".into(), "saved".into());
    // Captured from Construct #156 at 996b2d3; only the session id and
    // temporary error path are normalized. No later status update follows.
    for line in include_str!("../../deck-acp/tests/fixtures/construct-load.jsonl").lines() {
        for event in conn.read(line).events {
            deck.update(&mut cx, |d, cx| d.on_inbound(event, cx));
        }
    }
    cx.run_until_parked();
    deck.read_with(&cx, |d, _| {
        assert_eq!(
            d.session().entries,
            vec![
                Entry::You("read files".into()),
                Entry::Tool { id: "call_1".into(), title: "read note.txt".into(), status: ToolStatus::Done },
                Entry::Tool { id: "call_2".into(), title: "read missing.txt".into(), status: ToolStatus::Failed },
                Entry::Agent("done".into()),
            ]
        );
        assert_eq!(d.session().status, Status::Ready);
    });
}

#[gpui::test]
fn reattached_turn_state_controls_typing_and_permission_keys(cx: &mut TestAppContext) {
    use deck_acp::TurnState;
    let (deck, mut cx) = open(cx);
    deck.update(&mut cx, |d, cx| {
        d.on_inbound(Inbound::UserMessage("go".into()), cx);
        d.on_inbound(Inbound::Message("partial".into()), cx);
        d.on_inbound(
            Inbound::Ready { agent: "construct".into(), session_id: "saved".into(), turn: Some(TurnState::Working) },
            cx,
        );
    });
    cx.simulate_keystrokes("x enter");
    deck.read_with(&cx, |d, _| {
        assert_eq!(d.session().status, Status::Working);
        assert_eq!(d.session().entries, vec![Entry::You("go".into()), Entry::Agent("partial".into())]);
        assert_eq!(d.draft(), "x");
    });
    deck.update(&mut cx, |d, cx| {
        d.on_inbound(
            Inbound::Permission {
                request: json!(902),
                title: "read outside".into(),
                options: vec![PermissionOption { id: "once".into(), name: "Allow".into(), kind: "allow_once".into() }],
            },
            cx,
        );
        d.on_inbound(Inbound::Turn { number: 1, state: TurnState::Waiting }, cx);
    });
    cx.simulate_keystrokes("z 1");
    deck.read_with(&cx, |d, _| {
        assert!(d.session().ask.is_none());
        assert_eq!(d.session().status, Status::Working);
        assert_eq!(d.draft(), "x");
    });
    deck.update(&mut cx, |d, cx| {
        d.on_inbound(Inbound::Message(" continued".into()), cx);
        d.on_inbound(Inbound::Turn { number: 1, state: TurnState::Idle { stop_reason: Some("end_turn".into()) } }, cx);
    });
    cx.simulate_keystrokes("backspace h i enter");
    deck.read_with(&cx, |d, _| {
        assert_eq!(
            d.session().entries,
            vec![Entry::You("go".into()), Entry::Agent("partial continued".into()), Entry::You("hi".into())]
        );
    });
}

fn approval(id: &str) -> Approval {
    Approval {
        id: id.into(),
        task: "t".into(),
        project: "/w".into(),
        session: None,
        gateway: "model".into(),
        operation: "chat_completions".into(),
        destination: "other".into(),
        credential: Some("OPENAI_API_KEY".into()),
        details: Details { method: Some("POST".into()), path: Some("/v1/chat/completions".into()), size: Some(0) },
        reason: None,
        created_ms: 0,
        expires_ms: 0,
        policy: 0,
        state: deck_runtime::approvals::State::Pending,
        waiters: 0,
    }
}

#[gpui::test]
fn a_boundary_card_takes_the_decision_keys_instead_of_the_prompt(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    deck.update(&mut cx, |d, cx| d.on_boundary(approval("a1"), cx));
    deck.read_with(&cx, |d, _| {
        assert_eq!(d.boundary().pending().map(|item| item.destination.as_str()), Some("other"));
    });
    // No task is attached in this view, so the decision goes nowhere; the
    // point is that the keys never become prompt text.
    cx.simulate_keystrokes("2 h i");
    deck.read_with(&cx, |d, _| {
        assert_eq!(d.draft(), "");
        assert!(d.boundary().pending().is_some(), "a failed decision leaves the card open");
    });
}

#[gpui::test]
fn dragging_a_card_with_the_mouse_reorders_lanes(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    cx.run_until_parked();
    let before = deck.read_with(&cx, |d, _| d.board().order());
    assert_eq!(before, vec![0, 1, 2, 3]);

    // The first card sits under the Sessions heading, right of the closed rail.
    let start = point(px(200.), px(150.));
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    assert_eq!(deck.read_with(&cx, |d, _| d.board().dragging()), Some(0), "mouse down on the first card grabs it");
    for step in 1..=10 {
        let y = start.y + px(PITCH * 2.2 * step as f32 / 10.);
        cx.simulate_mouse_move(point(start.x, y), Some(MouseButton::Left), Modifiers::none());
    }
    cx.simulate_mouse_up(point(start.x, start.y + px(PITCH * 2.2)), MouseButton::Left, Modifiers::none());
    deck.read_with(&cx, |d, _| {
        assert_eq!(d.board().order(), vec![1, 2, 0, 3]);
        assert_eq!(d.board().dragging(), None);
    });
}

#[gpui::test]
fn the_rail_opens_under_the_pointer_and_selects_by_click(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    cx.run_until_parked();
    assert!(!deck.read_with(&cx, |d, _| d.rail().expanded()));

    // The fourth icon (Files), inside the closed rail.
    let files = point(px(36.), px(217.));
    cx.simulate_mouse_move(files, None, Modifiers::none());
    assert!(deck.read_with(&cx, |d, _| d.rail().expanded()), "hovering the rail opens it");
    cx.simulate_click(files, Modifiers::none());
    assert_eq!(deck.read_with(&cx, |d, _| d.rail().active()), 3);

    cx.simulate_mouse_move(point(px(700.), px(300.)), None, Modifiers::none());
    assert!(!deck.read_with(&cx, |d, _| d.rail().expanded()), "leaving the rail closes it");
}

#[gpui::test]
fn the_quit_key_is_bound(cx: &mut TestAppContext) {
    cx.update(deck::app::bind);
    let (_deck, mut cx) = open(cx);
    let binding = cx.update(|window, _| window.highest_precedence_binding_for_action(&deck::app::Quit)).unwrap();
    let [key] = binding.keystrokes() else { panic!("quit is one keystroke") };
    assert_eq!(key.key(), "q");
    let m = key.modifiers();
    assert!(if cfg!(target_os = "macos") { m.platform } else { m.control }, "{}", deck::app::QUIT_KEY);
}

#[cfg(unix)]
#[gpui::test]
fn quitting_detaches_and_the_windows_task_keeps_running(cx: &mut TestAppContext) {
    use deck_runtime::{DaemonClient, State};
    // The ACP reader wakes the UI from a real process, outside the test scheduler.
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-quit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, std::env::temp_dir(), cx)))
            .unwrap()
    });
    let vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx.clone()).unwrap();
    // A failed launch only lands in the session, so name it in the panic.
    let id = deck.read_with(&vcx, |d, _| {
        d.task_id().map(str::to_owned).unwrap_or_else(|| panic!("the window runs no task: {:?}", d.session().status))
    });
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    let before = daemon.task(&id).unwrap();
    assert_eq!(before.state, State::Running);

    cx.quit();
    let after = daemon.task(&id).unwrap();
    assert_eq!(after.state, State::Running);
    assert_eq!(after.pid, before.pid);
    assert!(
        std::process::Command::new("kill").args(["-0", &after.pid.unwrap().to_string()]).status().unwrap().success(),
        "the agent process remains alive after Deck quits"
    );
    daemon.stop(&id).unwrap();
    drop(daemon);
    cleanup_daemon(&data);
}

#[cfg(unix)]
#[gpui::test]
fn reopening_a_running_task_requires_the_restart_shortcut(cx: &mut TestAppContext) {
    use deck_runtime::{DaemonClient, Spec, State};
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let cwd = std::env::temp_dir();
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    let original = daemon.run(Spec { argv: argv.clone(), cwd: cwd.clone() }).unwrap().task;
    cx.update(deck::app::bind);
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, cwd, cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    deck.read_with(&vcx, |d, _| {
        assert_eq!(d.task_id(), Some(original.id.as_str()));
        assert_eq!(d.session().status, Status::Detached);
        assert!(!d.session().can_prompt());
    });
    let binding =
        vcx.update(|window, _| window.highest_precedence_binding_for_action(&deck::app::RestartTask)).unwrap();
    let [key] = binding.keystrokes() else { panic!("restart is one keystroke") };
    assert_eq!(key.key(), "r");
    assert!(if cfg!(target_os = "macos") { key.modifiers().platform } else { key.modifiers().control });

    vcx.simulate_keystrokes("x enter");
    deck.read_with(&vcx, |d, _| {
        assert_eq!(d.session().status, Status::Detached);
        assert!(d.session().entries.is_empty());
        assert!(d.draft().is_empty());
    });
    vcx.simulate_keystrokes(deck::app::RESTART_KEY);
    vcx.run_until_parked();
    let restarted = daemon.task(&original.id).unwrap();
    assert_eq!(restarted.state, State::Running);
    assert_ne!(restarted.pid, original.pid, "the shortcut stopped the old agent and started a replacement");
    assert_eq!(deck.read_with(&vcx, |d, _| d.task_id().map(str::to_string)), Some(original.id.clone()));

    // A stopped task remains restartable after its connection reports failure.
    daemon.stop(&original.id).unwrap();
    deck.update(&mut vcx, |d, cx| d.on_inbound(Inbound::Failed("the connection closed".into()), cx));
    assert!(deck.read_with(&vcx, |d, _| d.can_restart()));
    vcx.simulate_keystrokes(deck::app::RESTART_KEY);
    vcx.run_until_parked();
    let retried = daemon.task(&original.id).unwrap();
    assert_eq!(retried.state, State::Running);
    assert_ne!(retried.pid, restarted.pid, "retry resumes a task that already stopped");

    cx.quit();
    daemon.stop(&original.id).unwrap();
    drop(daemon);
    cleanup_daemon(&data);
}

#[cfg(unix)]
fn cleanup_daemon(data: &std::path::Path) {
    let paths = deck_runtime::paths::Paths::for_data(data).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while paths.control().exists() {
        assert!(std::time::Instant::now() < deadline, "test daemon did not exit after detaching");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = std::fs::remove_dir_all(data);
    let _ = std::fs::remove_dir_all(paths.directory);
}

#[cfg(unix)]
#[gpui::test]
fn a_boundary_approval_from_the_daemon_is_answered_by_key(cx: &mut TestAppContext) {
    use deck_runtime::approvals::{Ask, Scope, State as ApprovalState};
    use deck_runtime::{DaemonClient, State};
    // The watch connection and the held request wake the UI from real threads.
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-boundary-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, std::env::temp_dir(), cx)))
            .unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    let id = deck.read_with(&vcx, |d, _| {
        d.task_id().map(str::to_owned).unwrap_or_else(|| panic!("the window runs no task: {:?}", d.session().status))
    });

    let watcher = DaemonClient::connect_or_start(&data).unwrap();
    let holder = DaemonClient::connect(&data).unwrap();
    let ask = Ask {
        gateway: "model".into(),
        operation: "chat_completions".into(),
        destination: "other".into(),
        credential: Some("OPENAI_API_KEY".into()),
        details: Default::default(),
        reason: None,
    };
    let asked = id.clone();
    let approved = std::thread::spawn(move || holder.request_approval(&asked, ask));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        vcx.run_until_parked();
        let shown = deck.read_with(&vcx, |d, _| d.boundary().pending().is_some());
        if shown {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the window never saw the approval");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    vcx.simulate_keystrokes("2");
    vcx.run_until_parked();
    let outcome = approved.join().unwrap().unwrap();
    assert_eq!(outcome.state, ApprovalState::Allowed(Scope::Task));
    deck.read_with(&vcx, |d, _| assert!(d.boundary().pending().is_none(), "the decision closes the card"));
    assert_eq!(watcher.grants(&id).unwrap().len(), 1);

    cx.quit();
    watcher.stop(&id).unwrap();
    assert!(matches!(watcher.task(&id).unwrap().state, State::Stopped { .. }));
    drop(watcher);
    cleanup_daemon(&data);
}

#[cfg(unix)]
#[gpui::test]
fn a_late_boundary_decision_reaches_an_agent_that_advertised_it(cx: &mut TestAppContext) {
    use deck_runtime::approvals::{Ask, Scope, State as ApprovalState};
    use deck_runtime::{DaemonClient, State};
    // The watch connection and the held request wake the UI from real threads.
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-boundary-note-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let fake = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("deck-fake-agent");
    assert!(fake.is_file(), "build the fake agent beside deck: {}", fake.display());
    // `env` gives the agent a boundary endpoint, so it advertises the
    // capability and accepts the notification.
    let argv = vec!["env".to_string(), "DECK_BOUNDARY_URL=http://127.0.0.1:9".to_string(), fake.display().to_string()];
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, std::env::temp_dir(), cx)))
            .unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    let ready = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let id = loop {
        vcx.run_until_parked();
        let id = deck.read_with(&vcx, |d, _| {
            (d.session().status == Status::Ready).then(|| d.task_id().map(str::to_owned)).flatten()
        });
        if let Some(id) = id {
            break id;
        }
        assert!(std::time::Instant::now() < ready, "the fake agent never became ready");
        std::thread::sleep(std::time::Duration::from_millis(10));
    };

    let watcher = DaemonClient::connect_or_start(&data).unwrap();
    let holder = DaemonClient::connect(&data).unwrap();
    let ask = Ask {
        gateway: "model".into(),
        operation: "chat_completions".into(),
        destination: "other".into(),
        credential: Some("OPENAI_API_KEY".into()),
        details: Default::default(),
        reason: None,
    };
    let asked = id.clone();
    let approved = std::thread::spawn(move || holder.request_approval(&asked, ask));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        vcx.run_until_parked();
        if deck.read_with(&vcx, |d, _| d.boundary().pending().is_some()) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the window never saw the approval");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    vcx.simulate_keystrokes("2");
    let outcome = approved.join().unwrap().unwrap();
    assert_eq!(outcome.state, ApprovalState::Allowed(Scope::Task));

    // The agent was not waiting, so Deck sends the reverse-DNS boundary
    // notification; the fake agent puts the same note in its transcript.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        vcx.run_until_parked();
        let noted = deck.read_with(&vcx, |d, _| {
            d.session()
                .entries
                .iter()
                .any(|entry| matches!(entry, Entry::Agent(text) if text.contains("[deck boundary] other") && text.contains("approval")))
        });
        if noted {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the agent never received the boundary note");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    cx.quit();
    watcher.stop(&id).unwrap();
    assert!(matches!(watcher.task(&id).unwrap().state, State::Stopped { .. }));
    drop(watcher);
    cleanup_daemon(&data);
}
