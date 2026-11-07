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
use super::root::{Deck, LIVE};
use deck_acp::Inbound;
use deck_runtime::Task;
use deck_runtime::pull_request::State as Published;
use gpui::Context;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How often an open window refreshes its task list and publications.
const REFRESH: Duration = Duration::from_secs(15);

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

impl Deck {
    /// Refresh now and every [`REFRESH`] while the window lives.
    pub(super) fn follow_tasks(&mut self, cx: &mut Context<Self>) {
        let executor = cx.background_executor().clone();
        self.task_refresh = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(Some(task)) = this.update(cx, |deck, _| deck.task.clone()) else { break };
                let read = executor.spawn(async move {
                    let tasks = task.tasks()?;
                    let mut published = HashMap::new();
                    for t in tasks.iter().filter(|t| t.publication.is_some()) {
                        if let Ok(state) = task.pull_request(&t.id, false) {
                            published.insert(t.id.clone(), state);
                        }
                    }
                    anyhow::Ok((tasks, published))
                });
                if let Ok((tasks, published)) = read.await
                    && this.update(cx, |deck, cx| deck.on_tasks(tasks, published, cx)).is_err()
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
        let mut lanes: Vec<Lane> =
            self.board.cards().iter().filter(|c| c.lane.id == LIVE).map(|c| c.lane.clone()).collect();
        lanes.extend(self.lanes.lanes(&tasks, own.as_deref(), &published, now));
        self.board.sync(lanes);
        self.tasks = tasks;
        self.published = published;
        self.sync_live_lane();
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
        self.tasks
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.spec.cwd.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| id.to_string())
    }

    /// The capability warning for the window's own task (ADR 0008 §1).
    pub fn publish_notice(&self) -> Option<&'static str> {
        publication::notice(self.task.as_ref().is_some_and(|t| t.publishes()), self.agent_publishes)
    }

    /// Open the panel for lane `lane`'s task, or close it if it is open.
    pub fn toggle_publication(&mut self, lane: usize, cx: &mut Context<Self>) {
        let task = self.lanes.task(lane, self.task_id());
        self.destroy_confirm = false;
        self.publication_open = if self.publication_open == task { None } else { task };
        if self.publication_open.is_some() && self.access.is_open() {
            self.toggle_access(cx);
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
        let Some(task) = self.task.clone() else { return };
        let own = self.task_id() == Some(id.as_str());
        if own {
            // The agent is going away: stop reading from it first.
            self.attachment += 1;
            self.client = None;
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
                    Ok(()) if own => deck.session.apply(Inbound::Failed("task destroyed".into())),
                    Ok(()) => {}
                    Err(e) => deck.session.apply(Inbound::Failed(format!("destroying task: {e:#}"))),
                }
                let tasks: Vec<Task> = deck.tasks.iter().filter(|t| t.id != id).cloned().collect();
                let published = std::mem::take(&mut deck.published);
                deck.on_tasks(tasks, published, cx);
            });
        })
        .detach();
    }
}
