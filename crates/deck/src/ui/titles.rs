//! Recording a task's title with `deckd`: its first prompt names it, and a
//! task from before titles takes its name from its replayed transcript
//! (ADR 0010 §9).
//!
//! The design choice: the row updates at once from the in-memory record, while
//! the write to `deckd` runs off the UI thread like `seen` (`attention_state.rs`);
//! a failed write forgets the attempt, so a later prompt may record it.

use super::root::Deck;
use gpui::Context;

impl Deck {
    /// Name task `id` after the first line of `text`, unless its record
    /// already has a title or this window already reported one.
    pub(super) fn note_title(&mut self, id: &str, text: &str, cx: &mut Context<Self>) {
        let Some(title) = super::tasks::title_of(text) else { return };
        let untitled = self.tasks.iter().find(|task| task.id == id).is_some_and(|task| task.title.is_none());
        if !untitled || self.titled.contains_key(id) {
            return;
        }
        let Some(daemon) = self.daemon.clone() else { return };
        self.titled.insert(id.to_string(), title.clone());
        if let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) {
            task.title = Some(title.clone());
        }
        // Every row reads the in-memory record, so the new title shows now
        // rather than at the next task refresh.
        let (tasks, published) = (self.tasks.clone(), std::mem::take(&mut self.published));
        self.on_tasks(tasks, published, cx);

        let asked = id.to_string();
        let recorded = cx.background_executor().spawn({
            let asked = asked.clone();
            async move { daemon.title(&asked, &title) }
        });
        cx.spawn(async move |this, cx| {
            // Another prompt may still record it.
            if recorded.await.is_err() {
                let _ = this.update(cx, |deck, _| {
                    deck.titled.remove(&asked);
                });
            }
        })
        .detach();
    }

    /// Record a task's title from the first prompt its replayed transcript
    /// holds, when its record has none yet.
    pub(super) fn backfill_title(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(text) = self.attached.get(id).and_then(|attached| {
            attached.session.entries.iter().find_map(|entry| match entry {
                super::session::Entry::You(text) => Some(text.clone()),
                _ => None,
            })
        }) else {
            return;
        };
        self.note_title(id, &text, cx);
    }
}
