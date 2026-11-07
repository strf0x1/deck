//! The Deck view under GPUI's headless test platform: real layout, real
//! input dispatch, no window server. This is how an agent proves a UI change
//! without looking at the screen.
#![expect(clippy::unwrap_used, reason = "setup helpers outside #[test] fns; a failure is a test failure")]

use deck::ui::board::PITCH;
use deck::ui::root::Deck;
use deck::ui::session::{Entry, Status, Timing};
use deck_acp::{Inbound, PermissionOption};
use deck_runtime::approvals::{Approval, Details, Summary};
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
fn the_composer_edits_across_lines_and_recalls_earlier_prompts(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    deck.update(&mut cx, |d, cx| {
        d.on_inbound(Inbound::Ready { agent: "construct".into(), session_id: "s".into(), turn: None }, cx)
    });
    cx.simulate_keystrokes("f i x shift-enter t e s t s");
    assert_eq!(deck.read_with(&cx, |d, _| d.draft().to_string()), "fix\ntests");
    // Up moves to the row above, keeping the column; left and backspace edit there.
    cx.simulate_keystrokes("up backspace X");
    assert_eq!(deck.read_with(&cx, |d, _| d.draft().to_string()), "fiX\ntests");
    cx.simulate_keystrokes("ctrl-a n e w");
    assert_eq!(deck.read_with(&cx, |d, _| d.draft().to_string()), "new", "typing replaces a selection");
    cx.simulate_keystrokes("enter");
    deck.update(&mut cx, |d, cx| d.on_inbound(Inbound::TurnEnded("end_turn".into()), cx));
    cx.simulate_keystrokes("d r a f t up");
    assert_eq!(deck.read_with(&cx, |d, _| d.draft().to_string()), "new", "up on the first row recalls");
    cx.simulate_keystrokes("down");
    assert_eq!(deck.read_with(&cx, |d, _| d.draft().to_string()), "draft", "down returns to the draft");
}

#[gpui::test]
fn a_long_draft_wraps_and_a_click_places_the_caret(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    cx.run_until_parked();
    let one_row = cx.debug_bounds("composer").unwrap();
    cx.simulate_input(&"word ".repeat(60));
    cx.run_until_parked();
    let wrapped = cx.debug_bounds("composer").unwrap();
    assert!(wrapped.size.height > one_row.size.height, "the field grows: {one_row:?} -> {wrapped:?}");
    // The text starts below the field's 15px top padding, at its left edge.
    cx.simulate_click(gpui::point(wrapped.left() + px(1.), wrapped.top() + px(20.)), Modifiers::none());
    assert_eq!(deck.read_with(&cx, |d, _| d.draft_selection()), 0..0);
    cx.simulate_keystrokes("shift-right shift-right");
    assert_eq!(deck.read_with(&cx, |d, _| d.draft_selection()), 0..2);
    cx.simulate_input("s");
    deck.read_with(&cx, |d, _| assert!(d.draft().starts_with("srd word"), "{}", d.draft()));
}

#[gpui::test]
fn tool_cards_expand_and_the_transcript_follows_until_scrolled_back(cx: &mut TestAppContext) {
    use deck_acp::{ToolCall, ToolContent, ToolKind, ToolStatus};
    use gpui::{ScrollDelta, ScrollWheelEvent, point};

    let (deck, mut cx) = open(cx);
    deck.update(&mut cx, |d, cx| {
        d.on_inbound(Inbound::Ready { agent: "construct".into(), session_id: "s".into(), turn: None }, cx);
        for i in 0..30 {
            d.on_inbound(Inbound::UserMessage(format!("question {i}")), cx);
            d.on_inbound(Inbound::Message(format!("answer **{i}**\n\n- one\n- two")), cx);
        }
        let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 3\n", "line three\n").replace("line 30\n", "line thirty\n");
        let edit = ToolCall {
            id: "e1".into(),
            title: "edit src/lib.rs".into(),
            kind: ToolKind::Edit,
            status: ToolStatus::Done,
            content: vec![ToolContent::Diff { path: "src/lib.rs".into(), old: Some(old), new }],
            ..ToolCall::default()
        };
        d.on_inbound(Inbound::Tool(edit), cx);
    });
    cx.run_until_parked();
    assert!(!deck.read_with(&cx, |d, _| d.scrolled_back()), "new output is followed");

    click_control(&mut cx, "card-tool:e1");
    assert!(deck.read_with(&cx, |d, _| d.is_expanded("tool:e1")), "the header opens the card");

    let pane = cx.debug_bounds("transcript").unwrap();
    cx.simulate_event(ScrollWheelEvent {
        position: pane.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(600.))),
        ..ScrollWheelEvent::default()
    });
    cx.run_until_parked();
    assert!(deck.read_with(&cx, |d, _| d.scrolled_back()), "scrolling up stops following");
    settle(&deck, &mut cx);
    click_control(&mut cx, "jump-latest");
    assert!(!deck.read_with(&cx, |d, _| d.scrolled_back()), "the pill returns to the end");
}

#[gpui::test]
fn a_finished_turn_lists_its_files_and_copy_buttons_copy(cx: &mut TestAppContext) {
    use deck_acp::{ToolCall, ToolContent, ToolKind, ToolStatus};

    let (deck, mut cx) = open(cx);
    let edit = |id: &str, path: &str| ToolCall {
        id: id.into(),
        title: format!("edit {path}"),
        kind: ToolKind::Edit,
        status: ToolStatus::Done,
        content: vec![ToolContent::Diff { path: path.into(), old: Some("a\n".into()), new: "b\n".into() }],
        ..ToolCall::default()
    };
    let run = ToolCall {
        id: "x1".into(),
        title: "shell cargo test".into(),
        kind: ToolKind::Execute,
        status: ToolStatus::Done,
        input: Some(json!({ "command": "cargo test" })),
        ..ToolCall::default()
    };
    deck.update(&mut cx, |d, cx| {
        d.on_inbound(Inbound::Ready { agent: "construct".into(), session_id: "s".into(), turn: None }, cx);
        d.on_inbound(Inbound::UserMessage("rename it".into()), cx);
        d.on_inbound(Inbound::Tool(edit("e1", "src/lib.rs")), cx);
        d.on_inbound(Inbound::Tool(edit("e2", "src/main.rs")), cx);
        d.on_inbound(Inbound::Tool(run), cx);
        d.on_inbound(Inbound::Message("Renamed.".into()), cx);
        d.on_inbound(Inbound::TurnEnded("end_turn".into()), cx);
    });
    click_control(&mut cx, "touched-main.rs");
    assert!(deck.read_with(&cx, |d, _| d.is_expanded("tool:e2")), "a footer chip opens its edit card");
    assert!(!deck.read_with(&cx, |d, _| d.is_expanded("tool:e1")));

    // The header's copy button copies the command and does not fold the card.
    let copy = cx.debug_bounds("copy-tool:x1:subject").unwrap();
    cx.simulate_mouse_move(copy.center(), None, Modifiers::none());
    click_control(&mut cx, "copy-tool:x1:subject");
    assert_eq!(cx.read_from_clipboard().and_then(|c| c.text()).as_deref(), Some("cargo test"));
    deck.read_with(&cx, |d, _| {
        assert_eq!(d.copied(), Some("tool:x1:subject"), "the button shows its check");
        assert!(!d.is_expanded("tool:x1"), "copying is not a click on the header");
    });
    cx.executor().advance_clock(std::time::Duration::from_secs(2));
    cx.run_until_parked();
    assert_eq!(deck.read_with(&cx, |d, _| d.copied().map(str::to_owned)), None, "the check goes away");
}

#[gpui::test]
fn cards_are_selected_and_opened_from_the_keyboard(cx: &mut TestAppContext) {
    use deck_acp::{ToolCall, ToolKind, ToolStatus};

    let (deck, mut cx) = open(cx);
    let call = |id: &str, kind| ToolCall {
        id: id.into(),
        title: id.into(),
        kind,
        status: ToolStatus::Done,
        ..ToolCall::default()
    };
    deck.update(&mut cx, |d, cx| {
        d.on_inbound(Inbound::Ready { agent: "construct".into(), session_id: "s".into(), turn: None }, cx);
        d.on_inbound(Inbound::UserMessage("go".into()), cx);
        d.on_inbound(Inbound::Tool(call("x1", ToolKind::Execute)), cx);
        d.on_inbound(Inbound::Message("between".into()), cx);
        d.on_inbound(Inbound::Tool(call("x2", ToolKind::Execute)), cx);
    });
    cx.simulate_keystrokes("d r a f t alt-up");
    assert_eq!(deck.read_with(&cx, |d, _| d.selected_card().map(str::to_owned)), Some("tool:x2".into()));
    cx.simulate_keystrokes("alt-up tab");
    deck.read_with(&cx, |d, _| {
        assert_eq!(d.selected_card(), Some("tool:x1"), "alt-up steps back over prose");
        assert!(d.is_expanded("tool:x1"), "tab opens the selected card");
        assert_eq!(d.draft(), "draft", "card keys never reach the composer");
    });
    cx.simulate_keystrokes("escape");
    assert_eq!(deck.read_with(&cx, |d, _| d.selected_card().map(str::to_owned)), None, "escape lets go first");
    settle(&deck, &mut cx);
}

#[gpui::test]
fn loading_construct_history_preserves_terminal_tool_states(cx: &mut TestAppContext) {
    use deck_acp::{ToolKind, ToolStatus, conn::Conn};

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
        let order: Vec<String> = d
            .session()
            .entries
            .iter()
            .map(|e| match e {
                Entry::You(t) => format!("you: {t}"),
                Entry::Agent(t) => format!("agent: {t}"),
                Entry::Tool(t) => format!("tool: {}", t.call.id),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(order, ["you: read files", "tool: call_1", "tool: call_2", "agent: done"]);
        let tools: Vec<_> = d
            .session()
            .entries
            .iter()
            .filter_map(|e| match e {
                Entry::Tool(t) => Some((t.call.title.as_str(), t.call.kind, t.call.status, t.timing)),
                _ => None,
            })
            .collect();
        assert_eq!(
            tools,
            vec![
                ("read note.txt", ToolKind::Read, ToolStatus::Done, Timing::Unknown),
                ("read missing.txt", ToolKind::Read, ToolStatus::Failed, Timing::Unknown),
            ],
            "replayed calls keep their kind and final state, with no duration"
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
        details: Details {
            method: Some("POST".into()),
            path: Some("/v1/chat/completions".into()),
            size: Some(0),
            identity: None,
            summary: None,
        },
        reason: None,
        created_ms: 0,
        expires_ms: 0,
        grant_expires_ms: None,
        policy: 0,
        state: deck_runtime::approvals::State::Pending,
        waiters: 0,
    }
}

fn github_approval(id: &str, operation: &str, destination: &str, reason: &str, summary: Option<Summary>) -> Approval {
    Approval {
        id: id.into(),
        task: "t".into(),
        project: "/w".into(),
        session: None,
        gateway: "github".into(),
        operation: operation.into(),
        destination: destination.into(),
        credential: Some("DECK_GITHUB_TOKEN".into()),
        details: Details { summary, ..Default::default() },
        reason: Some(reason.into()),
        created_ms: 0,
        expires_ms: 0,
        grant_expires_ms: None,
        policy: 0,
        state: deck_runtime::approvals::State::Pending,
        waiters: 1,
    }
}

#[gpui::test]
fn a_ready_for_review_card_renders_without_a_publish_summary(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    deck.update(&mut cx, |d, cx| {
        d.on_boundary(github_approval("ready", "update_pr", "owner/name#pr7", "update pull request 7", None), cx)
    });
    deck.read_with(&cx, |d, _| {
        let pending = d.boundary().pending().expect("the ready-for-review card is shown");
        assert_eq!(pending.operation, "update_pr");
        assert_eq!(pending.destination, "owner/name#pr7");
        assert!(pending.details.summary.is_none(), "an update_pr card carries no publish summary");
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("boundary-summary").is_none(), "there is nothing to label as the agent's description");
}

#[gpui::test]
fn a_public_repo_publish_card_shows_the_agents_description(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    deck.update(&mut cx, |d, cx| {
        d.on_boundary(
            github_approval(
                "public",
                "publish",
                "owner/name#deck/t-1",
                "publish a commit to deck/t-1",
                Some(Summary { subject: "Add the gateway".into(), count: 1 }),
            ),
            cx,
        )
    });
    deck.read_with(&cx, |d, _| {
        let pending = d.boundary().pending().expect("the public-repo card is shown");
        assert_eq!(pending.operation, "publish");
        assert_eq!(pending.destination, "owner/name#deck/t-1");
        assert_eq!(pending.details.summary.as_ref().map(|summary| summary.count), Some(1));
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("boundary-summary").is_some(), "the agent's description is under the destination");
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
fn reviewing_external_access_does_not_answer_a_different_agent_permission(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    deck.update(&mut cx, |d, cx| {
        d.on_inbound(
            Inbound::Permission {
                request: json!(19),
                title: "different agent permission".into(),
                options: vec![PermissionOption {
                    id: "allow".into(),
                    name: "Allow once".into(),
                    kind: "allow_once".into(),
                }],
            },
            cx,
        );
        d.on_boundary(approval("reviewed"), cx);
    });
    click_control(&mut cx, "hdr-access");
    click_control(&mut cx, "access-review-0");
    cx.simulate_keystrokes("1");
    deck.read_with(&cx, |d, _| {
        assert!(d.session().ask.is_some(), "the reviewed boundary card owns the decision key");
        assert!(d.boundary().pending().is_some(), "without a daemon the boundary decision stays pending");
    });
    deck.update(&mut cx, |d, cx| {
        let mut decided = approval("reviewed");
        decided.state = deck_runtime::approvals::State::Denied;
        d.on_boundary(decided, cx);
    });
    cx.simulate_keystrokes("1");
    assert!(
        deck.read_with(&cx, |d, _| d.session().ask.is_none()),
        "the agent permission regains its keys after review ends"
    );
}

fn click_control(cx: &mut VisualTestContext, selector: &'static str) {
    cx.run_until_parked();
    let bounds = cx.debug_bounds(selector).unwrap_or_else(|| panic!("missing control {selector}"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

/// Draw frames until no spring moves, as the control socket's `settle` does.
/// Springs step by real time, so this sleeps a frame between draws.
fn settle(deck: &gpui::Entity<Deck>, cx: &mut VisualTestContext) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        if !deck.read_with(cx, |d, _| d.animating()) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "the springs did not settle");
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

fn wait_view(cx: &mut VisualTestContext, mut ready: impl FnMut(&VisualTestContext) -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        cx.run_until_parked();
        if ready(cx) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "the window did not receive its daemon reply");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[gpui::test]
fn credential_retry_instructions_fit_the_access_panel_at_supported_widths(cx: &mut TestAppContext) {
    use deck_runtime::broker::Listed;
    use gpui::size;

    let (deck, mut cx) = open(cx);
    click_control(&mut cx, "hdr-access");
    for width in [1180., 760.] {
        cx.simulate_resize(size(px(width), px(760.)));
        for name in ["OPENAI_API_KEY".to_string(), format!("KEY_{}", "A".repeat(60))] {
            let message = format!(
                "OS access was cancelled or refused; automatic reads are paused. Run `deckd credential retry {name}` to try again"
            );
            deck.update(&mut cx, |d, cx| {
                d.on_credentials(
                    0,
                    Ok(vec![
                        Listed { name, fingerprint: None, revoked: false, error: Some(message) },
                        Listed {
                            name: "DECK_GITHUB_TOKEN".into(),
                            fingerprint: Some("sha256:123456789abc".into()),
                            revoked: false,
                            error: None,
                        },
                    ]),
                    cx,
                );
            });
            cx.run_until_parked();
            let row = cx.debug_bounds("access-credential-row-0").unwrap();
            let name = cx.debug_bounds("access-credential-name-0").unwrap();
            let detail = cx.debug_bounds("access-credential-detail-0").unwrap();
            let status = cx.debug_bounds("access-credential-status-0").unwrap();
            assert!(
                detail.left() >= row.left() && detail.right() <= row.right(),
                "retry text must fit horizontally at {width}: {detail:?}, {row:?}"
            );
            assert!(detail.top() >= name.bottom(), "the recovery instruction needs its own space below the name");
            assert!(detail.size.height > px(20.), "the complete instruction must wrap onto multiple lines");
            assert!(detail.bottom() <= row.bottom(), "the row must grow to contain the complete instruction");
            assert!(
                status.left() >= row.left() && status.right() <= row.right(),
                "credential status must stay visible"
            );
            let active = cx.debug_bounds("access-credential-row-1").unwrap();
            let revoke = cx.debug_bounds("access-revoke-1").unwrap();
            assert!(
                revoke.left() >= active.left() && revoke.right() <= active.right(),
                "the active credential's action must stay visible"
            );
        }
    }
    click_control(&mut cx, "access-revoke-1");
    assert_eq!(
        deck.read_with(&cx, |d, _| d.access().confirming().map(|target| target.label())),
        Some("DECK_GITHUB_TOKEN".into())
    );
}

#[gpui::test]
fn access_grants_are_revoked_by_real_clicks_and_pending_requests_return_to_the_card(cx: &mut TestAppContext) {
    use deck::ui::access::Revoke;
    use deck_runtime::approvals::{Ask, Decision, Scope, State as ApprovalState};
    use deck_runtime::{DaemonClient, Spec};
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-access-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let task = daemon.run(Spec { argv: argv.clone(), cwd: data.clone(), github: None }).unwrap().task;
    let ask = Ask {
        gateway: "web".into(),
        operation: "connect".into(),
        destination: "example.com:443".into(),
        credential: None,
        details: Default::default(),
        reason: None,
        coverage: None,
    };
    let item = daemon.request_approval(&task.id, ask.clone()).unwrap();
    daemon.decide(&item.id, Decision::Task).unwrap();
    daemon.web_profile(&task.id, None, "custom", &["saved.example:443".into()], false).unwrap();
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, data.clone(), cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    click_control(&mut vcx, "hdr-access");
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.access().grants().len() == 2));
    click_control(&mut vcx, "access-grant-revoke-0");
    deck.read_with(&vcx, |d, _| {
        assert!(matches!(d.access().confirming(), Some(Revoke::Grant(grant)) if grant.scope == Scope::Task))
    });
    vcx.simulate_keystrokes("escape");
    assert_eq!(daemon.grants(&task.id).unwrap().len(), 1, "cancelling changes nothing");
    click_control(&mut vcx, "access-grant-revoke-0");
    vcx.simulate_keystrokes("h i enter 2");
    assert!(deck.read_with(&vcx, |d, _| d.draft().is_empty()), "Access never types into the hidden prompt");
    click_control(&mut vcx, "access-revoke-confirm");
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| !d.access().busy() && d.access().grants().len() == 1));
    assert!(daemon.grants(&task.id).unwrap().is_empty());
    click_control(&mut vcx, "access-grant-revoke-0");
    deck.read_with(&vcx, |d, _| {
        assert!(matches!(d.access().confirming(), Some(Revoke::Grant(grant)) if grant.scope == Scope::Project))
    });
    click_control(&mut vcx, "access-revoke-confirm");
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| !d.access().busy() && d.access().grants().is_empty()));
    assert!(daemon.access_grants(&task.id).unwrap().is_empty());

    let holder = DaemonClient::connect(&data).unwrap();
    let id = task.id.clone();
    let asked = std::thread::spawn(move || holder.request_approval(&id, ask));
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.boundary().pending().is_some()));
    click_control(&mut vcx, "access-review-0");
    assert!(!deck.read_with(&vcx, |d, _| d.access().is_open()));
    vcx.simulate_keystrokes("5");
    assert_eq!(asked.join().unwrap().unwrap().state, ApprovalState::Denied);
    cx.quit();
    daemon.stop(&task.id).unwrap();
    drop(daemon);
    cleanup_daemon(&data);
}

#[gpui::test]
fn the_access_panel_shows_the_tasks_github_approval_mode(cx: &mut TestAppContext) {
    use deck_runtime::github::{Approvals, Identity, Policy};
    use deck_runtime::{DaemonClient, Spec};
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-mode-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let task = daemon
        .run(Spec {
            argv: argv.clone(),
            cwd: data.clone(),
            github: Some(Policy {
                repository: "owner/repo".into(),
                base: "main".into(),
                branches: vec![],
                credential: "DECK_GITHUB_TOKEN".into(),
                approvals: Approvals::Ask,
                identity: Identity::default(),
            }),
        })
        .unwrap()
        .task;
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, data.clone(), cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    click_control(&mut vcx, "hdr-access");
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.access().approvals() == Some(Approvals::Ask)));
    assert!(vcx.debug_bounds("access-approvals").is_some(), "the panel shows the mode");
    cx.quit();
    daemon.stop(&task.id).unwrap();
    drop(daemon);
    cleanup_daemon(&data);
}

/// A task record as `deckd` lists it.
fn task(id: &str, state: serde_json::Value, publication: Option<serde_json::Value>) -> deck_runtime::Task {
    serde_json::from_value(json!({
        "id": id, "backend": "vm", "spec": { "argv": ["construct", "acp"], "cwd": format!("/src/{id}") },
        "state": state, "pid": null, "publication": publication,
    }))
    .unwrap()
}

/// Three more tasks on the board, as a daemon refresh would show them.
fn seed(deck: &gpui::Entity<Deck>, cx: &mut VisualTestContext) {
    let tasks = ["a", "b", "c"].map(|id| task(id, json!({ "state": "running" }), None)).to_vec();
    deck.update(cx, |d, cx| d.on_tasks(tasks, Default::default(), cx));
}

#[gpui::test]
fn dragging_a_card_with_the_mouse_reorders_lanes(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    seed(&deck, &mut cx);
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

    // The first icon (Sessions), inside the closed rail.
    let (x, y) = Deck::rail_item_center(0);
    let sessions = point(px(x), px(y));
    cx.simulate_mouse_move(sessions, None, Modifiers::none());
    assert!(deck.read_with(&cx, |d, _| d.rail().expanded()), "hovering the rail opens it");
    cx.simulate_click(sessions, Modifiers::none());
    assert_eq!(deck.read_with(&cx, |d, _| d.rail().active()), 0);

    cx.simulate_mouse_move(point(px(700.), px(300.)), None, Modifiers::none());
    assert!(!deck.read_with(&cx, |d, _| d.rail().expanded()), "leaving the rail closes it");
}

#[gpui::test]
fn the_rail_shows_only_sessions(cx: &mut TestAppContext) {
    let (_deck, mut cx) = open(cx);
    cx.run_until_parked();
    assert!(cx.debug_bounds("rail-item-0").is_some(), "the rail renders Sessions");
    assert!(cx.debug_bounds("rail-item-1").is_none(), "the rail has no second destination");
    assert!(cx.debug_bounds("rail-settings").is_none(), "the rail has no Settings item");
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
    let original = daemon.run(Spec { argv: argv.clone(), cwd: cwd.clone(), github: None }).unwrap().task;
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
#[gpui::test]
fn stopping_a_task_from_the_header_and_restarting_it(cx: &mut TestAppContext) {
    use deck_runtime::{DaemonClient, Spec, State};
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-stop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let cwd = std::env::temp_dir();
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    let original = daemon.run(Spec { argv: argv.clone(), cwd: cwd.clone(), github: None }).unwrap().task;
    cx.update(deck::app::bind);
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, cwd, cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    vcx.run_until_parked();
    assert!(deck.read_with(&vcx, |d, _| d.can_stop()), "the detached task is stoppable");
    let stop = vcx.debug_bounds("hdr-stop").expect("the stop button is shown").center();
    vcx.simulate_click(stop, Modifiers::none());
    vcx.run_until_parked();
    assert!(matches!(daemon.task(&original.id).unwrap().state, State::Stopped { .. }));
    deck.read_with(&vcx, |d, _| {
        assert_eq!(d.session().status, Status::Down("stopped".into()));
        assert!(d.can_restart());
    });
    assert!(vcx.debug_bounds("restart-task").is_some(), "the stopped task shows its restart card");
    vcx.simulate_keystrokes(deck::app::RESTART_KEY);
    vcx.run_until_parked();
    assert_eq!(daemon.task(&original.id).unwrap().state, State::Running);
    cx.quit();
    daemon.stop(&original.id).unwrap();
    drop(daemon);
    cleanup_daemon(&data);
}

#[cfg(unix)]
#[gpui::test]
fn the_stop_binding_stops_the_task(cx: &mut TestAppContext) {
    use deck_runtime::{DaemonClient, Spec, State};
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-stop-key-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let cwd = std::env::temp_dir();
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    let original = daemon.run(Spec { argv: argv.clone(), cwd: cwd.clone(), github: None }).unwrap().task;
    cx.update(deck::app::bind);
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, cwd, cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    vcx.run_until_parked();
    vcx.simulate_keystrokes(deck::app::STOP_KEY);
    vcx.run_until_parked();
    assert!(matches!(daemon.task(&original.id).unwrap().state, State::Stopped { .. }));
    deck.read_with(&vcx, |d, _| {
        assert_eq!(d.session().status, Status::Down("stopped".into()));
        assert!(d.can_restart());
    });
    cx.quit();
    daemon.destroy(&original.id).unwrap();
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
        coverage: None,
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
    vcx.simulate_keystrokes("3");
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
        coverage: None,
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
    vcx.simulate_keystrokes("3");
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

#[gpui::test]
fn the_sessions_column_shows_each_task_and_its_pull_request_chip(cx: &mut TestAppContext) {
    use deck::ui::publication::Dot;
    use deck_runtime::pull_request::{Live, State as Published};
    let (deck, mut cx) = open(cx);
    assert_eq!(deck.read_with(&cx, |d, _| d.board().order()), vec![0], "only the live card before a task list");
    let pull = json!({ "kind": "pull_request", "number": 7, "url": "https://github.com/o/r/pull/7", "title": "Add b", "draft": true });
    let tasks = vec![
        task("a", json!({ "state": "running" }), Some(pull.clone())),
        task("b", json!({ "state": "stopped", "clean": true, "reason": "" }), None),
        task("gone", json!({ "state": "destroyed" }), None),
    ];
    let checks = json!({ "state": "open", "draft": false, "merged": false, "head": "deck/t-a", "head_commit": "c".repeat(40),
        "checks": { "total": 1, "runs": [{ "name": "ci", "status": "in_progress", "conclusion": null }] } });
    let published: std::collections::HashMap<String, Published> = [(
        "a".to_string(),
        Published {
            publication: Some(serde_json::from_value(pull).unwrap()),
            live: Some(Live { read_at_ms: 1, pull_request: Some(checks), ..Live::default() }),
            fresh: true,
        },
    )]
    .into();
    deck.update(&mut cx, |d, cx| d.on_tasks(tasks, published, cx));
    cx.run_until_parked();
    let (order, lanes) = deck.read_with(&cx, |d, _| {
        let cards = d.board().cards();
        (
            d.board().order(),
            cards
                .iter()
                .map(|c| (c.lane.title.clone(), c.lane.detail.clone(), c.lane.chip.clone()))
                .collect::<Vec<_>>(),
        )
    });
    assert_eq!(order.len(), 3, "the live card plus a and b; a destroyed task has none");
    assert_eq!(lanes[1], ("a".into(), "vm".into(), Some((Dot::ChecksRunning, "#7".into()))));
    assert_eq!(lanes[2], ("b".into(), "stopped · vm".into(), None));

    // The chip opens the panel without picking the card up.
    let lane = order[1];
    click_control(&mut cx, Box::leak(format!("card-chip-{lane}").into_boxed_str()));
    assert_eq!(deck.read_with(&cx, |d, _| d.board().dragging()), None);
    assert_eq!(deck.read_with(&cx, |d, _| d.open_publication().map(|(t, _)| t.to_string())), Some("a".into()));
    assert!(cx.debug_bounds("publication-panel").is_some());
    assert!(cx.debug_bounds("publication-destroy").is_none(), "nothing to destroy before a merge");
    cx.simulate_keystrokes("escape");
    assert!(cx.debug_bounds("publication-panel").is_none());
}

#[gpui::test]
fn a_merged_pull_request_marks_the_task_done_and_offers_destroy_behind_a_second_click(cx: &mut TestAppContext) {
    use deck_runtime::pull_request::{Live, State as Published};
    let (deck, mut cx) = open(cx);
    let pull = json!({ "kind": "pull_request", "number": 9, "url": "https://github.com/o/r/pull/9", "title": "t", "draft": false });
    let merged = Published {
        publication: Some(serde_json::from_value(pull.clone()).unwrap()),
        live: Some(Live {
            read_at_ms: 1,
            pull_request: Some(json!({ "state": "closed", "merged": true })),
            ..Live::default()
        }),
        fresh: false,
    };
    deck.update(&mut cx, |d, cx| {
        d.on_tasks(vec![task("a", json!({ "state": "running" }), Some(pull))], [("a".to_string(), merged)].into(), cx)
    });
    cx.run_until_parked();
    let (lane, detail) = deck.read_with(&cx, |d, _| (d.board().order()[1], d.board().cards()[1].lane.detail.clone()));
    assert_eq!(detail, "done · merged");
    click_control(&mut cx, Box::leak(format!("card-chip-{lane}").into_boxed_str()));
    click_control(&mut cx, "publication-destroy");
    assert!(deck.read_with(&cx, |d, _| d.open_publication().is_some()), "the first click only asks");
}

#[gpui::test]
fn the_window_shows_its_own_tasks_publication_from_the_daemon(cx: &mut TestAppContext) {
    use deck_runtime::{DaemonClient, Publication, Spec};
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-pub-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let task = daemon.run(Spec { argv: argv.clone(), cwd: data.clone(), github: None }).unwrap().task;
    let pull = Publication::PullRequest {
        number: 7,
        url: "https://github.com/o/r/pull/7".into(),
        title: "t".into(),
        draft: true,
    };
    daemon.publication(&task.id, pull).unwrap();
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, data.clone(), cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.publication_of(deck::ui::root::LIVE).is_some()));
    click_control(&mut vcx, "hdr-publication");
    assert_eq!(deck.read_with(&vcx, |d, _| d.open_publication().map(|(t, _)| t.to_string())), Some(task.id.clone()));
    assert!(deck.read_with(&vcx, |d, _| d.publish_notice().is_none()), "a local task has no gateway to warn about");
    cx.quit();
    daemon.stop(&task.id).unwrap();
    drop(daemon);
    cleanup_daemon(&data);
}

#[gpui::test]
fn a_merged_own_task_reads_done_in_the_sessions_column(cx: &mut TestAppContext) {
    use deck::ui::board::LaneState;
    use deck_runtime::pull_request::{Live, State as Published};
    use deck_runtime::{DaemonClient, Spec};
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-own-merged-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let task = daemon.run(Spec { argv: argv.clone(), cwd: data.clone(), github: None }).unwrap().task;
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, data.clone(), cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.task_id() == Some(task.id.as_str())));
    let pull = json!({ "kind": "pull_request", "number": 9, "url": "https://github.com/o/r/pull/9", "title": "t", "draft": false });
    let merged = Published {
        publication: Some(serde_json::from_value(pull).unwrap()),
        live: Some(Live {
            read_at_ms: 1,
            pull_request: Some(json!({ "state": "closed", "merged": true })),
            ..Live::default()
        }),
        fresh: false,
    };
    let record = daemon.task(&task.id).unwrap();
    deck.update(&mut vcx, |d, cx| d.on_tasks(vec![record], [(task.id.clone(), merged)].into(), cx));
    let (state, detail) = deck.read_with(&vcx, |d, _| {
        let card = d.board().cards().iter().find(|c| c.lane.id == deck::ui::root::LIVE).unwrap();
        (card.lane.state, card.lane.detail.clone())
    });
    assert_eq!((state, detail.as_str()), (LaneState::Review, "done · merged"));
    cx.quit();
    daemon.stop(&task.id).unwrap();
    drop(daemon);
    cleanup_daemon(&data);
}

#[cfg(unix)]
#[gpui::test]
fn new_session_starts_another_task_beside_the_running_one(cx: &mut TestAppContext) {
    use deck_runtime::{DaemonClient, State};
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-new-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let fake = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("deck-fake-agent");
    assert!(fake.is_file(), "build the fake agent beside deck: {}", fake.display());
    let cwd = data.join("alpha");
    std::fs::create_dir_all(&cwd).unwrap();
    let journal = format!("DECK_FAKE_STATE={}", data.join("journal").display());
    let argv = ["env", "DECK_FAKE_DELAY_MS=1", &journal, &fake.display().to_string()].map(str::to_string).to_vec();
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, cwd.clone(), cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    let own = ready_on(&deck, &mut vcx, None);
    prompt_fake(&deck, &mut vcx, "first");

    // The header button: a second task with the same command and directory,
    // which `run` alone would never create.
    let button = vcx.debug_bounds("hdr-new").unwrap().center();
    vcx.simulate_click(button, Modifiers::none());
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.task_id() != Some(own.as_str())));
    let second = ready_on(&deck, &mut vcx, None);
    assert_ne!(second, own);
    assert!(said(&deck, &vcx).is_empty(), "a new session starts with an empty transcript");
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    let record = daemon.task(&second).unwrap();
    assert_eq!((record.spec.cwd.as_path(), record.state.clone()), (cwd.as_path(), State::Running));
    assert_eq!(daemon.task(&own).unwrap().state, State::Running, "the previous session keeps running");
    deck.read_with(&vcx, |d, _| {
        assert_eq!(d.board().order().len(), 2, "the previous session is an ordinary card");
        assert!(!d.new_session_state().is_creating());
    });
    prompt_fake(&deck, &mut vcx, "second");

    // The column's own button does the same; then a card switches back.
    let plus = vcx.debug_bounds("lanes-new").unwrap().center();
    vcx.simulate_click(plus, Modifiers::none());
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.task_id() != Some(second.as_str())));
    let third = ready_on(&deck, &mut vcx, None);
    assert!(third != own && third != second);
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.board().order().len() == 3));
    vcx.simulate_click(point(px(200.), px(150. + PITCH)), Modifiers::none());
    let back = ready_on(&deck, &mut vcx, None);
    assert!(back == own || back == second, "the card held one of the earlier sessions");
    let expect = if back == own { "first" } else { "second" };
    assert_eq!(said(&deck, &vcx), vec![expect.to_string()], "its conversation is rebuilt by live attach");

    cx.quit();
    for id in [&own, &second, &third] {
        daemon.stop(id).unwrap();
    }
    drop(daemon);
    cleanup_daemon(&data);
}

/// Wait until the window's session is ready on task `id` (any task when `None`).
#[cfg(unix)]
fn ready_on(deck: &gpui::Entity<Deck>, cx: &mut VisualTestContext, id: Option<&str>) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        let ready = deck.read_with(cx, |d, _| {
            (d.session().status == Status::Ready && id.is_none_or(|id| d.task_id() == Some(id)))
                .then(|| d.task_id().map(str::to_owned))
                .flatten()
        });
        if let Some(ready) = ready {
            return ready;
        }
        let status = deck.read_with(cx, |d, _| format!("{:?} on {:?}", d.session().status, d.task_id()));
        assert!(std::time::Instant::now() < deadline, "the window never became ready on {id:?}: {status}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Send `text` to the fake agent and allow its tool call, until the turn ends.
#[cfg(unix)]
fn prompt_fake(deck: &gpui::Entity<Deck>, cx: &mut VisualTestContext, text: &str) {
    cx.simulate_input(text);
    cx.simulate_keystrokes("enter");
    wait_view(cx, |cx| deck.read_with(cx, |d, _| d.session().ask.is_some()));
    cx.simulate_keystrokes("1");
    let id = deck.read_with(cx, |d, _| d.task_id().map(str::to_owned));
    ready_on(deck, cx, id.as_deref());
}

#[cfg(unix)]
fn said(deck: &gpui::Entity<Deck>, cx: &VisualTestContext) -> Vec<String> {
    deck.read_with(cx, |d, _| {
        d.session().entries.iter().filter_map(|e| if let Entry::You(t) = e { Some(t.clone()) } else { None }).collect()
    })
}

#[cfg(unix)]
#[gpui::test]
fn clicking_another_tasks_card_switches_the_window_to_its_live_session(cx: &mut TestAppContext) {
    use deck_runtime::{DaemonClient, Spec, State};
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-switch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let fake = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("deck-fake-agent");
    assert!(fake.is_file(), "build the fake agent beside deck: {}", fake.display());
    let spec = |name: &str| {
        let cwd = data.join(name);
        std::fs::create_dir_all(&cwd).unwrap();
        let journal = format!("DECK_FAKE_STATE={}", data.join(format!("journal-{name}")).display());
        let argv = ["env", "DECK_FAKE_DELAY_MS=1", &journal, &fake.display().to_string()].map(str::to_string).to_vec();
        Spec { argv, cwd, github: None }
    };
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    // The second task has never run: selecting it starts it.
    let other = daemon.create(spec("beta")).unwrap().id;
    let alpha = spec("alpha");
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, alpha.argv, alpha.cwd, cx)))
            .unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    let own = ready_on(&deck, &mut vcx, None);
    prompt_fake(&deck, &mut vcx, "first");
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.board().order().len() == 2));
    let lane = deck.read_with(&vcx, |d, _| d.board().order()[1]);

    // The second card sits one pitch below the live card.
    let card = point(px(200.), px(150. + PITCH));
    vcx.simulate_click(card, Modifiers::none());
    assert_eq!(deck.read_with(&vcx, |d, _| d.board().dragging()), None, "a click is not a drag");
    ready_on(&deck, &mut vcx, Some(&other));
    deck.read_with(&vcx, |d, _| {
        assert_eq!(d.board().order(), vec![deck::ui::root::LIVE, lane], "the column keeps its shape");
        assert_eq!(d.board().cards()[1].lane.title, "alpha", "the previous task is an ordinary card");
    });
    assert!(said(&deck, &vcx).is_empty(), "the transcript is the selected task's");
    assert_eq!(daemon.task(&own).unwrap().state, State::Running, "switching away only detaches");
    prompt_fake(&deck, &mut vcx, "second");
    assert_eq!(said(&deck, &vcx), vec!["second".to_string()]);

    // Back: the first task's conversation is rebuilt by live attach.
    vcx.simulate_click(card, Modifiers::none());
    ready_on(&deck, &mut vcx, Some(&own));
    assert_eq!(said(&deck, &vcx), vec!["first".to_string()]);
    assert_eq!(deck.read_with(&vcx, |d, _| d.board().cards()[1].lane.title.clone()), "beta");
    vcx.simulate_click(card, Modifiers::none());
    ready_on(&deck, &mut vcx, Some(&other));
    assert_eq!(said(&deck, &vcx), vec!["second".to_string()]);

    cx.quit();
    for id in [&own, &other] {
        daemon.stop(id).unwrap();
    }
    drop(daemon);
    cleanup_daemon(&data);
}
