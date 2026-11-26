//! The window's view of decisions interrupted by a stopped task.

use super::root::Deck;
use deck_runtime::DaemonClient;
use deck_runtime::recovery::{Answer, Delivery, Item, Question};
use gpui::Context;
use std::collections::HashMap;

impl Deck {
    /// The first interrupted question for the selected task, including an
    /// answer whose delivery has not been confirmed.
    pub fn interrupted(&self) -> Option<&Item> {
        self.task_id()
            .and_then(|id| self.recoveries.get(id))
            .and_then(|items| items.iter().find(|item| unresolved(item)))
    }

    pub fn interrupted_busy(&self) -> bool {
        self.task_id().is_some_and(|id| self.recovery_busy.contains(id))
    }

    pub fn interrupted_error(&self) -> Option<&str> {
        self.task_id().and_then(|id| self.recovery_errors.get(id)).map(String::as_str)
    }

    pub fn choose_interrupted(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        let Some(item) = self.interrupted() else { return };
        match &item.delivery {
            Delivery::Answered { .. } if index == Some(0) => return self.retry_interrupted(cx),
            Delivery::Sending { .. } | Delivery::Uncertain { .. } => match index {
                Some(0) => return self.retry_interrupted(cx),
                Some(1) => return self.review_interrupted(cx),
                _ => return,
            },
            Delivery::Pending => {}
            _ => return,
        }
        let answer = match (&item.question, index) {
            (Question::Agent { options, .. }, Some(index)) => {
                options.get(index).map(|option| Answer::Agent { option_id: option.id.clone() })
            }
            (Question::Agent { .. }, None) => Some(Answer::AgentCancelled),
            (Question::Boundary { .. }, Some(index)) => [
                deck_runtime::approvals::Decision::Once,
                deck_runtime::approvals::Decision::Timed,
                deck_runtime::approvals::Decision::Task,
                deck_runtime::approvals::Decision::Project,
                deck_runtime::approvals::Decision::Deny,
            ]
            .get(index)
            .copied()
            .map(|decision| Answer::Boundary { decision }),
            (Question::Boundary { .. }, None) => None,
        };
        if let Some(answer) = answer {
            self.answer_interrupted(answer, cx);
        }
    }

    /// Refresh from deckd. A newer read or answer invalidates an older reply.
    pub(super) fn load_recoveries(&mut self, cx: &mut Context<Self>) {
        let Some(data) = self.data.clone() else { return };
        self.recovery_revision += 1;
        let revision = self.recovery_revision;
        let ids: Vec<_> = self
            .tasks
            .iter()
            .filter(|task| task.state != deck_runtime::State::Destroyed)
            .map(|task| task.id.clone())
            .collect();
        let read = cx.background_executor().spawn(async move {
            let mut found = HashMap::new();
            let daemon = DaemonClient::connect(&data)?;
            for id in ids {
                let items = daemon.recovery_items(&id)?;
                if items.iter().any(unresolved) {
                    found.insert(id, items);
                }
            }
            anyhow::Ok(found)
        });
        cx.spawn(async move |this, cx| {
            let Ok(found) = read.await else { return };
            let _ = this.update(cx, |deck, cx| {
                if deck.recovery_revision != revision {
                    return;
                }
                deck.recoveries = found;
                deck.refresh_attention();
                deck.sync_attention_lanes();
                deck.sync_live_lane();
                cx.notify();
            });
        })
        .detach();
    }

    /// Record an answer before attempting delivery. The daemon's first answer
    /// wins if another window decides at the same time.
    pub fn answer_interrupted(&mut self, answer: Answer, cx: &mut Context<Self>) {
        let Some(item) = self.interrupted().cloned() else { return };
        if !matches!(item.delivery, Delivery::Pending | Delivery::Answered { .. }) || self.interrupted_busy() {
            return;
        }
        let Some(task) = self.current().task.clone() else { return };
        let Some(daemon) = self.daemon.clone() else { return };
        let task_id = task.id().to_owned();
        let item_id = item.id;
        self.recovery_busy.insert(task_id.clone());
        self.recovery_errors.remove(&task_id);
        self.recovery_revision += 1;
        cx.notify();
        let asked = task_id.clone();
        let decided = cx.background_executor().spawn(async move {
            let result = daemon.answer_recovery(&asked, &item_id, answer);
            let items = daemon.recovery_items(&asked);
            let connection = if result.as_ref().is_ok_and(|item| matches!(item.delivery, Delivery::Delivered { .. })) {
                task.attach_live().ok().flatten()
            } else {
                None
            };
            (result, items, connection)
        });
        cx.spawn(async move |this, cx| {
            let (result, items, connection) = decided.await;
            let _ = this.update(cx, |deck, cx| {
                deck.recovery_busy.remove(&task_id);
                deck.recovery_revision += 1;
                if let Ok(items) = items {
                    if items.iter().any(unresolved) {
                        deck.recoveries.insert(task_id.clone(), items);
                    } else {
                        deck.recoveries.remove(&task_id);
                    }
                }
                if let Err(error) = result {
                    deck.recovery_errors.insert(task_id.clone(), format!("{error:#}"));
                }
                if let Some(connection) = connection
                    && deck.attached.get(&task_id).is_some_and(|attached| attached.client.is_none())
                {
                    deck.connect(&task_id, connection, cx);
                }
                deck.reload_tasks(cx);
                deck.refresh_attention();
                deck.sync_attention_lanes();
                deck.sync_live_lane();
                cx.notify();
            });
        })
        .detach();
    }

    /// Retry a saved answer with its original key. An agent without support
    /// refuses before a line reaches it, leaving the answer uncertain.
    pub fn retry_interrupted(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.interrupted().cloned() else { return };
        if !matches!(item.delivery, Delivery::Answered { .. } | Delivery::Sending { .. } | Delivery::Uncertain { .. })
            || self.interrupted_busy()
        {
            return;
        }
        self.resolve_recovery(item, false, cx);
    }

    /// Mark uncertainty reviewed without claiming whether delivery happened.
    pub fn review_interrupted(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.interrupted().cloned() else { return };
        if !matches!(item.delivery, Delivery::Sending { .. } | Delivery::Uncertain { .. }) || self.interrupted_busy() {
            return;
        }
        self.resolve_recovery(item, true, cx);
    }

    fn resolve_recovery(&mut self, item: Item, review: bool, cx: &mut Context<Self>) {
        let Some(task) = self.current().task.clone() else { return };
        let Some(daemon) = self.daemon.clone() else { return };
        let task_id = task.id().to_owned();
        self.recovery_busy.insert(task_id.clone());
        self.recovery_errors.remove(&task_id);
        self.recovery_revision += 1;
        cx.notify();
        let asked = task_id.clone();
        let id = item.id.clone();
        let decided = cx.background_executor().spawn(async move {
            let result = if review { daemon.review_recovery(&asked, &id) } else { daemon.retry_recovery(&asked, &id) };
            let items = daemon.recovery_items(&asked);
            let connection = if result.as_ref().is_ok_and(|item| deck_runtime::recovery::resolved(&item.delivery)) {
                task.attach_live().ok().flatten()
            } else {
                None
            };
            (result, items, connection)
        });
        cx.spawn(async move |this, cx| {
            let (result, items, connection) = decided.await;
            let _ = this.update(cx, |deck, cx| {
                deck.recovery_busy.remove(&task_id);
                deck.recovery_revision += 1;
                if let Ok(items) = items {
                    if items.iter().any(unresolved) {
                        deck.recoveries.insert(task_id.clone(), items);
                    } else {
                        deck.recoveries.remove(&task_id);
                    }
                }
                if let Err(error) = result {
                    deck.recovery_errors.insert(task_id.clone(), format!("{error:#}"));
                }
                if let Some(connection) = connection
                    && deck.attached.get(&task_id).is_some_and(|attached| attached.client.is_none())
                {
                    deck.connect(&task_id, connection, cx);
                }
                deck.reload_tasks(cx);
                deck.refresh_attention();
                deck.sync_attention_lanes();
                deck.sync_live_lane();
                cx.notify();
            });
        })
        .detach();
    }
}

pub(super) fn unresolved(item: &Item) -> bool {
    !deck_runtime::recovery::resolved(&item.delivery)
}

pub(super) fn answer_label(item: &Item) -> Option<String> {
    let answer = match &item.delivery {
        Delivery::Answered { answer, .. }
        | Delivery::Sending { answer, .. }
        | Delivery::Uncertain { answer, .. }
        | Delivery::Reviewed { answer, .. }
        | Delivery::Delivered { answer, .. } => answer,
        Delivery::Pending => return None,
    };
    match (answer, &item.question) {
        (Answer::Agent { option_id }, Question::Agent { options, .. }) => {
            options.iter().find(|option| &option.id == option_id).map(|option| option.name.clone())
        }
        (Answer::AgentCancelled, Question::Agent { .. }) => Some("Cancelled".into()),
        (Answer::Boundary { decision }, Question::Boundary { .. }) => Some(
            match decision {
                deck_runtime::approvals::Decision::Once => "Allow once",
                deck_runtime::approvals::Decision::Timed => "Allow 5 minutes",
                deck_runtime::approvals::Decision::Task => "Allow for task",
                deck_runtime::approvals::Decision::Project => "Allow for project",
                deck_runtime::approvals::Decision::Deny => "Deny",
            }
            .into(),
        ),
        _ => None,
    }
}
