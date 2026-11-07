//! Connecting the window to its daemon-owned task, and switching it to
//! another one. Restart and switch run outside the UI thread, and every new
//! connection rebuilds Session before replay starts.
//!
//! The design choice: [`Deck::attachment`] is the generation of the window's
//! connection. Anything that replaces or drops the connection bumps it, and
//! a reply from a background restart or switch applies only while it is the
//! generation that started it, so a slower, earlier request cannot land on a
//! later task.

use super::boundary::Boundary;
use super::new_session::NewSession;
use super::root::{Deck, LIVE};
use super::session::{Session, Status};
use crate::task::{Connection, WindowTask};
use deck_acp::Inbound;
use deck_runtime::approvals::Approval;
use deck_runtime::{Task, Watch};
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::Context;
use std::path::{Path, PathBuf};
use std::sync::Arc;

impl Deck {
    /// Launch with records under the default data root.
    pub fn launch(argv: Vec<String>, cwd: PathBuf, cx: &mut Context<Self>) -> Self {
        match deck_runtime::data_root() {
            Ok(data) => Self::launch_in(&data, argv, cwd, cx),
            Err(e) => {
                let mut deck = Self::detached(cx);
                deck.session.apply(Inbound::Failed(format!("{e:#}")));
                deck
            }
        }
    }

    /// Find or start the task without taking ownership of its lifetime.
    pub fn launch_in(data: &Path, argv: Vec<String>, cwd: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut deck = Self::detached(cx);
        deck.agent_cmd = argv.join(" ");
        match WindowTask::run(data, argv, cwd) {
            Ok((task, connection)) => {
                let watch = task.watch();
                deck.task = Some(task);
                deck.follow_boundary(watch, cx);
                deck.follow_tasks(cx);
                deck._quit = Some(cx.on_app_quit(|deck, _| {
                    deck.attachment += 1;
                    deck.client = None;
                    deck.task = None;
                    // Drop the watch before the entity can outlive the app;
                    // its reader thread then closes the daemon connection.
                    deck.watch = None;
                    deck.access_refresh = None;
                    deck.task_refresh = None;
                    async {}
                }));
                match connection {
                    Some(connection) => deck.connect(connection, cx),
                    None => deck.session.status = Status::Detached,
                }
            }
            Err(e) => deck.session.apply(Inbound::Failed(format!("{e:#}"))),
        }
        deck.sync_live_lane();
        deck
    }

    /// A detached or failed connection can be rebuilt by restarting its task.
    pub fn can_restart(&self) -> bool {
        self.task.is_some() && matches!(self.session.status, Status::Detached | Status::Down(_))
    }

    /// Explicitly interrupt a detached task and load its recorded history.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        if !self.can_restart() {
            return;
        }
        let Some(task) = self.task.clone() else { return };
        self.attachment += 1;
        let generation = self.attachment;
        self.client = None;
        self.session.status = Status::Connecting;
        self.sync_live_lane();
        cx.notify();
        let restarted = cx.background_executor().spawn(async move { task.restart() });
        cx.spawn(async move |this, cx| {
            let result = restarted.await;
            let _ = this.update(cx, |deck, cx| {
                // A switch or app quit since then owns the window now.
                if deck.attachment != generation {
                    return;
                }
                match result {
                    Ok(connection) => deck.connect(connection, cx),
                    Err(e) => deck.session.apply(Inbound::Failed(format!("restarting task: {e:#}"))),
                }
                deck.sync_live_lane();
                cx.notify();
            });
        })
        .detach();
    }

    /// Whether a task is connected and not already down.
    pub fn can_stop(&self) -> bool {
        self.task.is_some() && !matches!(self.session.status, Status::Down(_))
    }

    /// Stop the window's task and leave it stopped. The connection is dropped
    /// and the generation bumped; a restart card replaces the conversation.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if !self.can_stop() {
            return;
        }
        let Some(task) = self.task.clone() else { return };
        self.attachment += 1;
        let generation = self.attachment;
        self.client = None;
        self.session.status = Status::Connecting;
        self.sync_live_lane();
        cx.notify();
        let stopped = cx.background_executor().spawn(async move { task.stop() });
        cx.spawn(async move |this, cx| {
            let result = stopped.await;
            let _ = this.update(cx, |deck, cx| {
                if deck.attachment != generation {
                    return;
                }
                match result {
                    Ok(()) => deck.session.status = Status::Down("stopped".into()),
                    Err(e) => deck.session.apply(Inbound::Failed(format!("stopping task: {e:#}"))),
                }
                deck.sync_live_lane();
                cx.notify();
            });
        })
        .detach();
    }

    /// Switch the window to the task card `lane` shows. The current task
    /// keeps running: its client and watch are dropped, which only detaches.
    /// The selected task then connects by launch's rules (`WindowTask::open`).
    pub fn select(&mut self, lane: usize, cx: &mut Context<Self>) {
        let Some(current) = self.task.clone() else { return };
        let Some(id) = self.lanes.task(lane, Some(current.id())).filter(|_| lane != LIVE) else { return };
        let Some(record) = self.tasks.iter().find(|t| t.id == id).cloned() else { return };
        // The previous task's card takes the selected one's slot.
        self.lanes.switch(Some(current.id()), lane);
        self.switch_to(&current, &record, cx);
    }

    /// Start a new session: another task with this window's command and
    /// directory, then switch to it like a selected card. The current task
    /// keeps running and becomes an ordinary card. One at a time; a failed
    /// create leaves the window where it was and says why in the header.
    pub fn new_session(&mut self, cx: &mut Context<Self>) {
        if self.new_session.is_creating() {
            return;
        }
        let Some(current) = self.task.clone() else { return };
        self.new_session = NewSession::Creating;
        cx.notify();
        let generation = self.attachment;
        let created = cx.background_executor().spawn({
            let current = current.clone();
            async move { current.create_sibling() }
        });
        cx.spawn(async move |this, cx| {
            let created = created.await;
            let _ = this.update(cx, |deck, cx| {
                match created {
                    // A switch, a restart or quit since the click owns the
                    // window: the new task stays a card for the next refresh.
                    Ok(_) if deck.attachment != generation => deck.new_session = NewSession::Idle,
                    Ok(record) => {
                        deck.new_session = NewSession::Idle;
                        deck.tasks.push(record.clone());
                        deck.switch_to(&current, &record, cx);
                    }
                    Err(e) => deck.new_session = NewSession::Failed(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Point the window at task `record`: detach from the current task, reset
    /// everything that belonged to its session, and connect by launch's rules
    /// (`WindowTask::open`), which starts a task that has never run.
    fn switch_to(&mut self, current: &WindowTask, record: &Task, cx: &mut Context<Self>) {
        let next = current.sibling(record);
        self.new_session = NewSession::Idle;
        self.attachment += 1;
        let generation = self.attachment;
        self.client = None;
        self.watch = None;
        self.task = Some(next.clone());
        self.session = Session::new();
        self.expanded.clear();
        self.row_sigs.clear();
        self.transcript = super::transcript_list::following();
        self.boundary = Boundary::new();
        self.agent_publishes = None;
        self.editor = super::editor::Editor::new();
        self.workspace = None;
        if self.access.is_open() {
            self.toggle_access(cx);
        }
        let (tasks, published) = (self.tasks.clone(), std::mem::take(&mut self.published));
        self.on_tasks(tasks, published, cx);
        let opened = cx.background_executor().spawn(async move { (next.watch(), next.open()) });
        cx.spawn(async move |this, cx| {
            let (watch, connection) = opened.await;
            let _ = this.update(cx, |deck, cx| {
                // Another switch, a restart or quit has replaced this one;
                // dropping the results detaches from the task again.
                if deck.attachment != generation {
                    return;
                }
                deck.follow_boundary(watch, cx);
                match connection {
                    Ok(Some(connection)) => deck.connect(connection, cx),
                    Ok(None) => deck.session.status = Status::Detached,
                    Err(e) => deck.session.apply(Inbound::Failed(format!("opening task: {e:#}"))),
                }
                deck.sync_live_lane();
                cx.notify();
            });
        })
        .detach();
    }

    /// Follow the daemon's approval queue for this window's task. The
    /// watch connection is the attachment lease; dropping it detaches.
    /// Approvals already in flight from a task the window has left are
    /// dropped.
    fn follow_boundary(&mut self, watch: anyhow::Result<(Watch, UnboundedReceiver<Approval>)>, cx: &mut Context<Self>) {
        let (watch, mut rx) = match watch {
            Ok(watch) => watch,
            Err(e) => return self.session.apply(Inbound::Failed(format!("watching task approvals: {e:#}"))),
        };
        let Some(task) = self.task_id().map(str::to_string) else { return };
        self.watch = Some(watch);
        cx.spawn(async move |this, cx| {
            while let Some(approval) = rx.next().await {
                let applied = this.update(cx, |deck, cx| {
                    let current = deck.task_id() == Some(task.as_str());
                    if current {
                        deck.on_boundary(approval, cx);
                    }
                    current
                });
                if !matches!(applied, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    fn connect(&mut self, (client, mut rx): Connection, cx: &mut Context<Self>) {
        self.attachment += 1;
        let attachment = self.attachment;
        self.session = Session::new();
        self.agent_publishes = None;
        self.client = Some(Arc::new(client));
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.next().await {
                if this
                    .update(cx, |deck, cx| {
                        if deck.attachment == attachment && deck.client.is_some() {
                            deck.on_inbound(ev, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
}
