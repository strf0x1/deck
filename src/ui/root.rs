//! The Deck window's root view: owns the lane board, the navigation rail, the
//! live session and the agent client, routes input, and drives animation frames.
//!
//! The design choice: one view entity holding plain state structs (`Board`,
//! `Session`); render helpers in `panels` read them. Animation is pull-based:
//! each render steps the springs by the real frame gap and requests another
//! frame only while something still moves, so an idle window draws nothing.

use super::board::{Board, Lane, LaneState};
use super::rail::Rail;
use super::session::Session;
use super::theme::{SANS, Tokens};
use super::{chrome, conversation, panels};
use crate::acp::{Client, Inbound};
use futures::StreamExt as _;
use gpui::{
    Context, FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseMoveEvent,
    ParentElement as _, Render, ScrollHandle, Styled as _, Window, div, px,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// The lane id of the live agent session; the others are placeholders until
/// the host runtime exists.
pub const LIVE: usize = 0;

/// Space around and between the window's regions.
const GUTTER: f32 = 12.;

pub struct Deck {
    pub(super) focus: FocusHandle,
    pub(super) board: Board,
    pub(super) rail: Rail,
    pub(super) session: Session,
    pub(super) draft: String,
    pub(super) scroll: ScrollHandle,
    pub(super) agent_cmd: String,
    client: Option<Arc<Client>>,
    last_frame: Option<Instant>,
}

fn lanes() -> Vec<Lane> {
    let lane = |id, title: &str, detail: &str, state| Lane { id, title: title.into(), detail: detail.into(), state };
    vec![
        lane(LIVE, "agent", "connecting…", LaneState::Idle),
        lane(1, "flaky test hunt", "vm · placeholder", LaneState::Working),
        lane(2, "auth refactor", "vm · placeholder", LaneState::Review),
        lane(3, "scratch", "placeholder", LaneState::Idle),
    ]
}

impl Deck {
    /// A Deck with no agent attached: tests feed it events with [`Deck::on_inbound`].
    pub fn detached(cx: &mut Context<Self>) -> Self {
        Deck {
            focus: cx.focus_handle(),
            board: Board::new(lanes()),
            rail: Rail::new(),
            session: Session::new(),
            draft: String::new(),
            scroll: ScrollHandle::new(),
            agent_cmd: String::new(),
            client: None,
            last_frame: None,
        }
    }

    /// A Deck driving the agent `argv` in `cwd`.
    pub fn launch(argv: Vec<String>, cwd: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut deck = Deck::detached(cx);
        deck.agent_cmd = argv.join(" ");
        match Client::spawn(&argv, cwd) {
            Ok((client, mut rx)) => {
                deck.client = Some(Arc::new(client));
                cx.spawn(async move |this, cx| {
                    while let Some(ev) = rx.next().await {
                        if this.update(cx, |deck, cx| deck.on_inbound(ev, cx)).is_err() {
                            break;
                        }
                    }
                })
                .detach();
            }
            Err(e) => deck.session.apply(Inbound::Failed(format!("{e:#}"))),
        }
        deck
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn board(&self) -> &Board {
        &self.board
    }

    pub fn rail(&self) -> &Rail {
        &self.rail
    }

    pub fn draft(&self) -> &str {
        &self.draft
    }

    pub fn on_inbound(&mut self, ev: Inbound, cx: &mut Context<Self>) {
        self.session.apply(ev);
        self.sync_live_lane();
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn sync_live_lane(&mut self) {
        use super::session::Status;
        let (state, detail) = match (&self.session.status, &self.session.ask) {
            (_, Some(ask)) => (LaneState::Waiting, format!("asks: {}", ask.title)),
            (Status::Connecting, _) => (LaneState::Idle, "connecting…".into()),
            (Status::Ready, _) => (LaneState::Idle, "ready".into()),
            (Status::Working, _) => (LaneState::Working, "working".into()),
            (Status::Down(e), _) => (LaneState::Idle, e.clone()),
        };
        let agent = self.session.agent.clone();
        if let Some(lane) = self.board.lane_mut(LIVE) {
            lane.title = agent;
            lane.state = state;
            lane.detail = detail;
        }
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.draft.trim().to_string();
        if text.is_empty() || !self.session.can_prompt() {
            return;
        }
        if let Some(client) = &self.client
            && let Err(e) = client.prompt(&text)
        {
            self.session.apply(Inbound::Failed(format!("{e:#}")));
            return cx.notify();
        }
        self.draft.clear();
        self.session.sent(&text);
        self.sync_live_lane();
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// Answer the pending permission question with option `index`, or cancel it.
    pub fn choose(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        let Some(ask) = self.session.answered() else { return };
        let option = index.and_then(|i| ask.options.get(i)).map(|o| o.id.as_str());
        if let Some(client) = &self.client
            && let Err(e) = client.answer(&ask.request, option)
        {
            self.session.apply(Inbound::Failed(format!("{e:#}")));
        }
        self.sync_live_lane();
        cx.notify();
    }

    fn key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if self.session.ask.is_some() {
            match k.key.as_str() {
                "escape" => self.choose(None, cx),
                d if d.len() == 1 && d.chars().all(|c| c.is_ascii_digit()) && d != "0" => {
                    let i = d.parse::<usize>().unwrap_or(1) - 1;
                    self.choose(Some(i), cx);
                }
                _ => {}
            }
            return;
        }
        match k.key.as_str() {
            "enter" => self.submit(cx),
            "backspace" => {
                self.draft.pop();
                cx.notify();
            }
            "escape" => {
                if let Some(c) = &self.client {
                    let _ = c.cancel();
                }
            }
            _ if k.modifiers.control || k.modifiers.platform || k.modifiers.alt => {}
            _ => {
                if let Some(ch) = &k.key_char {
                    self.draft.push_str(ch);
                    cx.notify();
                }
            }
        }
    }

    /// Whether a drag is in progress or any spring is still moving.
    pub fn animating(&self) -> bool {
        let board = self.board.cards().iter().any(|c| !c.y.at_rest() || !c.lift.at_rest());
        let rail = !self.rail.width.at_rest() || !self.rail.marker.at_rest();
        self.board.dragging().is_some() || board || rail
    }

    /// The centre of rail item `index` while the rail is closed, in window coordinates.
    pub fn rail_item_center(index: usize) -> (f32, f32) {
        chrome::rail_item_center(index, GUTTER)
    }

    pub(super) fn grab(&mut self, id: usize, y: f32, cx: &mut Context<Self>) {
        self.board.grab(id, y);
        cx.notify();
    }

    fn tick(&mut self, window: &mut Window) {
        let now = Instant::now();
        let dt = self.last_frame.map_or(0., |t| now.duration_since(t).as_secs_f32());
        let board = self.board.step(dt);
        let rail = self.rail.step(dt);
        if board || rail {
            self.last_frame = Some(now);
            window.request_animation_frame();
        } else {
            self.last_frame = None;
        }
    }
}

impl Focusable for Deck {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Deck {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tick(window);
        div()
            .id("deck")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev, _, cx| this.key(ev, cx)))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if this.board.dragging().is_some() {
                    this.board.move_to(f32::from(ev.position.y));
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.board.release();
                    cx.notify();
                }),
            )
            .relative()
            .size_full()
            .flex()
            .gap(px(GUTTER))
            .p(px(GUTTER))
            .bg(Tokens::base())
            .font_family(SANS)
            .text_size(px(13.5))
            .text_color(Tokens::text())
            .child(div().flex_none().w(px(chrome::RAIL_SLOT)))
            .child(
                div().flex_1().min_w_0().flex().flex_col().gap(px(GUTTER)).child(chrome::header(self)).child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .gap(px(GUTTER))
                        .child(panels::lanes(self, cx))
                        .child(conversation::main(self, cx)),
                ),
            )
            // Last, so the open rail floats over the sessions column.
            .child(chrome::rail(self, GUTTER, cx))
    }
}
