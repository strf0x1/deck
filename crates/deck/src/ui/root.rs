//! The Deck window's root view: owns the lane board, the navigation rail, the
//! live session and the agent client, routes input, and drives animation frames.
//!
//! The design choice: one view entity holding plain state structs (`Board`,
//! `Session`); render helpers in `panels` read them. Animation is pull-based:
//! each render steps the springs by the real frame gap and requests another
//! frame only while something still moves, so an idle window draws nothing.

use super::access::Access;
use super::board::{Board, Lane, LaneState};
use super::boundary::Boundary;
use super::composer;
use super::editor::Editor;
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
    ParentElement as _, Render, Styled as _, Subscription, Window, div, px,
};
use gpui::{FollowMode, ListState};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

/// The lane id of this window's own task, the live agent session. Every other
/// task `deckd` knows gets its own lane (`tasks.rs`).
pub const LIVE: usize = 0;

/// Space around and between the window's regions.
const GUTTER: f32 = 12.;

pub struct Deck {
    pub(super) focus: FocusHandle,
    pub(super) board: Board,
    pub(super) rail: Rail,
    pub(super) session: Session,
    pub(super) boundary: Boundary,
    pub(super) access: Access,
    pub(super) access_refresh: Option<gpui::Task<()>>,
    pub(super) editor: Editor,
    /// What the composer drew last, for hit tests and up/down moves.
    pub(super) composer: composer::Layout,
    /// A mouse press in the composer is selecting text.
    pub(super) selecting: bool,
    /// The window's task as `deckd` last listed it, for the status row.
    pub(super) workspace: Option<super::prompt::Workspace>,
    /// The transcript's virtualized list, the signature of each row it
    /// last measured, and the rows the user expanded (`transcript.rs`).
    pub(super) transcript: ListState,
    pub(super) row_sigs: Vec<u64>,
    pub(super) expanded: HashSet<String>,
    pub(super) agent_cmd: String,
    /// A connection to the daemon-owned task. Window shutdown detaches it.
    pub(super) task: Option<WindowTask>,
    /// The boundary watch connection; dropping it ends the window's lease.
    pub(super) watch: Option<Watch>,
    pub(super) _quit: Option<Subscription>,
    pub(super) client: Option<Arc<Client>>,
    pub(super) attachment: u64,
    /// Lane ids for the daemon's tasks, the last task list, and each
    /// task's publication as `deckd` last answered it.
    pub(super) lanes: super::tasks::Lanes,
    pub(super) tasks: Vec<deck_runtime::Task>,
    pub(super) published: std::collections::HashMap<String, deck_runtime::pull_request::State>,
    pub(super) task_refresh: Option<gpui::Task<()>>,
    /// The task whose publication panel is open, and whether Destroy was
    /// clicked once.
    pub(super) publication_open: Option<String>,
    pub(super) destroy_confirm: bool,
    /// Whether the agent advertised GitHub operations; `None` until ready.
    pub(super) agent_publishes: Option<bool>,
    last_frame: Option<Instant>,
}

/// Before the first task list arrives the column holds only the live card.
fn lanes() -> Vec<Lane> {
    vec![Lane { id: LIVE, title: "agent".into(), detail: "connecting…".into(), ..Lane::default() }]
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
            access: Access::new(),
            access_refresh: None,
            editor: Editor::new(),
            composer: composer::Layout::default(),
            selecting: false,
            workspace: None,
            transcript: super::transcript_list::following(),
            row_sigs: Vec::new(),
            expanded: HashSet::new(),
            agent_cmd: String::new(),
            task: None,
            watch: None,
            _quit: None,
            client: None,
            attachment: 0,
            lanes: super::tasks::Lanes::default(),
            tasks: Vec::new(),
            published: std::collections::HashMap::new(),
            task_refresh: None,
            publication_open: None,
            destroy_confirm: false,
            agent_publishes: None,
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
        self.editor.text()
    }

    /// The composer's selection, as byte offsets into [`Deck::draft`].
    pub fn draft_selection(&self) -> std::ops::Range<usize> {
        self.editor.selection()
    }

    pub fn boundary(&self) -> &Boundary {
        &self.boundary
    }

    pub fn access(&self) -> &Access {
        &self.access
    }

    /// Apply a boundary change from the daemon's watch connection.
    pub fn on_boundary(&mut self, approval: Approval, cx: &mut Context<Self>) {
        let changed = !matches!(approval.state, State::Pending);
        if let Some(decided) = self.boundary.apply(approval) {
            self.notify_boundary(&decided);
        }
        if changed && self.access.is_open() {
            self.load_grants(cx);
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
                if let Some(expires_at_ms) = approval.grant_expires_ms {
                    notice["expires_at_ms"] = serde_json::json!(expires_at_ms);
                }
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
        // `Publishes` comes before `Ready` when the agent advertised it.
        match ev {
            Inbound::Publishes => self.agent_publishes = Some(true),
            Inbound::Ready { .. } if self.agent_publishes.is_none() => self.agent_publishes = Some(false),
            _ => {}
        }
        self.session.apply(ev);
        self.sync_live_lane();
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
        let chip = self.publication_of(LIVE).map(|v| (v.dot, v.label));
        if let Some(lane) = self.board.lane_mut(LIVE) {
            lane.title = agent;
            lane.state = state;
            lane.detail = detail;
            lane.chip = chip;
        }
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.text().trim().to_string();
        if text.is_empty() || !self.session.can_prompt() {
            return;
        }
        if let Some(client) = &self.client
            && let Err(e) = client.prompt(&text)
        {
            self.session.apply(Inbound::Failed(format!("{e:#}")));
            return cx.notify();
        }
        self.editor.take();
        self.session.sent(&text);
        self.sync_live_lane();
        // Sending is a request to see the answer: follow the tail again.
        self.transcript.set_follow_mode(FollowMode::Tail);
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

    /// Keys for whatever holds the keyboard: an open panel, a question, a
    /// boundary card, else the composer. Anything but the composer stops the
    /// key here, so typed text never reaches the field behind a card.
    fn key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        // While Access is open, hidden prompt and approval shortcuts cannot
        // consume typing. Review returns to the existing boundary card.
        if self.access.is_open() {
            cx.stop_propagation();
            if k.key == "escape" && !self.access.cancel() {
                self.toggle_access(cx);
            }
            return cx.notify();
        }
        if self.publication_open.is_some() && k.key == "escape" {
            cx.stop_propagation();
            return self.close_publication(cx);
        }
        if self.session.ask.is_some() && !self.boundary.is_reviewing() {
            cx.stop_propagation();
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
            cx.stop_propagation();
            match k.key.as_str() {
                "1" => self.decide_boundary(Decision::Once, cx),
                "2" => self.decide_boundary(Decision::Timed, cx),
                "3" => self.decide_boundary(Decision::Task, cx),
                "4" => self.decide_boundary(Decision::Project, cx),
                "5" | "escape" => self.decide_boundary(Decision::Deny, cx),
                _ => {}
            }
            return;
        }
        if self.can_restart() {
            return cx.stop_propagation();
        }
        if self.edit(ev, cx) {
            cx.stop_propagation();
            cx.notify();
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
        self.sync_transcript();
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
                    this.selecting = false;
                    // A press that never became a drag selects the card.
                    if let Some(lane) = this.board.release()
                        && lane != LIVE
                    {
                        this.select(lane, cx);
                    }
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
                div().flex_1().min_w_0().flex().flex_col().gap(px(GUTTER)).child(chrome::header(self, cx)).child(
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
