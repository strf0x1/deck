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

/// Hover the sidebar's first glyph and let its spring settle, so the compact
/// task rows are laid out at full width for a click.
fn open_sidebar(deck: &gpui::Entity<Deck>, cx: &mut VisualTestContext) {
    let (x, y) = deck.read_with(cx, |d, _| d.rail_item_center(0));
    cx.simulate_mouse_move(point(px(x), px(y)), None, Modifiers::none());
    settle(deck, cx);
    assert_eq!(deck.read_with(cx, |d, _| d.rail().width.pos), deck::ui::rail::EXPANDED);
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

/// Draw a frame, so `debug_bounds` sees the current state.
fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

/// A project-view card's bounds; its debug selector is built at run time.
fn card_bounds(cx: &mut VisualTestContext, lane: usize) -> gpui::Bounds<gpui::Pixels> {
    let selector: &'static str = Box::leak(format!("project-view-card-{lane}").into_boxed_str());
    cx.debug_bounds(selector).unwrap_or_else(|| panic!("no project-view card for lane {lane}"))
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
                        Listed { name, fingerprint: None, revoked: false, error: Some(message), paused: true },
                        Listed {
                            name: "DECK_GITHUB_TOKEN".into(),
                            fingerprint: Some("sha256:123456789abc".into()),
                            revoked: false,
                            error: None,
                            paused: false,
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
fn a_paused_credential_offers_retry_instead_of_revoke(cx: &mut TestAppContext) {
    use deck_runtime::broker::Listed;

    let (deck, mut cx) = open(cx);
    click_control(&mut cx, "hdr-access");
    deck.update(&mut cx, |d, cx| {
        d.on_credentials(
            0,
            Ok(vec![
                Listed {
                    name: "PAUSED_KEY".into(),
                    fingerprint: None,
                    revoked: false,
                    error: Some("OS access was cancelled or refused; automatic reads are paused.".into()),
                    paused: true,
                },
                Listed {
                    name: "ACTIVE_KEY".into(),
                    fingerprint: Some("sha256:123456789abc".into()),
                    revoked: false,
                    error: None,
                    paused: false,
                },
            ]),
            cx,
        );
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("access-retry-0").is_some(), "a paused credential offers Retry");
    assert!(cx.debug_bounds("access-revoke-0").is_none(), "a paused credential has no value to revoke");
    assert!(cx.debug_bounds("access-retry-1").is_none(), "an active credential is not retryable");
    assert!(cx.debug_bounds("access-revoke-1").is_some());
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
    let project = daemon.open_project(&data, argv.clone()).unwrap();
    let task = daemon
        .run(Spec { argv: argv.clone(), cwd: data.clone(), project: Some(project.id), github: None })
        .unwrap()
        .task;
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
    let project = daemon.open_project(&data, argv.clone()).unwrap();
    let task = daemon
        .run(Spec {
            argv: argv.clone(),
            cwd: data.clone(),
            project: Some(project.id),
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

/// A project as `deckd` lists it.
fn project(id: &str, name: &str) -> deck_runtime::project::Project {
    deck_runtime::project::Project {
        id: id.into(),
        name: name.into(),
        checkouts: vec![std::path::PathBuf::from(format!("/src/{name}"))],
        argv: vec!["construct".into(), "acp".into()],
        opened_ms: 1,
        hidden: false,
    }
}

/// A task record as `deckd` lists it.
fn task(id: &str, state: serde_json::Value, publication: Option<serde_json::Value>) -> deck_runtime::Task {
    serde_json::from_value(json!({
        "id": id, "backend": "vm",
        "spec": { "argv": ["construct", "acp"], "cwd": format!("/src/{id}"), "project": "git:test" },
        "state": state, "pid": null, "publication": publication,
    }))
    .unwrap()
}

/// Select the project every [`task`] helper's record belongs to.
fn seed_project(deck: &gpui::Entity<Deck>, cx: &mut VisualTestContext) {
    deck.update(cx, |d, cx| d.on_projects(vec![project("git:test", "test")], Some("git:test".into()), cx));
}

/// Three more tasks on the board, as a daemon refresh would show them.
fn seed(deck: &gpui::Entity<Deck>, cx: &mut VisualTestContext) {
    seed_project(deck, cx);
    let tasks = ["a", "b", "c"].map(|id| task(id, json!({ "state": "running" }), None)).to_vec();
    deck.update(cx, |d, cx| d.on_tasks(tasks, Default::default(), cx));
}

#[gpui::test]
fn dragging_a_card_with_the_mouse_reorders_lanes(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    seed(&deck, &mut cx);
    cx.run_until_parked();
    let before = deck.read_with(&cx, |d, _| d.board().order());
    assert_eq!(before, vec![1, 2, 3], "the project's tasks; no live card without a selected task");

    // The open sidebar holds the task rows; hover its glyph to open it.
    open_sidebar(&deck, &mut cx);
    let start = point(px(148.), px(198.));
    cx.simulate_mouse_move(start, None, Modifiers::none());
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    assert_eq!(deck.read_with(&cx, |d, _| d.board().dragging()), Some(1), "mouse down on the first card grabs it");
    for step in 1..=10 {
        let y = start.y + px(PITCH * 2.2 * step as f32 / 10.);
        cx.simulate_mouse_move(point(start.x, y), Some(MouseButton::Left), Modifiers::none());
    }
    cx.simulate_mouse_up(point(start.x, start.y + px(PITCH * 2.2)), MouseButton::Left, Modifiers::none());
    deck.read_with(&cx, |d, _| {
        assert_eq!(d.board().order(), vec![2, 3, 1]);
        assert_eq!(d.board().dragging(), None);
    });
}

#[gpui::test]
fn the_rail_opens_under_the_pointer_and_selects_a_project_by_click(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    let older = project("git:older", "older");
    let newer = deck_runtime::project::Project { opened_ms: 2, ..project("git:newer", "newer") };
    deck.update(&mut cx, |d, cx| d.on_projects(vec![older, newer], Some("git:newer".into()), cx));
    cx.run_until_parked();
    assert!(!deck.read_with(&cx, |d, _| d.rail().expanded()));
    assert_eq!(deck.read_with(&cx, |d, _| d.rail().active()), Some(0), "the newest project is selected first");

    let (x, y) = deck.read_with(&cx, |d, _| d.rail_item_center(1));
    let older_item = point(px(x), px(y));
    cx.simulate_mouse_move(older_item, None, Modifiers::none());
    assert!(deck.read_with(&cx, |d, _| d.rail().expanded()), "hovering the rail opens it");
    cx.simulate_click(older_item, Modifiers::none());
    assert_eq!(deck.read_with(&cx, |d, _| d.rail().active()), Some(1));
    assert_eq!(deck.read_with(&cx, |d, _| d.project().map(str::to_string)), Some("git:older".into()));

    cx.simulate_mouse_move(point(px(700.), px(300.)), None, Modifiers::none());
    assert!(!deck.read_with(&cx, |d, _| d.rail().expanded()), "leaving the rail closes it");
}

#[gpui::test]
fn the_rail_lists_projects_and_open_project(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    let older = project("git:older", "older");
    let newer = deck_runtime::project::Project { opened_ms: 2, ..project("git:newer", "newer") };
    deck.update(&mut cx, |d, cx| d.on_projects(vec![older, newer], Some("git:newer".into()), cx));
    cx.run_until_parked();
    open_sidebar(&deck, &mut cx);
    assert!(cx.debug_bounds("sidebar-project-0").is_some(), "the sidebar renders the newest project");
    assert!(cx.debug_bounds("sidebar-project-1").is_some(), "and the other one");
    assert!(cx.debug_bounds("sidebar-project-2").is_none(), "and nothing else");
    assert!(cx.debug_bounds("sidebar-open").is_some(), "Open project… sits at its foot");
    assert!(cx.debug_bounds("sidebar-pin").is_some(), "the pin sits at its foot");
    assert!(cx.debug_bounds("sidebar-settings").is_none(), "the sidebar has no Settings item");
    assert!(cx.debug_bounds("sidebar-projects-header").is_some(), "the Projects label separates the sections");
    assert!(cx.debug_bounds("sidebar-project-chevron-0").is_some(), "a project group has a chevron");
    // Clicking another project's header still selects it.
    click_control(&mut cx, "sidebar-project-1");
    assert_eq!(deck.read_with(&cx, |d, _| d.project().map(str::to_string)), Some("git:older".into()));
}

/// The selected project's task rows indent under its tile, and its chevron
/// folds the list (ADR 0010 §9).
#[gpui::test]
fn a_project_header_holds_its_tasks_and_its_chevron_folds_them(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    seed(&deck, &mut cx);
    cx.run_until_parked();
    open_sidebar(&deck, &mut cx);
    assert!(cx.debug_bounds("sidebar-tasks").is_some(), "the selected project lists its tasks");
    let project = cx.debug_bounds("sidebar-project-0").unwrap();
    let task = cx.debug_bounds("card-row-1").unwrap();
    assert!(task.left() > project.left(), "a task row indents to the project's tile: {task:?} {project:?}");

    click_control(&mut cx, "sidebar-project-chevron-0");
    assert!(cx.debug_bounds("sidebar-tasks").is_none(), "the chevron folds the selected project");
    click_control(&mut cx, "sidebar-project-chevron-0");
    assert!(cx.debug_bounds("sidebar-tasks").is_some(), "and opens it again");
}

#[gpui::test]
fn the_header_of_an_empty_project_names_only_the_project(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    seed_project(&deck, &mut cx);
    cx.run_until_parked();
    deck.read_with(&cx, |d, _| {
        let header = d.header_title();
        assert_eq!(header.title, None, "a project without a task has no task title");
        assert_eq!(header.project.as_deref(), Some("test"), "the header still names the project");
        assert_eq!(header.detail, None, "and has no status line");
    });
    assert!(cx.debug_bounds("hdr-title").is_some(), "the project's name is the title block");
    assert!(cx.debug_bounds("hdr-project").is_none(), "there is no chip to repeat it");
    assert!(cx.debug_bounds("hdr-detail").is_none());
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
    use deck_runtime::{DaemonClient, Spec, State};
    // The ACP reader wakes the UI from a real process, outside the test scheduler.
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-quit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let cwd = std::env::temp_dir();
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    let project = daemon.open_project(&cwd, argv.clone()).unwrap();
    let task =
        daemon.create(Spec { argv: argv.clone(), cwd: cwd.clone(), project: Some(project.id), github: None }).unwrap();
    daemon.start(&task.id).unwrap();
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, cwd, cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    let id = deck.read_with(&vcx, |d, _| {
        d.task_id().map(str::to_owned).unwrap_or_else(|| panic!("the window runs no task: {:?}", d.session().status))
    });
    assert_eq!(id, task.id, "launch selects the project's task");
    // The launch's connection attempt is a background task; let it finish so
    // the window holds nothing when it quits.
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.session().status != Status::Connecting));
    deck.read_with(&vcx, |d, _| assert_eq!(d.session().status, Status::Detached));
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
    let project = daemon.open_project(&cwd, argv.clone()).unwrap();
    let original = daemon
        .run(Spec { argv: argv.clone(), cwd: cwd.clone(), project: Some(project.id), github: None })
        .unwrap()
        .task;
    cx.update(deck::app::bind);
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, cwd, cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.session().status != Status::Connecting));
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
    let project = daemon.open_project(&cwd, argv.clone()).unwrap();
    let original = daemon
        .run(Spec { argv: argv.clone(), cwd: cwd.clone(), project: Some(project.id), github: None })
        .unwrap()
        .task;
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
    let project = daemon.open_project(&cwd, argv.clone()).unwrap();
    let original = daemon
        .run(Spec { argv: argv.clone(), cwd: cwd.clone(), project: Some(project.id), github: None })
        .unwrap()
        .task;
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
#[gpui::test]
fn launching_inside_a_checkout_opens_its_project(cx: &mut TestAppContext) {
    use deck_runtime::DaemonClient;
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-launch-repo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let repo = git_repo(&data, "repo");
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let expected = deck_runtime::project::identify(&repo).unwrap().id;
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv.clone(), repo.clone(), cx)))
            .unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    vcx.run_until_parked();
    deck.read_with(&vcx, |d, _| {
        assert_eq!(d.project(), Some(expected.as_str()), "the checkout's root commit is the project");
        assert_eq!(d.task_id(), None, "opening a project starts no task");
        assert_eq!(d.projects().len(), 1);
        assert!(d.awaiting_task(), "the composer is ready for the first prompt");
    });
    assert!(vcx.debug_bounds("project-empty-state").is_some(), "the empty project state shows");
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    assert!(daemon.projects().unwrap().iter().any(|project| project.id == expected), "deckd recorded it");
    assert_eq!(daemon.tasks().unwrap().len(), 0);
    drop(daemon);
    cx.quit();
    cleanup_daemon(&data);
}

#[cfg(unix)]
#[gpui::test]
fn launching_outside_a_checkout_selects_the_project_opened_last(cx: &mut TestAppContext) {
    use deck_runtime::DaemonClient;
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-launch-last-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let repo = git_repo(&data, "repo");
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let opened = {
        let daemon = DaemonClient::connect_or_start(&data).unwrap();
        daemon.open_project(&repo, argv.clone()).unwrap()
    };
    let elsewhere = data.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, elsewhere, cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    vcx.run_until_parked();
    deck.read_with(&vcx, |d, _| {
        assert_eq!(d.project(), Some(opened.id.as_str()));
        assert_eq!(d.task_id(), None);
    });
    cx.quit();
    cleanup_daemon(&data);
}

#[cfg(unix)]
#[gpui::test]
fn launching_with_no_projects_shows_the_empty_state(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-launch-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let plain = data.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, plain, cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    vcx.run_until_parked();
    deck.read_with(&vcx, |d, _| {
        assert_eq!(d.project(), None);
        assert_eq!(d.task_id(), None);
        assert!(d.projects().is_empty());
    });
    assert!(vcx.debug_bounds("project-empty-state").is_some(), "the empty state shows");
    assert!(vcx.debug_bounds("empty-open").is_some(), "with Open project… in it");
    cx.quit();
    cleanup_daemon(&data);
}

#[cfg(unix)]
#[gpui::test]
fn selecting_a_project_shows_only_its_tasks(cx: &mut TestAppContext) {
    use deck_runtime::{DaemonClient, Spec, State};
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-projects-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let one = git_repo(&data, "one");
    let two = git_repo(&data, "two");
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    let p1 = daemon.open_project(&one, argv.clone()).unwrap();
    let t1 = daemon
        .create(Spec { argv: argv.clone(), cwd: one.clone(), project: Some(p1.id.clone()), github: None })
        .unwrap();
    let p2 = daemon.open_project(&two, argv.clone()).unwrap();
    let t2 = daemon
        .create(Spec { argv: argv.clone(), cwd: two.clone(), project: Some(p2.id.clone()), github: None })
        .unwrap();
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, one, cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    let index = deck.read_with(&vcx, |d, _| {
        assert_eq!(d.project(), Some(p1.id.as_str()), "the launch checkout's project is open");
        assert_eq!(d.task_id(), Some(t1.id.as_str()), "and its task is selected");
        let ids: Vec<&str> = d.projects().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec![p1.id.as_str(), p2.id.as_str()], "most recently opened first");
        d.projects().iter().position(|p| p.id == p2.id).unwrap()
    });
    wait_view(&mut vcx, |_| matches!(daemon.task(&t1.id).unwrap().state, State::Running));

    // A project click shows its project view, never one of its tasks.
    let (x, y) = deck.read_with(&vcx, |d, _| d.rail_item_center(index));
    vcx.simulate_click(point(px(x), px(y)), Modifiers::none());
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.task_id().is_none() && d.project() == Some(p2.id.as_str())));
    assert!(vcx.debug_bounds("project-view").is_some(), "the project view shows");
    assert!(vcx.debug_bounds("transcript-empty").is_none(), "and not a task's transcript");
    let lane = deck.read_with(&vcx, |d, _| {
        assert_eq!(d.project(), Some(p2.id.as_str()));
        assert_eq!(d.rail().active(), Some(index));
        assert_eq!(d.board().cards().len(), 1, "only the selected project's task is in the list");
        assert!(d.attached_ids().contains(&t1.id), "the other project's live session stays attached");
        d.board().order()[0]
    });
    let card = card_bounds(&mut vcx, lane).center();
    vcx.simulate_click(card, Modifiers::none());
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.task_id() == Some(t2.id.as_str())));
    wait_view(&mut vcx, |_| matches!(daemon.task(&t2.id).unwrap().state, State::Running));
    deck.read_with(&vcx, |d, _| {
        assert_eq!(d.board().cards().len(), 1);
        assert_eq!(d.board().cards()[0].lane.id, deck::ui::root::LIVE);
        assert!(d.attached_ids().contains(&t1.id));
    });

    // Back to the first: its live attachment was kept, and the second leaves the column.
    let (x, y) = deck.read_with(&vcx, |d, _| d.rail_item_center(0));
    vcx.simulate_click(point(px(x), px(y)), Modifiers::none());
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.task_id().is_none()));
    let lane = deck.read_with(&vcx, |d, _| d.board().order()[0]);
    let card = card_bounds(&mut vcx, lane).center();
    vcx.simulate_click(card, Modifiers::none());
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.task_id() == Some(t1.id.as_str())));
    deck.read_with(&vcx, |d, _| {
        assert_eq!(d.board().cards().len(), 1);
        assert!(d.attached_ids().contains(&t2.id));
    });

    cx.quit();
    for id in [&t1.id, &t2.id] {
        daemon.stop(id).unwrap();
    }
    drop(daemon);
    cleanup_daemon(&data);
}

#[cfg(unix)]
#[gpui::test]
fn the_first_prompt_in_an_empty_project_creates_its_task(cx: &mut TestAppContext) {
    use deck_runtime::DaemonClient;
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-first-prompt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let repo = git_repo(&data, "repo");
    let fake = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("deck-fake-agent");
    assert!(fake.is_file(), "build the fake agent beside deck: {}", fake.display());
    let journal = format!("DECK_FAKE_STATE={}", data.join("journal").display());
    let argv = ["env", "DECK_FAKE_DELAY_MS=1", &journal, &fake.display().to_string()].map(str::to_string).to_vec();
    let project = {
        let daemon = DaemonClient::connect_or_start(&data).unwrap();
        daemon.open_project(&repo, argv.clone()).unwrap()
    };
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv.clone(), repo.clone(), cx)))
            .unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    vcx.run_until_parked();
    deck.read_with(&vcx, |d, _| {
        assert_eq!(d.project(), Some(project.id.as_str()));
        assert_eq!(d.task_id(), None);
        assert!(d.awaiting_task());
    });
    assert!(vcx.debug_bounds("project-view").is_some(), "an open project shows its project view");
    assert!(vcx.debug_bounds("project-empty-state").is_some(), "with what a task would see");

    vcx.simulate_input("hello");
    vcx.simulate_keystrokes("enter");
    wait_view(&mut vcx, |cx| said(&deck, cx) == vec!["hello".to_string()]);
    let id = deck.read_with(&vcx, |d, _| d.task_id().unwrap().to_string());
    assert_eq!(
        deck.read_with(&vcx, |d, _| d.board().cards()[0].lane.title.clone()),
        "hello",
        "the row shows the first prompt before the next task refresh"
    );
    // The fake agent asks a permission during its turn; allow it so the
    // transcript settles as an ordinary finished turn.
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.session().ask.is_some()));
    vcx.simulate_keystrokes("1");
    ready_on(&deck, &mut vcx, Some(&id));

    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    wait_view(&mut vcx, |_| daemon.task(&id).unwrap().title.as_deref() == Some("hello"));
    let record = daemon.task(&id).unwrap();
    assert_eq!(record.title.as_deref(), Some("hello"), "deckd recorded the first prompt's first line");
    assert_eq!(record.spec.project.as_deref(), Some(project.id.as_str()));
    assert_eq!(record.spec.cwd, repo, "the task runs in the project's checkout");
    assert_eq!(record.spec.argv, argv, "with the project's command");
    assert_eq!(daemon.tasks().unwrap().len(), 1, "the first prompt created exactly one task");
    cx.quit();
    daemon.stop(&id).unwrap();
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
    use deck_runtime::{DaemonClient, Spec, State};
    // The watch connection and the held request wake the UI from real threads.
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-boundary-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let cwd = std::env::temp_dir();
    let opener = DaemonClient::connect_or_start(&data).unwrap();
    let project = opener.open_project(&cwd, argv.clone()).unwrap();
    opener.create(Spec { argv: argv.clone(), cwd: cwd.clone(), project: Some(project.id), github: None }).unwrap();
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, cwd, cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    let id = deck.read_with(&vcx, |d, _| {
        d.task_id().map(str::to_owned).unwrap_or_else(|| panic!("the window runs no task: {:?}", d.session().status))
    });
    // Wait for the launch's start; the watch lease must exist before asking.
    wait_view(&mut vcx, |_| matches!(opener.task(&id).unwrap().state, State::Running));

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
    drop(opener);
    cleanup_daemon(&data);
}

#[cfg(unix)]
#[gpui::test]
fn a_late_boundary_decision_reaches_an_agent_that_advertised_it(cx: &mut TestAppContext) {
    use deck_runtime::approvals::{Ask, Scope, State as ApprovalState};
    use deck_runtime::{DaemonClient, Spec, State};
    // The watch connection and the held request wake the UI from real threads.
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-boundary-note-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let fake = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("deck-fake-agent");
    assert!(fake.is_file(), "build the fake agent beside deck: {}", fake.display());
    // `env` gives the agent a boundary endpoint, so it advertises the
    // capability and accepts the notification.
    let argv = vec!["env".to_string(), "DECK_BOUNDARY_URL=http://127.0.0.1:9".to_string(), fake.display().to_string()];
    let cwd = std::env::temp_dir();
    let opener = DaemonClient::connect_or_start(&data).unwrap();
    let project = opener.open_project(&cwd, argv.clone()).unwrap();
    opener.create(Spec { argv: argv.clone(), cwd: cwd.clone(), project: Some(project.id), github: None }).unwrap();
    drop(opener);
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, cwd, cx))).unwrap()
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
    seed_project(&deck, &mut cx);
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
    assert_eq!(order.len(), 2, "a and b; a destroyed task has none");
    assert_eq!(
        lanes[0],
        ("Untitled · b".into(), "stopped · vm".into(), None),
        "most recent activity first, titled by its id until it has one (issue #162)"
    );
    assert_eq!(lanes[1], ("Untitled · a".into(), "vm".into(), Some((Dot::ChecksRunning, "#7".into()))));

    // The chip opens the panel without picking the card up.
    let lane = order[1];
    open_sidebar(&deck, &mut cx);
    let selector = match lane {
        1 => "card-chip-1",
        2 => "card-chip-2",
        _ => panic!("unexpected lane {lane}"),
    };
    click_control(&mut cx, selector);
    assert_eq!(deck.read_with(&cx, |d, _| d.board().dragging()), None);
    assert_eq!(deck.read_with(&cx, |d, _| d.open_publication().map(|(t, _)| t.to_string())), Some("a".into()));
    assert!(cx.debug_bounds("publication-panel").is_some());
    assert!(cx.debug_bounds("publication-destroy").is_none(), "nothing to destroy before a merge");
    cx.simulate_keystrokes("escape");
    assert!(cx.debug_bounds("publication-panel").is_none());
}

/// A row and the publication panel read the record's title, never the
/// checkout's directory name (issue #162).
#[gpui::test]
fn a_task_row_shows_its_recorded_title_instead_of_its_directory(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    seed_project(&deck, &mut cx);
    let named: deck_runtime::Task = serde_json::from_value(json!({
        "id": "abcdef123456", "backend": "local", "title": "fix the rail drag",
        "spec": { "argv": ["construct", "acp"], "cwd": "/src/some-checkout", "project": "git:test" },
        "state": { "state": "running" }, "pid": null,
    }))
    .unwrap();
    deck.update(&mut cx, |d, cx| d.on_tasks(vec![named], Default::default(), cx));
    deck.read_with(&cx, |d, _| {
        assert_eq!(d.board().cards()[0].lane.title, "fix the rail drag", "the row shows the recorded title");
        assert_eq!(d.task_title("abcdef123456"), "fix the rail drag", "and so does the publication panel");
    });
}

#[gpui::test]
fn a_merged_pull_request_marks_the_task_done_and_offers_destroy_behind_a_second_click(cx: &mut TestAppContext) {
    use deck_runtime::pull_request::{Live, State as Published};
    let (deck, mut cx) = open(cx);
    seed_project(&deck, &mut cx);
    let pull = json!({ "kind": "pull_request", "number": 9, "url": "https://github.com/o/r/pull/9", "title": "t", "draft": false });
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(0))
        .unwrap();
    let merged = Published {
        publication: Some(serde_json::from_value(pull.clone()).unwrap()),
        live: Some(Live {
            read_at_ms: now_ms,
            pull_request: Some(json!({ "state": "closed", "merged": true })),
            ..Live::default()
        }),
        fresh: false,
    };
    deck.update(&mut cx, |d, cx| {
        d.on_tasks(vec![task("a", json!({ "state": "running" }), Some(pull))], [("a".to_string(), merged)].into(), cx)
    });
    cx.run_until_parked();
    // A merged pull request settles the task, so its card folds below the
    // column; its chip still opens the publication panel.
    assert_eq!(deck.read_with(&cx, |d, _| d.settled().len()), 1);
    assert_eq!(deck.read_with(&cx, |d, _| d.board().cards().len()), 0);
    open_sidebar(&deck, &mut cx);
    click_control(&mut cx, "settled-fold");
    click_control(&mut cx, "settled-chip-0");
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
    let project = daemon.open_project(&data, argv.clone()).unwrap();
    let task = daemon
        .run(Spec { argv: argv.clone(), cwd: data.clone(), project: Some(project.id), github: None })
        .unwrap()
        .task;
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
    let project = daemon.open_project(&data, argv.clone()).unwrap();
    let task = daemon
        .run(Spec { argv: argv.clone(), cwd: data.clone(), project: Some(project.id), github: None })
        .unwrap()
        .task;
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
    use deck_runtime::{DaemonClient, Spec, State};
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-new-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let fake = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("deck-fake-agent");
    assert!(fake.is_file(), "build the fake agent beside deck: {}", fake.display());
    let cwd = data.join("alpha");
    std::fs::create_dir_all(&cwd).unwrap();
    let journal = format!("DECK_FAKE_STATE={}", data.join("journal").display());
    let argv = ["env", "DECK_FAKE_DELAY_MS=1", &journal, &fake.display().to_string()].map(str::to_string).to_vec();
    let opener = DaemonClient::connect_or_start(&data).unwrap();
    let project = opener.open_project(&cwd, argv.clone()).unwrap();
    opener.create(Spec { argv: argv.clone(), cwd: cwd.clone(), project: Some(project.id), github: None }).unwrap();
    drop(opener);
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
    // The project's checkout is canonical (`/private/var` on macOS).
    let checkout = std::fs::canonicalize(&cwd).unwrap();
    assert_eq!((record.spec.cwd.as_path(), record.state.clone()), (checkout.as_path(), State::Running));
    assert_eq!(daemon.task(&own).unwrap().state, State::Running, "the previous session keeps running");
    deck.read_with(&vcx, |d, _| {
        assert_eq!(d.board().order().len(), 2, "the previous session is an ordinary card");
        assert!(!d.new_session_state().is_creating());
    });
    prompt_fake(&deck, &mut vcx, "second");

    // The header button does the same; then a card switches back.
    let plus = vcx.debug_bounds("hdr-new").unwrap().center();
    vcx.simulate_click(plus, Modifiers::none());
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.task_id() != Some(second.as_str())));
    let third = ready_on(&deck, &mut vcx, None);
    assert!(third != own && third != second);
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.board().order().len() == 3));
    open_sidebar(&deck, &mut vcx);
    vcx.simulate_click(point(px(148.), px(198. + PITCH)), Modifiers::none());
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
    let agent = |name: &str| {
        let journal = format!("DECK_FAKE_STATE={}", data.join(format!("journal-{name}")).display());
        ["env", "DECK_FAKE_DELAY_MS=1", &journal, &fake.display().to_string()].map(str::to_string).to_vec()
    };
    // One project with two checkouts, so both tasks stay in the column.
    let repo = git_repo(&data, "repo");
    std::fs::create_dir_all(repo.join("beta")).unwrap();
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    let project = daemon.open_project(&repo, agent("alpha")).unwrap();
    // The second task has never run: selecting it starts it.
    let other = daemon
        .create(Spec { argv: agent("beta"), cwd: repo.join("beta"), project: Some(project.id.clone()), github: None })
        .unwrap()
        .id;
    let alpha = Spec { argv: agent("alpha"), cwd: repo.join("alpha"), project: Some(project.id), github: None };
    std::fs::create_dir_all(&alpha.cwd).unwrap();
    let launched = daemon.create(alpha.clone()).unwrap();
    // The rows read their records' titles, not their checkouts' names.
    daemon.title(&launched.id, "alpha").unwrap();
    daemon.title(&other, "beta").unwrap();
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, alpha.argv, alpha.cwd, cx)))
            .unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    let own = ready_on(&deck, &mut vcx, None);
    assert_eq!(own, launched.id, "launch finds the task for its own command and directory");
    prompt_fake(&deck, &mut vcx, "first");
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.board().order().len() == 2));
    let lane = deck.read_with(&vcx, |d, _| d.board().order()[1]);

    // The second card sits one pitch below the live card.
    open_sidebar(&deck, &mut vcx);
    let card = point(px(148.), px(198. + PITCH));
    vcx.simulate_click(card, Modifiers::none());
    assert_eq!(deck.read_with(&vcx, |d, _| d.board().dragging()), None, "a click is not a drag");
    ready_on(&deck, &mut vcx, Some(&other));
    deck.read_with(&vcx, |d, _| {
        assert_eq!(d.board().order(), vec![deck::ui::root::LIVE, lane], "the column keeps its shape");
        assert_eq!(
            d.board().cards()[0].lane.title,
            "beta",
            "the selected row is its record's title, not the agent's name"
        );
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

/// The header names the selected task and its project, and follows a switch
/// between projects (issue #156).
#[cfg(unix)]
#[gpui::test]
fn the_header_names_the_selected_task_and_follows_a_switch(cx: &mut TestAppContext) {
    use deck_runtime::{DaemonClient, Spec};
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-header-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let fake = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("deck-fake-agent");
    assert!(fake.is_file(), "build the fake agent beside deck: {}", fake.display());
    let argv = vec![fake.display().to_string()];
    let one = git_repo(&data, "one");
    let two = git_repo(&data, "two");
    let alpha = one.join("alpha");
    let beta = two.join("beta");
    std::fs::create_dir_all(&alpha).unwrap();
    std::fs::create_dir_all(&beta).unwrap();
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    let first = daemon.open_project(&one, argv.clone()).unwrap();
    let second = daemon.open_project(&two, argv.clone()).unwrap();
    let spec = |cwd: &std::path::Path, project: &str| Spec {
        argv: argv.clone(),
        cwd: cwd.to_path_buf(),
        project: Some(project.to_string()),
        github: None,
    };
    let a = daemon.create(spec(&alpha, &first.id)).unwrap().id;
    let b = daemon.create(spec(&beta, &second.id)).unwrap().id;
    // Recorded titles are what the header shows, not the checkouts' names.
    daemon.title(&a, "alpha").unwrap();
    daemon.title(&b, "beta").unwrap();

    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv.clone(), alpha, cx)))
            .unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    ready_on(&deck, &mut vcx, Some(&a));
    vcx.run_until_parked();
    deck.read_with(&vcx, |d, _| {
        let header = d.header_title();
        assert_eq!(header.title.as_deref(), Some("alpha"), "the header names the selected task");
        assert_eq!(header.project.as_deref(), Some("one"), "and its project");
        assert_eq!(header.detail.as_deref(), Some("ready"), "and the agent's status");
    });
    assert!(vcx.debug_bounds("hdr-title").is_some(), "the title block renders");
    assert!(vcx.debug_bounds("hdr-project").is_some(), "with the project chip");
    assert!(vcx.debug_bounds("hdr-detail").is_some(), "and the status line");
    let title = vcx.debug_bounds("hdr-title").expect("the title renders");
    let chip = vcx.debug_bounds("hdr-project").expect("the chip renders");
    assert!(title.size.width > px(10.), "the title shows its text, not only an ellipsis: {title:?}");
    assert!(chip.left() <= title.right() + px(12.), "the chip sits beside the title: {title:?} {chip:?}");

    // Another project's task: both the title and the chip follow the selection.
    deck.update(&mut vcx, |d, cx| d.select_task_id(&b, cx));
    ready_on(&deck, &mut vcx, Some(&b));
    vcx.run_until_parked();
    deck.read_with(&vcx, |d, _| {
        let header = d.header_title();
        assert_eq!(header.title.as_deref(), Some("beta"), "the title follows the switch");
        assert_eq!(header.project.as_deref(), Some("two"), "and so does the project chip");
    });

    // The project chip leads back to the project view; only the place shown
    // is highlighted in the sidebar (ADR 0010 §1).
    open_sidebar(&deck, &mut vcx);
    assert!(vcx.debug_bounds("sidebar-project-marker").is_none(), "a shown task highlights only its row");
    click_control(&mut vcx, "hdr-project");
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.task_id().is_none()));
    assert!(vcx.debug_bounds("project-view").is_some(), "the project chip shows the project view");
    assert!(vcx.debug_bounds("transcript-empty").is_none(), "and not the task's transcript");
    open_sidebar(&deck, &mut vcx);
    assert!(vcx.debug_bounds("sidebar-project-marker").is_some(), "the project row marks the view shown");

    cx.quit();
    for id in [&a, &b] {
        daemon.stop(id).unwrap();
    }
    drop(daemon);
    cleanup_daemon(&data);
}

/// Start a fake-agent task, record its ACP session, then detach. The task
/// keeps running, which is what a later window attaches to.
#[cfg(unix)]
fn start_recorded(data: &std::path::Path, argv: &[String], cwd: std::path::PathBuf) -> String {
    use deck::task::WindowTask;
    let (task, connection) = WindowTask::run(data, argv.to_vec(), cwd).unwrap();
    let id = task.id().to_string();
    let (_client, mut rx) = connection.unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match rx.try_recv() {
            Ok(Inbound::Ready { session_id, .. }) => {
                task.record_session(&session_id).unwrap();
                break;
            }
            Ok(_) => {}
            Err(_) => {
                assert!(std::time::Instant::now() < deadline, "the fake agent never became ready");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
    id
}

/// A task recorded before titles existed takes its title from its replayed
/// transcript when a window attaches (ADR 0010 §9).
#[cfg(unix)]
#[gpui::test]
fn a_task_without_a_title_gets_one_from_its_replayed_transcript(cx: &mut TestAppContext) {
    use deck::task::WindowTask;
    use deck_runtime::DaemonClient;
    cx.background_executor.allow_parking();
    let (data, argv, alpha, _beta) = two_fake_tasks("backfill");
    let (task, connection) = WindowTask::run(&data, argv.clone(), alpha.clone()).unwrap();
    let id = task.id().to_string();
    let (client, mut rx) = connection.unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match rx.try_recv() {
            Ok(Inbound::Ready { session_id, .. }) => {
                task.record_session(&session_id).unwrap();
                break;
            }
            Ok(_) => {}
            Err(_) => {
                assert!(std::time::Instant::now() < deadline, "the fake agent never became ready");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
    client.prompt("ship the spike").unwrap();
    // Let the turn finish, answering its question, so the agent is idle and
    // its transcript is complete when the window attaches.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match rx.try_recv() {
            Ok(Inbound::Permission { request, options, .. }) => {
                let allow = options.iter().find(|o| o.kind.starts_with("allow")).map(|o| o.id.as_str());
                client.answer(&request, allow).unwrap();
            }
            // The fake agent reports an idle turn; a real agent may answer
            // with TurnEnded instead.
            Ok(Inbound::Turn { state: deck_acp::TurnState::Idle { .. }, .. } | Inbound::TurnEnded(_)) => break,
            Ok(_) => {}
            Err(_) => {
                assert!(std::time::Instant::now() < deadline, "the fake agent never finished its turn");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
    // Both connections go; the task itself keeps running for the window.
    drop(client);
    drop(task);
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    assert_eq!(daemon.task(&id).unwrap().title, None, "the record predates titles");

    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, alpha, cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    ready_on(&deck, &mut vcx, Some(&id));
    wait_view(&mut vcx, |_| daemon.task(&id).unwrap().title.as_deref() == Some("ship the spike"));
    assert_eq!(
        deck.read_with(&vcx, |d, _| d.board().cards()[0].lane.title.clone()),
        "ship the spike",
        "the row takes the replayed prompt's first line"
    );

    cx.quit();
    daemon.stop(&id).unwrap();
    drop(daemon);
    cleanup_daemon(&data);
}

/// A git checkout under `data/name` with one commit: a project by root commit.
#[cfg(unix)]
fn git_repo(data: &std::path::Path, name: &str) -> std::path::PathBuf {
    let dir = data.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let run = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&dir)
            .args(["-c", "user.email=deck@test", "-c", "user.name=deck"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    if !dir.join(".git").exists() {
        run(&["init", "-q"]);
        // A distinct message keeps two checkouts' root commits distinct.
        run(&["commit", "-q", "--allow-empty", "-m", &format!("init {name}")]);
    }
    std::fs::canonicalize(&dir).unwrap()
}

/// A fake-agent launch pair: one project with two checkouts, and the agent
/// command both tasks run.
#[cfg(unix)]
fn two_fake_tasks(name: &str) -> (std::path::PathBuf, Vec<String>, std::path::PathBuf, std::path::PathBuf) {
    let data = std::env::temp_dir().join(format!("deck-view-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let fake = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("deck-fake-agent");
    assert!(fake.is_file(), "build the fake agent beside deck: {}", fake.display());
    let argv = vec![fake.display().to_string()];
    let repo = git_repo(&data, "repo");
    let alpha = repo.join("alpha");
    let beta = repo.join("beta");
    std::fs::create_dir_all(&alpha).unwrap();
    std::fs::create_dir_all(&beta).unwrap();
    (data, argv, alpha, beta)
}

#[cfg(unix)]
#[gpui::test]
fn launching_attaches_every_running_task_that_recorded_a_session(cx: &mut TestAppContext) {
    use deck_runtime::{DaemonClient, State};
    cx.background_executor.allow_parking();
    let (data, argv, alpha, beta) = two_fake_tasks("attach-live");
    let a = start_recorded(&data, &argv, alpha.clone());
    let b = start_recorded(&data, &argv, beta.clone());
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    let pids = (daemon.task(&a).unwrap().pid, daemon.task(&b).unwrap().pid);

    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, alpha, cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let both = |d: &Deck| {
        let ids = d.attached_ids();
        ids.len() == 2 && ids.contains(&a) && ids.contains(&b)
    };
    while !deck.read_with(&vcx, |d, _| both(d)) {
        vcx.run_until_parked();
        assert!(std::time::Instant::now() < deadline, "the window never attached both tasks");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(daemon.task(&a).unwrap().pid, pids.0, "attaching starts nothing");
    assert_eq!(daemon.task(&b).unwrap().pid, pids.1, "attaching starts nothing");
    assert_eq!(daemon.task(&a).unwrap().state, State::Running);
    assert_eq!(daemon.task(&b).unwrap().state, State::Running);

    cx.quit();
    for id in [&a, &b] {
        daemon.stop(id).unwrap();
    }
    drop(daemon);
    cleanup_daemon(&data);
}

#[cfg(unix)]
#[gpui::test]
fn switching_keeps_each_attachment_and_its_pending_question(cx: &mut TestAppContext) {
    use deck_runtime::approvals::{Ask, Scope, State as ApprovalState};
    use deck_runtime::{DaemonClient, State};
    cx.background_executor.allow_parking();
    let (data, argv, alpha, beta) = two_fake_tasks("kept");
    let a = start_recorded(&data, &argv, alpha.clone());
    let b = start_recorded(&data, &argv, beta.clone());
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    daemon.title(&a, "alpha").unwrap();
    daemon.title(&b, "beta").unwrap();
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, alpha, cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    ready_on(&deck, &mut vcx, Some(&a));
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.attached_ids().contains(&b)));
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.board().order().len() == 2));
    let beta_lane =
        deck.read_with(&vcx, |d, _| d.board().cards().iter().find(|c| c.lane.title == "beta").unwrap().lane.id);

    // A asks a permission question; leave it unanswered and switch away.
    vcx.simulate_input("first");
    vcx.simulate_keystrokes("enter");
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.session().ask.is_some()));
    let (request, entries) =
        deck.read_with(&vcx, |d, _| (d.session().ask.as_ref().unwrap().request.clone(), d.session().entries.len()));

    deck.update(&mut vcx, |d, cx| d.select(beta_lane, cx));
    ready_on(&deck, &mut vcx, Some(&b));
    assert_eq!(deck.read_with(&vcx, |d, _| d.session().entries.len()), 0, "beta keeps its own session");

    // Back to A: the same client, the same question, no second replay.
    let alpha_lane =
        deck.read_with(&vcx, |d, _| d.board().cards().iter().find(|c| c.lane.title == "alpha").unwrap().lane.id);
    deck.update(&mut vcx, |d, cx| d.select(alpha_lane, cx));
    // A is waiting on its question, not ready.
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.task_id() == Some(a.as_str())));
    deck.read_with(&vcx, |d, _| {
        let ask = d.session().ask.as_ref().expect("the question was kept");
        assert_eq!(ask.request, request, "the same question, not a replayed one");
        assert_eq!(d.session().entries.len(), entries, "switching back replays nothing");
    });
    vcx.simulate_keystrokes("1");
    ready_on(&deck, &mut vcx, Some(&a));
    assert_eq!(said(&deck, &vcx), vec!["first".to_string()], "the question was answerable");

    // An approval for beta raised while alpha is selected waits on beta's card.
    let ask = Ask {
        gateway: "github".into(),
        operation: "create_pr".into(),
        destination: "owner/repo".into(),
        credential: Some("DECK_GITHUB_TOKEN".into()),
        details: Default::default(),
        reason: None,
        coverage: None,
    };
    let b_ask = b.clone();
    let holder = DaemonClient::connect(&data).unwrap();
    let asked = std::thread::spawn(move || holder.request_approval(&b_ask, ask));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while daemon.approvals(&b).unwrap().is_empty() {
        assert!(std::time::Instant::now() < deadline, "beta's approval was never queued");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(deck.read_with(&vcx, |d, _| d.boundary().pending().is_none()), "alpha has no card");
    let beta_lane =
        deck.read_with(&vcx, |d, _| d.board().cards().iter().find(|c| c.lane.title == "beta").unwrap().lane.id);
    deck.update(&mut vcx, |d, cx| d.select(beta_lane, cx));
    ready_on(&deck, &mut vcx, Some(&b));
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.boundary().pending().is_some()));
    vcx.simulate_keystrokes("3");
    assert_eq!(asked.join().unwrap().unwrap().state, ApprovalState::Allowed(Scope::Task));
    assert_eq!(daemon.task(&b).unwrap().state, State::Running);

    cx.quit();
    for id in [&a, &b] {
        daemon.stop(id).unwrap();
    }
    drop(daemon);
    cleanup_daemon(&data);
}

/// The project view lists the project's unsettled tasks in the board's
/// order, a card switches to its task, its footer opens the rules, and the
/// composer starts another task (issue #164).
#[cfg(unix)]
#[gpui::test]
fn the_project_view_lists_tasks_and_starts_another(cx: &mut TestAppContext) {
    use deck_runtime::DaemonClient;
    cx.background_executor.allow_parking();
    let (data, argv, alpha, beta) = two_fake_tasks("project-view");
    let a = start_recorded(&data, &argv, alpha.clone());
    let b = start_recorded(&data, &argv, beta);
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    daemon.title(&a, "alpha").unwrap();
    daemon.title(&b, "beta").unwrap();
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, alpha, cx))).unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    ready_on(&deck, &mut vcx, Some(&a));
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.attached_ids().len() == 2 && d.board().cards().len() == 2));

    // A project click lists its tasks, newest to oldest, in the board's order.
    open_sidebar(&deck, &mut vcx);
    click_control(&mut vcx, "sidebar-project-0");
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.task_id().is_none()));
    draw(&mut vcx);
    assert!(vcx.debug_bounds("project-view").is_some());
    assert!(vcx.debug_bounds("transcript-empty").is_none(), "the project view replaces the transcript");
    assert!(vcx.debug_bounds("hdr-project").is_none(), "no task's header chip while the project shows");
    assert!(vcx.debug_bounds("project-view-sessions").is_some());
    assert!(vcx.debug_bounds("project-empty-state").is_none(), "a project with tasks lists them");
    deck.read_with(&vcx, |d, _| {
        assert_eq!(d.board().cards().len(), 2, "one card per unsettled task");
        assert!(d.attached_ids().contains(&a) && d.attached_ids().contains(&b), "attachments survive the switch");
        let titles: Vec<&str> = d.board().cards().iter().map(|c| c.lane.title.as_str()).collect();
        assert!(titles.contains(&"alpha") && titles.contains(&"beta"), "{titles:?}");
    });
    let order = deck.read_with(&vcx, |d, _| d.board().order());
    let tops: Vec<f32> = order.iter().map(|lane| card_bounds(&mut vcx, *lane).top().into()).collect();
    assert!(tops.windows(2).all(|pair| pair[0] < pair[1]), "the cards follow the board's order: {tops:?}");

    // A card switches to its task, as a sidebar row does.
    let beta_lane = lane_named(&deck, &vcx, "beta");
    let card = card_bounds(&mut vcx, beta_lane).center();
    vcx.simulate_click(card, Modifiers::none());
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.task_id() == Some(b.as_str())));
    draw(&mut vcx);
    assert!(vcx.debug_bounds("project-view").is_none(), "the card leaves the project view");
    assert!(vcx.debug_bounds("transcript-empty").is_some(), "and shows that task's transcript");
    assert!(vcx.debug_bounds("hdr-project").is_some(), "the header names the task's project");

    // The task header's project chip returns to the view; its footer opens
    // the project's rules.
    click_control(&mut vcx, "hdr-project");
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.task_id().is_none()));
    draw(&mut vcx);
    assert!(vcx.debug_bounds("project-view").is_some());
    click_control(&mut vcx, "project-view-rules");
    assert!(deck.read_with(&vcx, |d, _| d.access().is_open()), "Rules and grants opens the access panel");
    assert!(vcx.debug_bounds("project-view").is_none(), "the panel replaces the project view");
    vcx.simulate_keystrokes("escape");
    assert!(!deck.read_with(&vcx, |d, _| d.access().is_open()));
    assert!(vcx.debug_bounds("project-view").is_some(), "closing the panel returns to the project view");

    // Its composer starts another task, even though the project has tasks.
    vcx.simulate_input("third");
    vcx.simulate_keystrokes("enter");
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.task_id().is_some_and(|id| id != a && id != b)));
    let created = deck.read_with(&vcx, |d, _| d.task_id().unwrap().to_string());
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.session().ask.is_some()));
    vcx.simulate_keystrokes("1");
    ready_on(&deck, &mut vcx, Some(&created));
    wait_view(&mut vcx, |_| daemon.tasks().unwrap().len() == 3);
    assert_eq!(said(&deck, &vcx), vec!["third".to_string()], "the prompt created and reached a new task");

    cx.quit();
    for id in [&a, &b, &created] {
        daemon.stop(id).unwrap();
    }
    drop(daemon);
    cleanup_daemon(&data);
}

/// The lane of the card whose title is `title`.
#[cfg(unix)]
fn lane_named(deck: &gpui::Entity<Deck>, cx: &VisualTestContext, title: &str) -> usize {
    deck.read_with(cx, |d, _| {
        d.board()
            .cards()
            .iter()
            .find(|card| card.lane.title == title)
            .unwrap_or_else(|| panic!("no card titled {title}"))
            .lane
            .id
    })
}

#[cfg(unix)]
#[gpui::test]
fn the_needs_you_strip_opens_a_background_question_and_seen_clears_a_finished_turn(cx: &mut TestAppContext) {
    use deck::ui::attention::Reason;
    use deck_acp::TurnState;
    use deck_runtime::DaemonClient;
    cx.background_executor.allow_parking();
    let (data, argv, alpha, beta) = two_fake_tasks("needs-you");
    let a = start_recorded(&data, &argv, alpha.clone());
    let b = start_recorded(&data, &argv, beta.clone());
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    daemon.title(&a, "alpha").unwrap();
    daemon.title(&b, "beta").unwrap();
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv, alpha.clone(), cx)))
            .unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    ready_on(&deck, &mut vcx, Some(&a));
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.board().cards().len() == 2));
    wait_view(&mut vcx, |cx| {
        let ids = deck.read_with(cx, |d, _| d.attached_ids());
        ids.contains(&a) && ids.contains(&b)
    });

    // Beta asks a question, then stays in the background.
    let beta_lane = lane_named(&deck, &vcx, "beta");
    deck.update(&mut vcx, |d, cx| d.select(beta_lane, cx));
    ready_on(&deck, &mut vcx, Some(&b));
    vcx.simulate_input("first");
    vcx.simulate_keystrokes("enter");
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.session().ask.is_some()));
    let alpha_lane = lane_named(&deck, &vcx, "alpha");
    deck.update(&mut vcx, |d, cx| d.select(alpha_lane, cx));
    ready_on(&deck, &mut vcx, Some(&a));

    wait_view(&mut vcx, |cx| {
        deck.read_with(cx, |d, _| {
            d.needs_you().iter().any(|item| item.task == b && matches!(item.reason, Reason::Question { .. }))
        })
    });
    let index = deck.read_with(&vcx, |d, _| d.needs_you().iter().position(|item| item.task == b).unwrap());
    open_sidebar(&deck, &mut vcx);
    assert_eq!(index, 0, "the only item is the background question");
    let selector = "sidebar-need-0";
    assert!(vcx.debug_bounds(selector).is_some(), "the sidebar shows the background question");
    assert_eq!(deck.read_with(&vcx, |d, _| d.needs_you()[index].project.clone()), {
        deck.read_with(&vcx, |d, _| d.project().unwrap().to_string())
    });
    click_control(&mut vcx, selector);
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.task_id() == Some(b.as_str())));
    assert!(vcx.debug_bounds("permission-card").is_some(), "selecting the item opens its card");
    vcx.simulate_keystrokes("1");
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.needs_you().iter().all(|item| item.task != b)));

    // A turn that ends while the task is in the background is unseen.
    deck.update(&mut vcx, |d, cx| d.select(alpha_lane, cx));
    ready_on(&deck, &mut vcx, Some(&a));
    deck.update(&mut vcx, |d, cx| {
        d.on_inbound_for(
            &b,
            Inbound::Turn { number: 2, state: TurnState::Idle { stop_reason: Some("end_turn".into()) } },
            cx,
        )
    });
    wait_view(&mut vcx, |cx| {
        deck.read_with(cx, |d, _| {
            d.needs_you().iter().any(|item| item.task == b && matches!(item.reason, Reason::Finished { turn: 2, .. }))
        })
    });
    let index = deck.read_with(&vcx, |d, _| d.needs_you().iter().position(|item| item.task == b).unwrap());
    assert_eq!(index, 0, "the only item is the background question");
    click_control(&mut vcx, "sidebar-need-0");
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.task_id() == Some(b.as_str())));
    wait_view(&mut vcx, |_| daemon.task(&b).unwrap().seen >= 2);
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.needs_you().iter().all(|item| item.task != b)));

    cx.quit();
    for id in [&a, &b] {
        daemon.stop(id).unwrap();
    }
    drop(daemon);
    cleanup_daemon(&data);
}

#[cfg(unix)]
#[gpui::test]
fn marking_a_task_done_folds_it_and_unmarking_restores_it(cx: &mut TestAppContext) {
    use deck_runtime::{DaemonClient, Spec, State};
    cx.background_executor.allow_parking();
    let data = std::env::temp_dir().join(format!("deck-view-settled-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let repo = git_repo(&data, "repo");
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let daemon = DaemonClient::connect_or_start(&data).unwrap();
    let project = daemon.open_project(&repo, argv.clone()).unwrap();
    let spec = |name: &str| {
        let cwd = repo.join(name);
        std::fs::create_dir_all(&cwd).unwrap();
        Spec { argv: argv.clone(), cwd, project: Some(project.id.clone()), github: None }
    };
    let older = daemon.create(spec("alpha")).unwrap();
    let newer = daemon.create(spec("beta")).unwrap();
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| Deck::launch_in(&data, argv.clone(), repo.clone(), cx)))
            .unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let deck = window.root(&mut vcx).unwrap();
    vcx.update(|window, cx| deck.focus_handle(cx).focus(window, cx));
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.board().cards().len() == 2));
    assert_eq!(deck.read_with(&vcx, |d, _| d.task_id().map(str::to_string)), Some(newer.id.clone()));
    let lane = deck.read_with(&vcx, |d, _| d.board().order()[1]);
    assert_eq!(
        deck.read_with(&vcx, |d, _| d.board().cards()[1].lane.title.clone()),
        format!("Untitled · {}", older.id.chars().take(6).collect::<String>()),
        "a task without a record title stands in with its id, not its directory"
    );
    open_sidebar(&deck, &mut vcx);
    let selector = match lane {
        1 => "card-done-1",
        2 => "card-done-2",
        _ => panic!("unexpected lane {lane}"),
    };
    assert!(vcx.debug_bounds(selector).is_none(), "Mark done shows only while its row is hovered");
    let row_selector = match lane {
        1 => "card-row-1",
        2 => "card-row-2",
        _ => panic!("unexpected lane {lane}"),
    };
    let row = vcx.debug_bounds(row_selector).expect("the task row is laid out").center();
    vcx.simulate_mouse_move(row, None, Modifiers::none());
    vcx.run_until_parked();
    assert!(vcx.debug_bounds(selector).is_some(), "hovering the row shows its Mark done check");
    click_control(&mut vcx, selector);
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.settled().len() == 1 && d.board().cards().len() == 1));
    assert_eq!(deck.read_with(&vcx, |d, _| d.settled()[0].task.id.clone()), older.id);
    assert!(daemon.task(&older.id).unwrap().done_ms.is_some());

    click_control(&mut vcx, "settled-fold");
    click_control(&mut vcx, "settled-done-0");
    wait_view(&mut vcx, |cx| deck.read_with(cx, |d, _| d.settled().is_empty() && d.board().cards().len() == 2));
    assert_eq!(daemon.task(&older.id).unwrap().done_ms, None);
    assert!(deck.read_with(&vcx, |d, _| d.board().order().contains(&lane)));

    // A task destroyed longer ago than the column's window is absent; a
    // recent one is folded. Records keep the times.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(0))
        .unwrap();
    let day: u64 = 24 * 60 * 60 * 1000;
    let destroyed = |id: &str, at_ms: u64| -> deck_runtime::Task {
        serde_json::from_value(json!({
            "id": id, "backend": "local",
            "spec": { "argv": argv, "cwd": repo.display().to_string(), "project": project.id },
            "state": { "state": "destroyed" }, "pid": null, "destroyed_ms": at_ms,
        }))
        .unwrap()
    };
    let old = destroyed("0100-old", now - 15 * day);
    let recent = destroyed("0200-recent", now - 13 * day);
    let old_id = old.id.clone();
    let recent_id = recent.id.clone();
    let mut tasks: Vec<deck_runtime::Task> = deck.read_with(&vcx, |d, _| d.tasks().to_vec());
    tasks.push(old);
    tasks.push(recent);
    deck.update(&mut vcx, |d, cx| d.on_tasks(tasks, Default::default(), cx));
    vcx.run_until_parked();
    deck.read_with(&vcx, |d, _| {
        assert!(d.settled().iter().all(|item| item.task.id != old_id), "15 days settled is out");
        assert!(d.settled().iter().any(|item| item.task.id == recent_id), "13 days settled is in");
        assert!(d.tasks().iter().any(|task| task.id == old_id), "the record stays");
        assert_eq!(d.settled().iter().filter(|item| item.task.state == State::Destroyed).count(), 1);
    });

    cx.quit();
    for id in [&older.id, &newer.id] {
        if daemon.task(id).unwrap().state == State::Running {
            daemon.stop(id).unwrap();
        }
    }
    drop(daemon);
    cleanup_daemon(&data);
}
