//! What the window does with a project's coordinator (ADR 0010 §5, ADR 0011
//! §9): show it as the project view, start one from the composer's first
//! prompt, and report an agent that refuses its session.
//!
//! The design choice: the coordinator is an ordinary selected attachment, so
//! its transcript, composer, stop and restart are the task machinery; what
//! makes it the project view is only that its record's kind is
//! `coordinator`. Selecting it never starts it: a stopped coordinator shows
//! Restart, and only the user's prompt or Restart runs it.

use super::coordinator;
use super::new_session::NewSession;
use super::root::Deck;
use crate::task::WindowTask;
use deck_runtime::{Task, TaskKind};
use gpui::Context;

impl Deck {
    /// The selected project's coordinator, running, stopped or failed.
    pub fn coordinator(&self) -> Option<&Task> {
        coordinator::of_project(&self.tasks, self.projects.selected_id()?)
    }

    /// Whether the selected task is a coordinator, so the main area is its
    /// project's view.
    pub fn coordinator_selected(&self) -> bool {
        let Some(id) = self.task_id() else { return false };
        self.tasks.iter().any(|task| task.id == id && coordinator::is_coordinator(task))
    }

    /// Whether the main area shows the selected project's view: no task is
    /// selected, or the selected task is the project's coordinator.
    pub fn showing_project(&self) -> bool {
        self.projects.selected().is_some() && (self.task_id().is_none() || self.coordinator_selected())
    }

    /// The backend a new coordinator would run on, for the composer.
    pub fn coordinator_backend(&self) -> String {
        coordinator::backend(&self.tasks, std::env::var_os("DECK_VM_IMAGE").is_some())
    }

    /// Why the project's coordinator refused its session, while it stands.
    pub fn coordinator_refusal(&self) -> Option<&str> {
        let id = self.coordinator()?.id.as_str();
        self.refusals.get(id).map(String::as_str)
    }

    /// Who started the task card `lane` shows, when a coordinator did
    /// (ADR 0011 §6).
    pub fn lane_provenance(&self, lane: usize) -> Option<String> {
        let id = self.lanes.task(lane, self.task_id())?;
        let record = self.tasks.iter().find(|task| task.id == id)?;
        coordinator::provenance(record, &self.tasks)
    }

    /// Show the selected project's view: its coordinator's conversation when
    /// it has one, else an empty conversation whose composer starts one.
    pub fn show_project(&mut self, cx: &mut Context<Self>) {
        self.coordinator_destroy = false;
        self.clear_task(cx);
        let Some(record) = self.coordinator().cloned() else { return };
        let Some(daemon) = self.daemon.clone() else { return };
        let task = WindowTask::of(daemon, &record);
        self.select_task(&task, record, cx);
    }

    /// The selected project's create spec for its coordinator; `deckd` puts
    /// in the project's coordinator command (ADR 0011 §2).
    fn coordinator_spec(&self) -> Option<deck_runtime::Spec> {
        Some(deck_runtime::Spec { kind: TaskKind::Coordinator, ..self.project_spec()? })
    }

    /// The first prompt in a project with no coordinator is its brief: create
    /// the coordinator, select it, and send the text once its connection is
    /// ready (ADR 0010 §5). A coordinator another window started in the
    /// meantime is selected instead, and the text goes back to the composer.
    pub(super) fn start_coordinator(&mut self, text: String, cx: &mut Context<Self>) {
        if self.new_session.is_creating() {
            return;
        }
        let Some(daemon) = self.daemon.clone() else { return };
        let Some(spec) = self.coordinator_spec() else { return };
        let project = self.projects.selected_id().map(str::to_owned);
        self.new_session = NewSession::Creating;
        self.pending_prompt = Some(text.clone());
        self.editor.take();
        cx.notify();
        let creating = daemon.clone();
        let created = cx.background_executor().spawn(async move {
            let created = creating.create(spec);
            // A refusal may be the scope's existing coordinator: read the
            // list so the window can select it.
            let tasks = if created.is_err() { creating.tasks().ok() } else { None };
            (created, tasks)
        });
        cx.spawn(async move |this, cx| {
            let (created, tasks) = created.await;
            let _ = this.update(cx, |deck, cx| {
                deck.new_session = NewSession::Idle;
                let restore = |deck: &mut Deck| {
                    deck.pending_prompt = None;
                    if deck.editor.is_empty() {
                        deck.editor.insert(&text);
                    }
                };
                match created {
                    // The user switched project meanwhile: it stays that
                    // project's coordinator, and the text is not lost.
                    Ok(record) if deck.projects.selected_id() != project.as_deref() => {
                        deck.tasks.push(record);
                        restore(deck);
                    }
                    Ok(record) => {
                        deck.tasks.push(record.clone());
                        let task = WindowTask::of(daemon, &record);
                        deck.select_task(&task, record, cx);
                    }
                    Err(e) => {
                        restore(deck);
                        let existing = tasks.as_ref().and_then(|tasks| {
                            coordinator::of_project(tasks, project.as_deref()?).cloned().map(|c| (c, tasks.clone()))
                        });
                        match existing {
                            Some((record, tasks)) if deck.projects.selected_id() == project.as_deref() => {
                                let published = std::mem::take(&mut deck.published);
                                deck.on_tasks(tasks, published, cx);
                                let task = WindowTask::of(daemon, &record);
                                deck.select_task(&task, record, cx);
                            }
                            _ => deck.new_session = NewSession::Failed(format!("{e:#}")),
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Destroy the project's stopped coordinator, on the second click: its
    /// workers keep running (ADR 0011 §2), and the composer is ready to start
    /// the next one.
    pub fn destroy_coordinator(&mut self, cx: &mut Context<Self>) {
        let Some(record) = self.coordinator().cloned() else { return };
        if record.state == deck_runtime::State::Running {
            return;
        }
        if !self.coordinator_destroy {
            self.coordinator_destroy = true;
            return cx.notify();
        }
        self.coordinator_destroy = false;
        let Some(daemon) = self.daemon.clone() else { return };
        let id = record.id.clone();
        let destroyed = cx.background_executor().spawn(async move {
            daemon.destroy(&id)?;
            daemon.tasks()
        });
        cx.spawn(async move |this, cx| {
            let result = destroyed.await;
            let _ = this.update(cx, |deck, cx| match result {
                Ok(tasks) => {
                    deck.attached.remove(&record.id);
                    deck.refusals.remove(&record.id);
                    let published = std::mem::take(&mut deck.published);
                    deck.on_tasks(tasks, published, cx);
                    if deck.task_id() == Some(record.id.as_str()) || deck.task_id().is_none() {
                        deck.show_project(cx);
                    }
                }
                Err(e) => {
                    deck.new_session = NewSession::Failed(format!("destroying the coordinator: {e:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Read the task list now rather than at the next refresh: after a stop
    /// or restart the header's record-driven controls follow at once.
    pub(super) fn reload_tasks(&mut self, cx: &mut Context<Self>) {
        // Only the data root crosses into the read, never the window's
        // connection: a read still queued at quit must not hold it open
        // (as `follow_tasks` says).
        let Some(data) = self.data.clone() else { return };
        let read = cx.background_executor().spawn(async move { deck_runtime::DaemonClient::connect(&data)?.tasks() });
        cx.spawn(async move |this, cx| {
            let Ok(tasks) = read.await else { return };
            let _ = this.update(cx, |deck, cx| {
                let published = std::mem::take(&mut deck.published);
                deck.on_tasks(tasks, published, cx);
                deck.attach_new(cx);
            });
        })
        .detach();
    }

    /// Whether Destroy waits for its second click.
    pub fn coordinator_destroy_armed(&self) -> bool {
        self.coordinator_destroy
    }

    /// The coordinator's agent refused its session (deck#151 R6): tell
    /// `deckd`, which records a failed start, and keep the reason for the
    /// project view instead of leaving a dead task behind.
    pub(super) fn coordinator_refused(&mut self, id: &str, failure: &str, cx: &mut Context<Self>) {
        self.refusals.insert(id.to_string(), failure.to_string());
        let Some(daemon) = self.daemon.clone() else { return };
        let (asked, error) = (id.to_string(), failure.to_string());
        let recorded = cx.background_executor().spawn(async move {
            daemon.session_refused(&asked, &error)?;
            daemon.tasks()
        });
        cx.spawn(async move |this, cx| {
            let Ok(tasks) = recorded.await else { return };
            let _ = this.update(cx, |deck, cx| {
                let published = std::mem::take(&mut deck.published);
                deck.on_tasks(tasks, published, cx);
            });
        })
        .detach();
    }
}
