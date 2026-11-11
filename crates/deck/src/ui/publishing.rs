//! The window's view of every task and what each published: a refresh loop
//! off the UI thread, the publication panel's open state, and destroy after
//! a merge (ADR 0008 §8).
//!
//! The design choice: the window asks `deckd` on a fixed interval while it is
//! open, and `deckd` decides whether that reaches GitHub (`pull_request`'s
//! own interval and backoff), so many windows cost one read per task. A reply
//! from before the window's last refresh is applied anyway: task lists and
//! publications only ever describe the present.

use super::board::Lane;
use super::publication::{self, View};
use super::root::Deck;
use deck_acp::Inbound;
use deck_runtime::pull_request::State as Published;
use deck_runtime::{DaemonClient, Task};
use gpui::Context;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How often an open window refreshes its task list and publications.
const REFRESH: Duration = Duration::from_secs(15);

pub(super) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

impl Deck {
    /// Refresh now and every [`REFRESH`] while the window lives.
    pub(super) fn follow_tasks(&mut self, cx: &mut Context<Self>) {
        let Some(data) = self.data.clone() else { return };
        let executor = cx.background_executor().clone();
        self.task_refresh = Some(cx.spawn(async move |this, cx| {
            loop {
                let root = data.clone();
                // Tasks this window already watches get their items from the
                // watch connection; the rest need the queue listed.
                let watched = this.update(cx, |deck, _| deck.attached_ids()).unwrap_or_default();
                // Only the data root crosses into the background read, not a
                // daemon handle: a queued read the window drops before it runs
                // would otherwise hold a control connection open and delay
                // daemon exit (as in the launch attach, `attachment.rs`).
                let read = executor.spawn(async move {
                    let daemon = DaemonClient::connect(&root)?;
                    let tasks = daemon.tasks()?;
                    let mut published = HashMap::new();
                    let mut approvals = HashMap::new();
                    for t in &tasks {
                        if t.publication.is_some()
                            && let Ok(state) = daemon.pull_request(&t.id, false)
                        {
                            published.insert(t.id.clone(), state);
                        }
                        if !watched.contains(&t.id)
                            && let Ok(items) = daemon.approvals(&t.id)
                            && !items.is_empty()
                        {
                            approvals.insert(t.id.clone(), items);
                        }
                    }
                    anyhow::Ok((tasks, published, approvals))
                });
                if let Ok((tasks, published, approvals)) = read.await
                    && this
                        .update(cx, |deck, cx| {
                            deck.on_tasks(tasks, published, cx);
                            deck.on_approvals(approvals, cx);
                        })
                        .is_err()
                {
                    break;
                }
                executor.timer(REFRESH).await;
            }
        }));
    }

    /// Show `tasks` and their publications: the window's own task is the live
    /// card, every other task that is not destroyed has one of its own.
    pub fn on_tasks(&mut self, tasks: Vec<Task>, published: HashMap<String, Published>, cx: &mut Context<Self>) {
        let own = self.task_id().map(str::to_string);
        let now = now_ms();
        // The column lists the selected project's tasks only (ADR 0010 §9),
        // settled ones folding below the rest.
        self.tasks = tasks;
        // A refresh read before this window's title write landed still says
        // `None`; keep the title the row already shows (ADR 0010 §9).
        let titled = &self.titled;
        for record in &mut self.tasks {
            if record.title.is_none() {
                record.title = titled.get(&record.id).cloned();
            }
        }
        self.published = published;
        let mut visible = Vec::new();
        for record in &self.tasks {
            if own.as_deref() == Some(record.id.as_str()) || self.projects.holds(record) {
                visible.push(record.clone());
            }
        }
        let mut shown = Vec::new();
        let mut settled = Vec::new();
        for record in visible {
            let at = super::attention::settled(&record, self.published.get(&record.id));
            let is_own = own.as_deref() == Some(record.id.as_str());
            if !is_own
                && let Some(at_ms) = at
                && now.saturating_sub(at_ms) < super::tasks::SETTLED_WINDOW_MS
            {
                settled.push(super::tasks::Settled { task: record, at_ms });
            } else if is_own || at.is_none() {
                // A settled selection stays the live card; a record settled
                // longer ago than the window shows nowhere.
                shown.push(record);
            }
        }
        settled.sort_by(|a, b| b.at_ms.cmp(&a.at_ms).then_with(|| a.task.id.cmp(&b.task.id)));
        // A settled task has no card, but its chip still opens the
        // publication panel, so it keeps a lane id for the window's life.
        for item in &settled {
            self.lanes.slot(&item.task.id, own.as_deref());
        }
        self.settled = settled;
        // Most recent activity first; the record id begins with its time.
        shown.sort_by(|a, b| b.id.cmp(&a.id));
        self.refresh_attention();
        let mut lanes: Vec<Lane> = Vec::new();
        let live_lane = self.lanes.live();
        let had_live = self.board.cards().iter().any(|c| c.lane.id == live_lane);
        if own.is_some() {
            let live = self
                .board
                .cards()
                .iter()
                .find(|c| c.lane.id == live_lane)
                .map_or_else(|| super::root::live_card(live_lane), |c| c.lane.clone());
            lanes.push(live);
        }
        lanes.extend(self.lanes.lanes(&shown, own.as_deref(), &self.published, &self.needs_you, now));
        self.board.sync(lanes);
        // A new live card belongs at the top, where the selected task shows;
        // an existing one keeps the slot the user clicked.
        if own.is_some() && !had_live {
            self.board.to_front(live_lane);
        }
        // One small file read for the branch, every refresh, not every frame.
        self.workspace = self.tasks.iter().find(|t| Some(&t.id) == own.as_ref()).map(super::prompt::Workspace::of);
        self.sync_live_lane();
        self.note_seen(cx);
        cx.notify();
    }

    /// The publication of the task lane `lane` shows.
    pub fn publication_of(&self, lane: usize) -> Option<View> {
        let task = self.lanes.task(lane, self.task_id())?;
        publication::view(self.published.get(&task)?, now_ms())
    }

    /// The task whose publication panel is open, and its view.
    pub fn open_publication(&self) -> Option<(&str, Option<View>)> {
        let task = self.publication_open.as_deref()?;
        Some((task, self.published.get(task).and_then(|p| publication::view(p, now_ms()))))
    }

    /// The project name of task `id`, for the panel's heading.
    pub fn task_title(&self, id: &str) -> String {
        self.tasks.iter().find(|t| t.id == id).map_or_else(|| id.to_string(), super::tasks::title)
    }

    /// The capability warning for the window's own task (ADR 0008 §1).
    pub fn publish_notice(&self) -> Option<&'static str> {
        let attached = self.current();
        publication::notice(attached.task.as_ref().is_some_and(|t| t.publishes()), attached.agent_publishes)
    }

    /// Open the panel for lane `lane`'s task, or close it if it is open.
    pub fn toggle_publication(&mut self, lane: usize, cx: &mut Context<Self>) {
        let task = self.lanes.task(lane, self.task_id());
        self.destroy_confirm = false;
        self.publication_open = if self.publication_open == task { None } else { task };
        if self.publication_open.is_some() && self.access.is_open() {
            self.toggle_access(cx);
        }
        if self.publication_open.is_some() && self.settings.is_open() {
            self.settings.toggle();
        }
        cx.notify();
    }

    pub fn close_publication(&mut self, cx: &mut Context<Self>) {
        self.publication_open = None;
        self.destroy_confirm = false;
        cx.notify();
    }

    /// The first click on Destroy asks; the second destroys. Offered only for
    /// a merged pull request.
    pub fn destroy_published(&mut self, cx: &mut Context<Self>) {
        let Some((id, Some(view))) = self.open_publication() else { return };
        if !view.done() {
            return;
        }
        if !self.destroy_confirm {
            self.destroy_confirm = true;
            return cx.notify();
        }
        let id = id.to_string();
        let Some(task) = self.current().task.clone() else { return };
        let own = self.task_id() == Some(id.as_str());
        if own {
            // The agent is going away: stop reading from it first.
            let generation = self.stamp();
            if let Some(attached) = self.attached.get_mut(&id) {
                attached.generation = generation;
                attached.client = None;
            }
        }
        let destroyed = cx.background_executor().spawn({
            let id = id.clone();
            async move { task.destroy(&id) }
        });
        cx.spawn(async move |this, cx| {
            let result = destroyed.await;
            let _ = this.update(cx, |deck, cx| {
                deck.publication_open = None;
                deck.destroy_confirm = false;
                match result {
                    Ok(()) if own => {
                        if let Some(attached) = deck.attached.get_mut(&id) {
                            attached.session.apply(Inbound::Failed("task destroyed".into()));
                        }
                    }
                    Ok(()) => {}
                    Err(e) => {
                        deck.current_mut().session.apply(Inbound::Failed(format!("destroying task: {e:#}")));
                    }
                }
                let tasks: Vec<Task> = deck.tasks.iter().filter(|t| t.id != id).cloned().collect();
                let published = std::mem::take(&mut deck.published);
                deck.on_tasks(tasks, published, cx);
            });
        })
        .detach();
    }
}
