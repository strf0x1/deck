//! The Deck window's root view: owns the lane board, the navigation rail, an
//! attachment per task, routes input, and drives animation frames.
//!
//! The design choice: one view entity holding plain state structs (`Board`,
//! `Attached`); render helpers in `panels` read the selected attachment.
//! Animation is pull-based:
//! each render steps the springs by the real frame gap and requests another
//! frame only while something still moves, so an idle window draws nothing.

use super::access::Access;
use super::attached::Attached;
use super::board::{Board, Lane};
use super::boundary::Boundary;
use super::composer;
use super::editor::Editor;
use super::rail::Rail;
use super::session::Session;
use super::theme::{SANS, Tokens, size};
use super::{chrome, conversation, panels};
use deck_acp::{Client, Inbound};
use deck_runtime::approvals::{Approval, Decision, State};
use gpui::{
    Context, FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseMoveEvent,
    ParentElement as _, Render, Styled as _, Subscription, Window, div, px,
};
use gpui::{FollowMode, ListState};
use std::collections::{HashMap, HashSet};
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
    /// Every task this window is attached to, by task id (ADR 0010 §8).
    pub(super) attached: HashMap<String, Attached>,
    /// The task whose transcript and composer the window shows.
    pub(super) selected: Option<String>,
    /// Where events go before any daemon task exists: a headless deck.
    pub(super) idle: Attached,
    /// The last generation stamped on a connection or an operation.
    pub(super) next_generation: u64,
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
    /// The copy button showing its check (`copy.rs`).
    pub(super) copied: Option<String>,
    /// The transcript row focused from the keyboard (alt-up/down), by key.
    pub(super) focused: Option<String>,
    /// Card bodies fading in and the jump pill (`motion.rs`).
    pub(super) motion: super::motion::Motion,
    pub(super) agent_cmd: String,
    /// The launch's agent command: a project opened from the picker uses it.
    pub(super) argv: Vec<String>,
    /// The records root this window's daemon serves, for background work.
    pub(super) data: Option<std::path::PathBuf>,
    /// The window's control connection to `deckd`, for projects and creates
    /// that outlive the selected task.
    pub(super) daemon: Option<deck_runtime::DaemonClient>,
    /// The project rail and the column's filter (ADR 0010 §1, §9).
    pub(super) projects: super::projects::Projects,
    /// A first prompt that created its task and waits for the connection.
    pub(super) pending_prompt: Option<String>,
    pub(super) _quit: Option<Subscription>,
    /// Lane ids for the daemon's tasks, the last task list, and each
    /// task's publication as `deckd` last answered it.
    pub(super) lanes: super::tasks::Lanes,
    pub(super) tasks: Vec<deck_runtime::Task>,
    pub(super) published: std::collections::HashMap<String, deck_runtime::pull_request::State>,
    pub(super) task_refresh: Option<gpui::Task<()>>,
    /// The background attach of other live tasks at launch; dropped on quit
    /// so its daemon connection cannot outlive the window.
    pub(super) attach_task: Option<gpui::Task<()>>,
    /// The task whose publication panel is open, and whether Destroy was
    /// clicked once.
    pub(super) publication_open: Option<String>,
    pub(super) destroy_confirm: bool,
    /// The strip's items, when each was first seen, and `deckd`'s pending
    /// approvals for tasks the window is not attached to (ADR 0010 §6, §7).
    pub(super) needs_you: Vec<super::attention::Item>,
    pub(super) attention_since: HashMap<String, Instant>,
    pub(super) approvals: HashMap<String, Vec<Approval>>,
    /// The highest turn number this window already reported as seen.
    pub(super) seen_sent: HashMap<String, u64>,
    /// The column's settled tasks and whether its fold is open.
    pub(super) settled: Vec<super::tasks::Settled>,
    pub(super) settled_open: bool,
    /// Why the last Mark done failed, shown in the header until the next one.
    pub(super) mark_error: Option<String>,
    /// The New session buttons: idle, creating, or the last failure.
    pub(super) new_session: super::new_session::NewSession,
    last_frame: Option<Instant>,
}

/// Before the first task list arrives the column holds only the live card.
fn lanes() -> Vec<Lane> {
    vec![live_lane()]
}

/// The selected task's card before its session says anything.
pub(super) fn live_lane() -> Lane {
    Lane { id: LIVE, title: "agent".into(), detail: "connecting…".into(), ..Lane::default() }
}

impl Deck {
    /// A Deck with no agent attached: tests feed it events with [`Deck::on_inbound`].
    pub fn detached(cx: &mut Context<Self>) -> Self {
        Deck {
            focus: cx.focus_handle(),
            board: Board::new(lanes()),
            rail: Rail::new(),
            attached: HashMap::new(),
            selected: None,
            idle: Attached::unattached(),
            next_generation: 0,
            access: Access::new(),
            access_refresh: None,
            editor: Editor::new(),
            composer: composer::Layout::default(),
            selecting: false,
            workspace: None,
            transcript: super::transcript_list::following(),
            row_sigs: Vec::new(),
            expanded: HashSet::new(),
            copied: None,
            focused: None,
            motion: super::motion::Motion::default(),
            agent_cmd: String::new(),
            argv: Vec::new(),
            data: None,
            daemon: None,
            projects: super::projects::Projects::default(),
            pending_prompt: None,
            _quit: None,
            lanes: super::tasks::Lanes::default(),
            tasks: Vec::new(),
            published: std::collections::HashMap::new(),
            task_refresh: None,
            attach_task: None,
            publication_open: None,
            destroy_confirm: false,
            needs_you: Vec::new(),
            attention_since: HashMap::new(),
            approvals: HashMap::new(),
            seen_sent: HashMap::new(),
            settled: Vec::new(),
            settled_open: false,
            mark_error: None,
            new_session: super::new_session::NewSession::Idle,
            last_frame: None,
        }
    }

    /// The attachment the window shows, or the idle one before launch.
    pub(super) fn current(&self) -> &Attached {
        self.selected.as_deref().and_then(|id| self.attached.get(id)).unwrap_or(&self.idle)
    }

    /// The attachment the window shows; mutations before launch land on the
    /// idle one, which is what a headless deck's tests read back.
    pub(super) fn current_mut(&mut self) -> &mut Attached {
        let Some(id) = self.selected.clone() else { return &mut self.idle };
        if self.attached.contains_key(&id) {
            return self.attached.get_mut(&id).unwrap_or(&mut self.idle);
        }
        &mut self.idle
    }

    fn attachment(&self, id: Option<&str>) -> Option<&Attached> {
        match id {
            Some(id) => self.attached.get(id),
            None => Some(&self.idle),
        }
    }

    fn attachment_mut(&mut self, id: Option<&str>) -> Option<&mut Attached> {
        match id {
            Some(id) => self.attached.get_mut(id),
            None => Some(&mut self.idle),
        }
    }

    /// The ids of every attached task: the window's connection per task.
    pub fn attached_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.attached.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Stamp a connection or an operation, invalidating earlier replies.
    pub(super) fn stamp(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    /// The id of the task this window runs, while it runs one.
    pub fn task_id(&self) -> Option<&str> {
        self.current().task_id()
    }

    pub fn session(&self) -> &Session {
        &self.current().session
    }

    /// The New session control's state.
    pub fn new_session_state(&self) -> &super::new_session::NewSession {
        &self.new_session
    }

    pub fn board(&self) -> &Board {
        &self.board
    }

    pub fn rail(&self) -> &Rail {
        &self.rail
    }

    /// The visible projects, most recently opened first.
    pub fn projects(&self) -> &[deck_runtime::project::Project] {
        self.projects.list()
    }

    /// The open project's id, if one is open.
    pub fn project(&self) -> Option<&str> {
        self.projects.selected_id()
    }

    /// The open project's name, for the empty state.
    pub fn project_name(&self) -> Option<&str> {
        self.projects.selected().map(|project| project.name.as_str())
    }

    /// The checkout a first prompt or New session would use.
    pub fn project_checkout(&self) -> Option<std::path::PathBuf> {
        self.projects.checkout()
    }

    /// Why the last project open failed, while the notice stands (ADR 0010 §2).
    pub fn projects_error(&self) -> Option<&str> {
        self.projects.error()
    }

    /// Whether the window launched against a daemon, as a real window does;
    /// a headless test deck has no project state to show.
    pub fn is_launched(&self) -> bool {
        self.daemon.is_some()
    }

    /// A project is open but has no task yet: the composer will create one
    /// with its first prompt (ADR 0010 §5).
    pub fn awaiting_task(&self) -> bool {
        self.task_id().is_none() && self.projects.selected().is_some()
    }

    pub fn draft(&self) -> &str {
        self.editor.text()
    }

    /// The composer's selection, as byte offsets into [`Deck::draft`].
    pub fn draft_selection(&self) -> std::ops::Range<usize> {
        self.editor.selection()
    }

    pub fn boundary(&self) -> &Boundary {
        &self.current().boundary
    }

    pub fn access(&self) -> &Access {
        &self.access
    }

    /// Apply a boundary change from the selected task's watch connection.
    pub fn on_boundary(&mut self, approval: Approval, cx: &mut Context<Self>) {
        let id = self.selected.clone();
        self.boundary_for(id.as_deref(), approval, cx);
    }

    /// Apply a boundary change to task `id`'s card, whether or not it is
    /// selected. A background task's card is drawn when it is selected.
    pub(super) fn boundary_for(&mut self, id: Option<&str>, approval: Approval, cx: &mut Context<Self>) {
        let changed = !matches!(approval.state, State::Pending);
        let (decided, client) = match self.attachment_mut(id) {
            Some(attached) => (attached.boundary.apply(approval), attached.client.clone()),
            None => return,
        };
        self.refresh_attention();
        self.sync_attention_lanes();
        cx.notify();
        if id != self.selected.as_deref() {
            return;
        }
        if let Some(decided) = decided {
            Self::notify_boundary(client.as_deref(), &decided);
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
        let Some(item) = self.current().boundary.pending().cloned() else { return };
        let Some(task) = self.current().task.clone() else { return };
        match task.decide(&item.id, decision) {
            Ok(approval) => {
                let decided = self.current_mut().boundary.apply(approval);
                if let Some(decided) = decided {
                    let client = self.current().client.clone();
                    Self::notify_boundary(client.as_deref(), &decided);
                }
            }
            Err(e) => {
                self.current_mut().session.apply(Inbound::Failed(format!("deciding boundary approval: {e:#}")));
            }
        }
        self.refresh_attention();
        self.sync_attention_lanes();
        self.sync_live_lane();
        cx.notify();
    }

    /// An agent that advertised a boundary capability (reverse-DNS, or the
    /// bare key from a released Construct) learns about a decision it did
    /// not wait for; the client ignores an agent without the capability.
    fn notify_boundary(client: Option<&Client>, approval: &Approval) {
        let Some(client) = client else { return };
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

    /// Apply an event from the selected attachment's connection. The
    /// connection pumps use [`Deck::inbound_from`] with their task id.
    pub fn on_inbound(&mut self, ev: Inbound, cx: &mut Context<Self>) {
        let selected = self.selected.clone();
        self.inbound_from(selected.as_deref(), ev, cx);
    }

    /// Apply an event to task `id`'s attachment, selected or not: what the
    /// window's other connections deliver in the background (ADR 0010 §8).
    pub fn on_inbound_for(&mut self, id: &str, ev: Inbound, cx: &mut Context<Self>) {
        self.inbound_from(Some(id), ev, cx);
    }

    /// Apply an event to task `id`'s attachment, selected or not. Only the
    /// selection re-renders, except that a background turn's state is kept.
    pub(super) fn inbound_from(&mut self, id: Option<&str>, ev: Inbound, cx: &mut Context<Self>) {
        let selected = id == self.selected.as_deref();
        let ready = matches!(&ev, Inbound::Ready { .. });
        let attention_event = matches!(
            &ev,
            Inbound::Ready { .. }
                | Inbound::Turn { .. }
                | Inbound::Permission { .. }
                | Inbound::TurnEnded(_)
                | Inbound::Failed(_)
                | Inbound::Detached
        );
        let mut turn_changed = false;
        if let Inbound::Ready { session_id, .. } = &ev
            && let Some(task) = self.attachment(id).and_then(|a| a.task.clone())
            && let Err(e) = task.record_session(session_id)
            && let Some(attached) = self.attachment_mut(id)
        {
            attached.session.apply(Inbound::Failed(format!("recording session: {e:#}")));
        }
        if ev == Inbound::Detached {
            let generation = self.stamp();
            if let Some(attached) = self.attachment_mut(id) {
                attached.generation = generation;
                attached.client = None;
            }
        }
        if let Some(attached) = self.attachment_mut(id) {
            if let Inbound::Turn { number, state } = &ev {
                let next = (*number, state.clone());
                turn_changed = attached.turn.as_ref() != Some(&next);
                attached.turn = Some(next);
            }
            // `Publishes` comes before `Ready` when the agent advertised it.
            match &ev {
                Inbound::Publishes => attached.agent_publishes = Some(true),
                Inbound::Ready { .. } if attached.agent_publishes.is_none() => {
                    attached.agent_publishes = Some(false);
                }
                _ => {}
            }
            attached.session.apply(ev);
        }
        if attention_event {
            self.refresh_attention();
            self.sync_attention_lanes();
        }
        if selected && ready {
            self.send_pending();
        }
        if selected {
            self.note_seen(cx);
            self.sync_live_lane();
        }
        if selected || turn_changed || attention_event {
            cx.notify();
        }
    }

    /// Send the first prompt now that its new task is connected.
    pub(super) fn send_pending(&mut self) {
        let Some(text) = self.pending_prompt.take() else { return };
        let prompt = self.current().client.as_ref().map(|client| client.prompt(&text));
        match prompt {
            Some(Ok(())) => {
                self.current_mut().session.sent(&text);
                // Sending is a request to see the answer: follow the tail again.
                self.transcript.set_follow_mode(FollowMode::Tail);
            }
            Some(Err(e)) => {
                self.current_mut().session.apply(Inbound::Failed(format!("sending the first prompt: {e:#}")));
                if self.editor.is_empty() {
                    self.editor.insert(&text);
                }
            }
            None => self.pending_prompt = Some(text),
        }
    }

    pub(super) fn sync_live_lane(&mut self) {
        let attached = self.current();
        let view = self.publication_of(LIVE);
        let (state, detail) = super::tasks::live(
            &attached.session.status,
            attached.session.ask.as_ref().map(|ask| ask.title.as_str()),
            attached.boundary.pending().is_some(),
            view.as_ref(),
        );
        let agent = attached.session.agent.clone();
        let chip = view.map(|v| (v.dot, v.label));
        let attention = self
            .needs_you
            .iter()
            .find(|item| Some(item.task.as_str()) == self.selected.as_deref())
            .map(|item| item.reason.text());
        if let Some(lane) = self.board.lane_mut(LIVE) {
            lane.title = agent;
            lane.state = state;
            lane.detail = detail;
            lane.chip = chip;
            lane.attention = attention;
        }
    }

    /// A project with no task yet turns the first prompt into its task
    /// (ADR 0010 §5); `attachment.rs` creates it, then [`Deck::send_pending`]
    /// sends the text once the connection is ready.
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.text().trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.task_id().is_none() {
            // A selected project with no task creates one (ADR 0010 §5); a
            // headless deck's idle attachment still takes the prompt.
            if self.projects.selected().is_some() && self.current().client.is_none() {
                return self.first_prompt(text, cx);
            }
        }
        if self.pending_prompt.is_some() || !self.current().session.can_prompt() {
            return;
        }
        if let Some(client) = self.current().client.clone()
            && let Err(e) = client.prompt(&text)
        {
            self.current_mut().session.apply(Inbound::Failed(format!("{e:#}")));
            return cx.notify();
        }
        self.editor.take();
        self.current_mut().session.sent(&text);
        self.sync_live_lane();
        // Sending is a request to see the answer: follow the tail again.
        self.transcript.set_follow_mode(FollowMode::Tail);
        cx.notify();
    }

    /// Answer the pending permission question with option `index`, or cancel it.
    pub fn choose(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        let client = self.current().client.clone();
        let Some(ask) = self.current_mut().session.answered() else { return };
        let option = index.and_then(|i| ask.options.get(i)).map(|o| o.id.as_str());
        if let Some(client) = client
            && let Err(e) = client.answer(&ask.request, option)
        {
            self.current_mut().session.apply(Inbound::Failed(format!("{e:#}")));
        }
        self.refresh_attention();
        self.sync_attention_lanes();
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
        if self.current().session.ask.is_some() && !self.current().boundary.is_reviewing() {
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
        if self.current().boundary.pending().is_some() {
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
        self.board.dragging().is_some() || board || rail || !self.motion.at_rest()
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
        let motion = self.motion.step(dt);
        if board || rail || motion {
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
        self.sync_transcript();
        // Aim the pill before stepping, so it starts moving this frame.
        self.motion.pill_to(self.scrolled_back());
        self.tick(window);
        div()
            .id("deck")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &crate::app::RestartTask, _, cx| this.restart(cx)))
            .on_action(cx.listener(|this, _: &crate::app::StopTask, _, cx| this.stop(cx)))
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
            .text_size(px(size::BODY))
            .text_color(Tokens::text())
            .child(div().flex_none().w(px(chrome::RAIL_SLOT)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(GUTTER))
                    .child(super::needs_you::strip(self, cx))
                    .child(chrome::header(self, cx))
                    .child(
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
