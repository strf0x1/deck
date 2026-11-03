//! The Deck window's root view: owns the lane board, the navigation rail, the
//! live session and the agent client, routes input, and drives animation frames.
//!
//! The design choice: one view entity holding plain state structs (`Board`,
//! `Session`); render helpers in `panels` read them. Animation is pull-based:
//! each render steps the springs by the real frame gap and requests another
//! frame only while something still moves, so an idle window draws nothing.

use super::board::{Board, Lane, LaneState};
use super::boundary::Boundary;
use super::rail::Rail;
use super::session::{Session, Status};
use super::theme::{SANS, Tokens};
use super::{chrome, conversation, panels};
use crate::task::WindowTask;
use deck_acp::{Client, Inbound};
use deck_runtime::Watch;
use deck_runtime::approvals::{Approval, Decision, State};
use gpui::{
    Context, FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseMoveEvent,
    ParentElement as _, Render, ScrollHandle, Styled as _, Subscription, Window, div, px,
};
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
    pub(super) boundary: Boundary,
    pub(super) draft: String,
    pub(super) scroll: ScrollHandle,
    pub(super) agent_cmd: String,
    /// A connection to the daemon-owned task. Window shutdown detaches it.
    pub(super) task: Option<WindowTask>,
    /// The boundary watch connection; dropping it ends the window's lease.
    pub(super) watch: Option<Watch>,
    pub(super) _quit: Option<Subscription>,
    pub(super) client: Option<Arc<Client>>,
    pub(super) attachment: u64,
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
            boundary: Boundary::new(),
            draft: String::new(),
            scroll: ScrollHandle::new(),
            agent_cmd: String::new(),
            task: None,
            watch: None,
            _quit: None,
            client: None,
            attachment: 0,
            last_frame: None,
        }
    }

    /// The id of the task this window runs, while it runs one.
    pub fn task_id(&self) -> Option<&str> {
        self.task.as_ref().map(WindowTask::id)
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

    pub fn boundary(&self) -> &Boundary {
        &self.boundary
    }

    /// Apply a boundary change from the daemon's watch connection.
    pub fn on_boundary(&mut self, approval: Approval, cx: &mut Context<Self>) {
        if let Some(decided) = self.boundary.apply(approval) {
            self.notify_boundary(&decided);
        }
        self.sync_live_lane();
        cx.notify();
    }

    /// Send the card's decision. The card closes on the daemon's reply and
    /// on its next watch update; a failed decision leaves it open.
    pub fn decide_boundary(&mut self, decision: Decision, cx: &mut Context<Self>) {
        let Some(item) = self.boundary.pending().cloned() else { return };
        let Some(task) = self.task.clone() else { return };
        match task.decide(&item.id, decision) {
            Ok(approval) => {
                if let Some(decided) = self.boundary.apply(approval) {
                    self.notify_boundary(&decided);
                }
            }
            Err(e) => self.session.apply(Inbound::Failed(format!("deciding boundary approval: {e:#}"))),
        }
        self.sync_live_lane();
        cx.notify();
    }

    /// An agent that advertised a boundary capability (reverse-DNS, or the
    /// bare key from a released Construct) learns about a decision it did
    /// not wait for; the client ignores an agent without the capability.
    fn notify_boundary(&self, approval: &Approval) {
        let Some(client) = &self.client else { return };
        let mut notice = serde_json::json!({
            "approval_id": approval.id,
            "destination": approval.destination,
        });
        match &approval.state {
            State::Allowed(scope) => {
                notice["outcome"] = serde_json::json!("allowed");
                notice["scope"] = serde_json::json!(scope.as_str());
            }
            State::Denied => notice["outcome"] = serde_json::json!("denied"),
            _ => return,
        }
        let _ = client.boundary_notice(&notice);
    }

    pub fn on_inbound(&mut self, ev: Inbound, cx: &mut Context<Self>) {
        if let Inbound::Ready { session_id, .. } = &ev
            && let Some(task) = &self.task
            && let Err(e) = task.record_session(session_id)
        {
            self.session.apply(Inbound::Failed(format!("recording session: {e:#}")));
        }
        if ev == Inbound::Detached {
            self.attachment += 1;
            self.client = None;
        }
        self.session.apply(ev);
        self.sync_live_lane();
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    pub(super) fn sync_live_lane(&mut self) {
        let (state, detail) = match (&self.session.status, &self.session.ask) {
            _ if self.boundary.pending().is_some() => (LaneState::Waiting, "asks: external access".into()),
            (_, Some(ask)) => (LaneState::Waiting, format!("asks: {}", ask.title)),
            (Status::Connecting, _) => (LaneState::Idle, "connecting…".into()),
            (Status::Ready, _) => (LaneState::Idle, "ready".into()),
            (Status::Waiting, _) => (LaneState::Waiting, "waiting for permission".into()),
            (Status::Working, _) => (LaneState::Working, "working".into()),
            (Status::Detached, _) => (LaneState::Working, "running · detached".into()),
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
        if self.boundary.pending().is_some() {
            match k.key.as_str() {
                "1" => self.decide_boundary(Decision::Once, cx),
                "2" => self.decide_boundary(Decision::Task, cx),
                "3" => self.decide_boundary(Decision::Project, cx),
                "4" | "escape" => self.decide_boundary(Decision::Deny, cx),
                _ => {}
            }
            return;
        }
        if self.can_restart() {
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
            .on_action(cx.listener(|this, _: &crate::app::RestartTask, _, cx| this.restart(cx)))
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
