//! Connecting the window to daemon-owned tasks, switching the selection, and
//! attaching the other running tasks at launch (ADR 0010 §8). Everything that
//! talks to the daemon runs off the UI thread, and every reply carries the
//! generation it was asked with.
//!
//! The design choice: each task's connection state lives in its own
//! [`Attached`] ([`super::attached`]), so switching only moves `selected`.
//! Replacing a connection bumps that attachment's generation; a slower,
//! earlier reply applies only while it is still the generation asked.

use super::attached::Attached;
use super::new_session::NewSession;
use super::root::{Deck, LIVE};
use super::session::{Session, Status};
use crate::task::{Connection, WindowTask};
use deck_acp::Inbound;
use deck_runtime::approvals::Approval;
use deck_runtime::{DaemonClient, State, Task, Watch};
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::Context;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// How many other running tasks launch attaches at once.
const LAUNCH_ATTACH: usize = 4;

impl Deck {
    /// Launch with records under the default data root.
    pub fn launch(argv: Vec<String>, cwd: PathBuf, cx: &mut Context<Self>) -> Self {
        match deck_runtime::data_root() {
            Ok(data) => Self::launch_in(&data, argv, cwd, cx),
            Err(e) => {
                let mut deck = Self::detached(cx);
                deck.idle.session.apply(Inbound::Failed(format!("{e:#}")));
                deck
            }
        }
    }

    /// Find or start the first task without taking ownership of its lifetime,
    /// then attach every other running task that recorded a session.
    pub fn launch_in(data: &Path, argv: Vec<String>, cwd: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut deck = Self::detached(cx);
        deck.agent_cmd = argv.join(" ");
        deck.data = Some(data.to_path_buf());
        match WindowTask::run(data, argv, cwd) {
            Ok((task, connection)) => {
                let id = task.id().to_string();
                let watch = task.watch();
                let generation = deck.stamp();
                let mut attached = Attached::new(task);
                attached.generation = generation;
                deck.attached.insert(id.clone(), attached);
                deck.selected = Some(id.clone());
                deck.follow_boundary(&id, watch, cx);
                deck.follow_tasks(cx);
                deck._quit = Some(cx.on_app_quit(|deck, _| {
                    // Dropping every attachment detaches; it never stops an
                    // agent (ADR 0002). Drop the watches before the entity can
                    // outlive the app, so their readers close the daemon
                    // connections.
                    deck.attached.clear();
                    deck.selected = None;
                    deck.access_refresh = None;
                    deck.task_refresh = None;
                    deck.attach_task = None;
                    async {}
                }));
                match connection {
                    Some(connection) => deck.connect(&id, connection, cx),
                    None => {
                        if let Some(attached) = deck.attached.get_mut(&id) {
                            attached.session.status = Status::Detached;
                        }
                    }
                }
                deck.attach_live(cx);
            }
            Err(e) => deck.idle.session.apply(Inbound::Failed(format!("{e:#}"))),
        }
        deck.sync_live_lane();
        deck
    }

    /// A detached or failed connection can be rebuilt by restarting its task.
    pub fn can_restart(&self) -> bool {
        self.current().task.is_some() && matches!(self.current().session.status, Status::Detached | Status::Down(_))
    }

    /// Explicitly interrupt the selected task and load its recorded history.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        if !self.can_restart() {
            return;
        }
        let Some(task) = self.current().task.clone() else { return };
        let id = task.id().to_string();
        let generation = self.replace_connection(&id);
        self.current_mut().session.status = Status::Connecting;
        self.sync_live_lane();
        cx.notify();
        let restarted = cx.background_executor().spawn(async move { task.restart() });
        cx.spawn(async move |this, cx| {
            let result = restarted.await;
            let _ = this.update(cx, |deck, cx| {
                // A switch, restart or quit since then owns the window now.
                if deck.attached.get(&id).map(|a| a.generation) != Some(generation) {
                    return;
                }
                match result {
                    Ok(connection) => deck.connect(&id, connection, cx),
                    Err(e) => {
                        if let Some(attached) = deck.attached.get_mut(&id) {
                            attached.session.apply(Inbound::Failed(format!("restarting task: {e:#}")));
                        }
                    }
                }
                deck.sync_live_lane();
                cx.notify();
            });
        })
        .detach();
    }

    /// Whether the selected task is connected and not already down.
    pub fn can_stop(&self) -> bool {
        self.current().task.is_some() && !matches!(self.current().session.status, Status::Down(_))
    }

    /// Stop the selected task and leave it stopped. Its connection is dropped
    /// and its generation bumped; a restart card replaces the conversation.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if !self.can_stop() {
            return;
        }
        let Some(task) = self.current().task.clone() else { return };
        let id = task.id().to_string();
        let generation = self.replace_connection(&id);
        self.current_mut().session.status = Status::Connecting;
        self.sync_live_lane();
        cx.notify();
        let stopped = cx.background_executor().spawn(async move { task.stop() });
        cx.spawn(async move |this, cx| {
            let result = stopped.await;
            let _ = this.update(cx, |deck, cx| {
                if deck.attached.get(&id).map(|a| a.generation) != Some(generation) {
                    return;
                }
                match result {
                    Ok(()) => {
                        if let Some(attached) = deck.attached.get_mut(&id) {
                            attached.session.status = Status::Down("stopped".into());
                        }
                    }
                    Err(e) => {
                        if let Some(attached) = deck.attached.get_mut(&id) {
                            attached.session.apply(Inbound::Failed(format!("stopping task: {e:#}")));
                        }
                    }
                }
                deck.sync_live_lane();
                cx.notify();
            });
        })
        .detach();
    }

    /// Switch the window to the task card `lane` shows. Every attached task
    /// keeps running and keeps its state; only `selected` moves.
    pub fn select(&mut self, lane: usize, cx: &mut Context<Self>) {
        let Some(current) = self.current().task.clone() else { return };
        let Some(id) = self.lanes.task(lane, Some(current.id())).filter(|_| lane != LIVE) else { return };
        let Some(record) = self.tasks.iter().find(|t| t.id == id).cloned() else { return };
        // The previously selected card takes the selected one's slot.
        self.lanes.switch(Some(current.id()), lane);
        self.select_task(&current, record, cx);
    }

    /// Start a new session: another task with this window's command and
    /// directory, then select it like a card. One at a time; a failed create
    /// leaves the window where it was and says why in the header.
    pub fn new_session(&mut self, cx: &mut Context<Self>) {
        if self.new_session.is_creating() {
            return;
        }
        let Some(current) = self.current().task.clone() else { return };
        let from = current.id().to_string();
        let generation = self.current().generation;
        self.new_session = NewSession::Creating;
        cx.notify();
        let created = cx.background_executor().spawn({
            let current = current.clone();
            async move { current.create_sibling() }
        });
        cx.spawn(async move |this, cx| {
            let created = created.await;
            let _ = this.update(cx, |deck, cx| {
                let unchanged =
                    deck.selected.as_deref() == Some(from.as_str()) && deck.current().generation == generation;
                match created {
                    // A switch, restart or quit since the click owns the
                    // window: the new task stays a card for the next refresh.
                    Ok(_) if !unchanged => deck.new_session = NewSession::Idle,
                    Ok(record) => {
                        deck.new_session = NewSession::Idle;
                        deck.tasks.push(record.clone());
                        deck.select_task(&current, record, cx);
                    }
                    Err(e) => deck.new_session = NewSession::Failed(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Make `record` the selection and reset what belongs to the previous
    /// one. Connect it only if it has no attachment yet.
    fn select_task(&mut self, current: &WindowTask, record: Task, cx: &mut Context<Self>) {
        let id = record.id.clone();
        // Insert first, so `current()` resolves while the lanes rebuild: a
        // missing attachment would make the target an ordinary card too.
        let next = (!self.attached.contains_key(&id)).then(|| current.sibling(&record));
        let generation = self.stamp();
        if let Some(next) = &next {
            self.attached.insert(id.clone(), Attached { generation, ..Attached::new(next.clone()) });
        }
        self.new_session = NewSession::Idle;
        self.selected = Some(id.clone());
        self.expanded.clear();
        self.row_sigs.clear();
        self.transcript = super::transcript_list::following();
        self.editor = super::editor::Editor::new();
        self.workspace = self.tasks.iter().find(|t| t.id == id).map(super::prompt::Workspace::of);
        if self.access.is_open() {
            self.toggle_access(cx);
        }
        let (tasks, published) = (self.tasks.clone(), std::mem::take(&mut self.published));
        self.on_tasks(tasks, published, cx);
        let Some(next) = next else { return };
        let opened = cx.background_executor().spawn(async move { (next.watch(), next.open()) });
        cx.spawn(async move |this, cx| {
            let (watch, connection) = opened.await;
            let _ = this.update(cx, |deck, cx| {
                // Another switch, a restart or quit has replaced this one;
                // dropping the results detaches from the task again.
                if deck.attached.get(&id).map(|a| a.generation) != Some(generation) {
                    return;
                }
                deck.follow_boundary(&id, watch, cx);
                match connection {
                    Ok(Some(connection)) => deck.connect(&id, connection, cx),
                    Ok(None) => {
                        if let Some(attached) = deck.attached.get_mut(&id) {
                            attached.session.status = Status::Detached;
                        }
                    }
                    Err(e) => {
                        if let Some(attached) = deck.attached.get_mut(&id) {
                            attached.session.apply(Inbound::Failed(format!("opening task: {e:#}")));
                        }
                    }
                }
                deck.sync_live_lane();
                cx.notify();
            });
        })
        .detach();
    }

    /// Attach, in the background, every other task that is running and whose
    /// record has a session. Nothing is started, resumed or restarted.
    fn attach_live(&mut self, cx: &mut Context<Self>) {
        let Some(data) = self.data.clone() else { return };
        let Some(own_id) = self.task_id().map(str::to_string) else { return };
        let known: Vec<String> = self.attached.keys().cloned().collect();
        // Only the data root crosses into the background task, not a daemon
        // handle: a queued task that the window drops before it runs would
        // otherwise keep a control connection open and delay daemon exit.
        let opened = cx.background_executor().spawn(async move {
            let daemon = DaemonClient::connect(&data)?;
            let live: Vec<Task> = daemon
                .tasks()?
                .into_iter()
                .filter(|t| {
                    t.id != own_id && t.state == State::Running && t.session_id.is_some() && !known.contains(&t.id)
                })
                .collect();
            let mut out = Vec::new();
            for chunk in live.chunks(LAUNCH_ATTACH) {
                let mut batch = Vec::new();
                for record in chunk {
                    let task = WindowTask::of(daemon.clone(), record);
                    let watch = task.watch();
                    let connection = task.attach_live();
                    batch.push((record.id.clone(), task, watch, connection));
                }
                out.extend(batch);
            }
            anyhow::Ok(out)
        });
        self.attach_task = Some(cx.spawn(async move |this, cx| {
            let Ok(opened) = opened.await else { return };
            let _ = this.update(cx, |deck, cx| {
                for (id, task, watch, connection) in opened {
                    if deck.attached.contains_key(&id) {
                        continue;
                    }
                    let Ok(Some(connection)) = connection else { continue };
                    let generation = deck.stamp();
                    deck.attached.insert(id.clone(), Attached { generation, ..Attached::new(task) });
                    deck.follow_boundary(&id, watch, cx);
                    deck.connect(&id, connection, cx);
                }
                cx.notify();
            });
        }));
    }

    /// Drop `id`'s connection and invalidate replies asked before now. The
    /// attachment, its session and its approval lease stay.
    fn replace_connection(&mut self, id: &str) -> u64 {
        let generation = self.stamp();
        if let Some(attached) = self.attached.get_mut(id) {
            attached.generation = generation;
            attached.client = None;
        }
        generation
    }

    /// Follow the daemon's approval queue for task `id`. The watch connection
    /// is the attachment lease; dropping it detaches. Approvals already in
    /// flight from an attachment that was dropped are ignored.
    fn follow_boundary(
        &mut self,
        id: &str,
        watch: anyhow::Result<(Watch, UnboundedReceiver<Approval>)>,
        cx: &mut Context<Self>,
    ) {
        let (watch, mut rx) = match watch {
            Ok(watch) => watch,
            Err(e) => {
                if let Some(attached) = self.attached.get_mut(id) {
                    attached.session.apply(Inbound::Failed(format!("watching task approvals: {e:#}")));
                }
                return;
            }
        };
        let Some(attached) = self.attached.get_mut(id) else { return };
        attached.watch = Some(watch);
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            while let Some(approval) = rx.next().await {
                let applied = this.update(cx, |deck, cx| {
                    if !deck.attached.contains_key(&id) {
                        return false;
                    }
                    deck.boundary_for(Some(&id), approval, cx);
                    true
                });
                if !matches!(applied, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Start pumping a connection into task `id`'s attachment. The event is
    /// applied only while this is still the connection's generation.
    fn connect(&mut self, id: &str, (client, mut rx): Connection, cx: &mut Context<Self>) {
        let generation = self.replace_connection(id);
        let Some(attached) = self.attached.get_mut(id) else { return };
        attached.session = Session::new();
        attached.agent_publishes = None;
        attached.client = Some(Arc::new(client));
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.next().await {
                let applied = this.update(cx, |deck, cx| {
                    if deck.attached.get(&id).map(|a| a.generation) != Some(generation) {
                        return false;
                    }
                    deck.inbound_from(Some(&id), ev, cx);
                    true
                });
                if !matches!(applied, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }
}
