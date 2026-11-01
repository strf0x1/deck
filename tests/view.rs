//! The Deck view under GPUI's headless test platform: real layout, real
//! input dispatch, no window server. This is how an agent proves a UI change
//! without looking at the screen.
#![expect(clippy::unwrap_used, reason = "setup helpers outside #[test] fns; a failure is a test failure")]

use deck::acp::{Inbound, PermissionOption};
use deck::ui::board::PITCH;
use deck::ui::root::Deck;
use deck::ui::session::{Entry, Status};
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
        d.on_inbound(Inbound::Ready { agent: "construct".into(), session_id: "s".into() }, cx)
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
fn dragging_a_card_with_the_mouse_reorders_lanes(cx: &mut TestAppContext) {
    let (deck, mut cx) = open(cx);
    cx.run_until_parked();
    let before = deck.read_with(&cx, |d, _| d.board().order());
    assert_eq!(before, vec![0, 1, 2, 3]);

    // The first card sits under the LANES label in the left panel.
    let start = point(px(150.), px(110.));
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
