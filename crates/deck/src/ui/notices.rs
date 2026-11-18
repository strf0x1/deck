//! `deckd`'s notice stream as the window uses it: prompts a task's holder
//! sent, which a worker's transcript shows as "from coordinator" (deck#150
//! R8, ADR 0011 §9), and a coordinator's cap refusals (deck#152 R4).
//!
//! The design choice: a holder's prompt reaches the agent without passing
//! through the window, so the window learns of it here and puts it in the
//! attached transcript itself. After a later attach the agent's replay holds
//! the message and `prompts.jsonl` (read through `deckd`) says who sent it,
//! so both paths end in the same per-task list the label reads.
//!
//! The stream is followed across `deckd` restarts (deck#199): after a gap it
//! reconnects, and the window reads the task list and each attached task's
//! prompt log again, since notices sent while `deckd` was gone are lost.

use super::root::Deck;
use deck_runtime::Notice;
use deck_runtime::daemon_client::Followed;
use deck_runtime::record::Prompt;
use futures::StreamExt as _;

use gpui::Context;

/// The notice stream's receiver, shared so quit can drop it synchronously.
pub(super) type Notices = std::rc::Rc<std::cell::RefCell<Option<futures::channel::mpsc::UnboundedReceiver<Followed>>>>;

impl Deck {
    /// Follow `deckd`'s notices while the window lives, on a connection of
    /// their own. The receiver is shared with [`Deck::stop_notices`], which
    /// drops it at once: a cancelled task's future is only dropped when the
    /// executor next runs, and after quit it may never run again.
    pub(super) fn follow_notices(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = self.daemon.clone() else { return };
        let Ok(rx) = daemon.follow() else { return };
        let shared = std::rc::Rc::new(std::cell::RefCell::new(Some(rx)));
        self.notices = Some(shared.clone());
        cx.spawn(async move |this, cx| {
            loop {
                let next = futures::future::poll_fn(|ctx| match shared.borrow_mut().as_mut() {
                    Some(rx) => rx.poll_next_unpin(ctx),
                    None => std::task::Poll::Ready(None),
                })
                .await;
                let Some(next) = next else { break };
                let applied = this.update(cx, |deck, cx| match next {
                    Followed::Notice(notice) => deck.on_notice(*notice, cx),
                    Followed::Resubscribed => deck.on_resubscribed(cx),
                });
                if applied.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Close the notice connection now.
    pub(super) fn stop_notices(&mut self) {
        if let Some(shared) = self.notices.take() {
            shared.borrow_mut().take();
        }
    }

    /// Apply one notice: a prompt a holder sent, or a cap a coordinator's
    /// `create` met. A task the window has not listed, one whose lifecycle
    /// moved, or one still waiting for its session reads the list now rather
    /// than at the next refresh, so a coordinator's new worker shows, and is
    /// attached, at once.
    pub fn on_notice(&mut self, notice: Notice, cx: &mut Context<Self>) {
        // A running task with no session recorded yet cannot be attached;
        // the list is read again until its session shows (ADR 0010 §8).
        let stale = self.tasks.iter().find(|task| task.id == notice.task).is_none_or(|task| {
            task.state != notice.state || (task.state == deck_runtime::State::Running && task.session_id.is_none())
        });
        if stale {
            self.reload_tasks(cx);
        }
        if let Some(prompt) = notice.prompt {
            self.on_prompt(&notice.task, prompt, cx);
        }
        if let Some(cap) = notice.cap {
            self.on_cap(cap, cx);
        }
    }

    /// The notice stream is back after `deckd` was gone: read what the
    /// missed notices would have said from the records. A holder prompt sent
    /// in the gap goes into its attached transcript as a live one would.
    fn on_resubscribed(&mut self, cx: &mut Context<Self>) {
        self.reload_tasks(cx);
        let Some(data) = self.data.clone() else { return };
        let ids: Vec<String> = self.attached.keys().cloned().collect();
        let read = cx.background_executor().spawn(async move {
            let daemon = deck_runtime::DaemonClient::connect(&data)?;
            anyhow::Ok(ids.into_iter().filter_map(|id| Some((daemon.prompts(&id).ok()?, id))).collect::<Vec<_>>())
        });
        cx.spawn(async move |this, cx| {
            let Ok(logs) = read.await else { return };
            let _ = this.update(cx, |deck, cx| {
                for (prompts, id) in logs {
                    for prompt in prompts {
                        deck.on_prompt(&id, prompt, cx);
                    }
                }
            });
        })
        .detach();
    }

    /// A prompt task `task`'s holder sent: kept for its label, and shown in
    /// the attached transcript, which never saw it go out. A notice can be
    /// read after a newer connection replayed the same prompt; that one is
    /// left to the replay.
    fn on_prompt(&mut self, task: &str, prompt: Prompt, cx: &mut Context<Self>) {
        let list = self.prompts.entry(task.to_string()).or_default();
        if list.iter().any(|known| known.turn == prompt.turn && known.text == prompt.text) {
            return;
        }
        list.push(prompt.clone());
        // A connection still loading will replay the prompt itself; only a
        // ready one needs it put in (`Ready` follows the replay).
        if let Some(attached) = self.attached.get_mut(task)
            && attached.client.is_some()
            && attached.session.status != super::session::Status::Connecting
            && attached.session.last_prompt() != Some(prompt.text.as_str())
        {
            attached.session.prompted(&prompt.text);
        }
        if self.task_id() == Some(task) {
            self.transcript.set_follow_mode(gpui::FollowMode::Tail);
        }
        cx.notify();
    }

    /// Read the prompts task `id`'s holder recorded, for the labels of a
    /// transcript the agent just replayed.
    pub(super) fn load_prompts(&mut self, id: &str, cx: &mut Context<Self>) {
        // The data root, not the window's connection (see `reload_tasks`).
        let Some(data) = self.data.clone() else { return };
        let asked = id.to_string();
        let read =
            cx.background_executor().spawn(async move { deck_runtime::DaemonClient::connect(&data)?.prompts(&asked) });
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            let Ok(prompts) = read.await else { return };
            let _ = this.update(cx, |deck, cx| {
                deck.prompts.insert(id, prompts);
                cx.notify();
            });
        })
        .detach();
    }

    /// The selected transcript's entries that a coordinator sent.
    pub fn from_coordinator(&self) -> Vec<usize> {
        let Some(id) = self.task_id() else { return Vec::new() };
        let Some(prompts) = self.prompts.get(id) else { return Vec::new() };
        super::coordinator::from_coordinator(&self.session().entries, prompts)
    }
}
