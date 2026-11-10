//! What the window does about attention: fold the items, record seen turns,
//! select a task from an item, and mark a task done.
//!
//! The design choice: the fold lives in one place and every action asks `deckd`
//! off the UI thread, then updates the record from the reply. Nothing here
//! renders; `sidebar_view.rs` reads the state it keeps.

use super::attention::{self, Item, Watched};
use super::root::Deck;
use crate::task::WindowTask;
use deck_acp::TurnState;
use deck_runtime::approvals::Approval;
use gpui::{Context, FollowMode};
use std::collections::HashMap;
use std::time::Instant;

impl Deck {
    /// Fold every task's current reason into the strip, oldest first.
    pub(super) fn refresh_attention(&mut self) {
        let watched: HashMap<String, Watched<'_>> = self
            .attached
            .iter()
            .map(|(id, attached)| {
                let watched = Watched {
                    turn: attached.turn.as_ref().map(|(number, state)| (*number, state)),
                    question: attached.session.ask.as_ref().map(|ask| ask.title.as_str()),
                    approval: attached.boundary.pending(),
                    files: attention::changed_files(&attached.session.entries),
                };
                (id.clone(), watched)
            })
            .collect();
        self.needs_you =
            attention::items(&self.tasks, &watched, &self.approvals, &mut self.attention_since, Instant::now());
    }

    /// Put each card's reason beside its state, without rebuilding the board.
    pub(super) fn sync_attention_lanes(&mut self) {
        let own = self.task_id().map(str::to_owned);
        let updates: Vec<(usize, Option<&'static str>)> = self
            .board
            .cards()
            .iter()
            .filter_map(|card| {
                let task = self.lanes.task(card.lane.id, own.as_deref())?;
                let attention = self.needs_you.iter().find(|item| item.task == task).map(|item| item.reason.word());
                Some((card.lane.id, attention))
            })
            .collect();
        for (lane, attention) in updates {
            if let Some(lane) = self.board.lane_mut(lane) {
                lane.attention = attention;
            }
        }
    }

    /// The sidebar's Needs-you items, oldest first.
    pub fn needs_you(&self) -> &[Item] {
        &self.needs_you
    }

    /// Every task record `deckd` last listed.
    pub fn tasks(&self) -> &[deck_runtime::Task] {
        &self.tasks
    }

    /// The selected project's settled tasks, most recently settled first.
    pub fn settled(&self) -> &[super::tasks::Settled] {
        &self.settled
    }

    pub fn settled_open(&self) -> bool {
        self.settled_open
    }

    pub fn toggle_settled(&mut self, cx: &mut Context<Self>) {
        self.settled_open = !self.settled_open;
        cx.notify();
    }

    pub fn mark_error(&self) -> Option<&str> {
        self.mark_error.as_deref()
    }

    /// `deckd`'s pending approvals, for tasks the window does not watch.
    pub fn on_approvals(&mut self, approvals: HashMap<String, Vec<Approval>>, cx: &mut Context<Self>) {
        self.approvals = approvals;
        self.refresh_attention();
        self.sync_attention_lanes();
        cx.notify();
    }

    /// Select the project and task of sidebar item `index` and show what it
    /// needs: the permission or boundary card, or the last turn's footer.
    pub fn open_item(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.needs_you.get(index).cloned() else { return };
        self.select_task_id(&item.task, cx);
        // The footer of the last finished turn is at the transcript's tail.
        self.transcript.set_follow_mode(FollowMode::Tail);
        self.refresh_attention();
        self.sync_attention_lanes();
        self.sync_live_lane();
        cx.notify();
    }

    /// Select task `id` in its own project, keeping its live attachment.
    pub fn select_task_id(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(record) = self.tasks.iter().find(|task| task.id == id).cloned() else { return };
        if let Some(project) = self.projects.task_project(&record) {
            self.projects.select(&project);
            self.rail.select(self.projects.index());
        }
        let from = self.task_id().map(str::to_owned);
        if from.as_deref() == Some(id) {
            self.refresh_attention();
            self.sync_attention_lanes();
            return cx.notify();
        }
        let lane = self.lanes.slot(id, from.as_deref());
        self.lanes.switch(from.as_deref(), lane);
        let current =
            self.current().task.clone().or_else(|| self.daemon.clone().map(|daemon| WindowTask::of(daemon, &record)));
        if let Some(current) = current {
            self.select_task(&current, record, cx);
        }
    }

    /// Record that the user has looked at the selected task's current turn,
    /// at most once per (task, turn) (ADR 0010 §6).
    pub(super) fn note_seen(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else { return };
        let Some((number, TurnState::Idle { .. })) = self.attached.get(&id).and_then(|a| a.turn.clone()) else {
            return;
        };
        let seen = self.tasks.iter().find(|task| task.id == id).map_or(0, |task| task.seen);
        if number <= seen || self.seen_sent.get(&id).is_some_and(|sent| *sent >= number) {
            return;
        }
        let Some(daemon) = self.daemon.clone() else { return };
        self.seen_sent.insert(id.clone(), number);
        let asked = id.clone();
        let recorded = cx.background_executor().spawn(async move { daemon.seen(&asked, number) });
        cx.spawn(async move |this, cx| {
            let result = recorded.await;
            let _ = this.update(cx, |deck, cx| match result {
                Ok(()) => {
                    if let Some(task) = deck.tasks.iter_mut().find(|task| task.id == id) {
                        task.seen = task.seen.max(number);
                    }
                    deck.refresh_attention();
                    deck.sync_attention_lanes();
                    deck.sync_live_lane();
                    cx.notify();
                }
                // Another attempt may still succeed, so do not remember it.
                Err(_) => {
                    deck.seen_sent.remove(&id);
                }
            });
        })
        .detach();
    }

    /// Mark task `id` done, or clear the mark. `deckd` owns the record; the
    /// card and fold update from its reply.
    pub fn mark_task(&mut self, id: &str, done: bool, cx: &mut Context<Self>) {
        let Some(daemon) = self.daemon.clone() else { return };
        let id = id.to_string();
        let asked = id.clone();
        let recorded = cx.background_executor().spawn(async move { daemon.mark(&asked, done) });
        cx.spawn(async move |this, cx| {
            let result = recorded.await;
            let _ = this.update(cx, |deck, cx| match result {
                Ok(()) => {
                    if let Some(task) = deck.tasks.iter_mut().find(|task| task.id == id) {
                        task.done_ms = done.then(super::publishing::now_ms);
                    }
                    deck.mark_error = None;
                    let (tasks, published) = (deck.tasks.clone(), std::mem::take(&mut deck.published));
                    deck.on_tasks(tasks, published, cx);
                }
                Err(e) => {
                    deck.mark_error = Some(format!("{e:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }
}
