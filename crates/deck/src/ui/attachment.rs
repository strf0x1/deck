//! Connecting the window to its daemon-owned task. Restart runs outside the
//! UI thread, and every new connection rebuilds Session before replay starts.

use super::root::Deck;
use super::session::{Session, Status};
use crate::task::{Connection, WindowTask};
use deck_acp::Inbound;
use futures::StreamExt as _;
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
                deck.task = Some(task);
                deck._quit = Some(cx.on_app_quit(|deck, _| {
                    deck.attachment += 1;
                    deck.client = None;
                    deck.task = None;
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
        self.client = None;
        self.session.status = Status::Connecting;
        self.sync_live_lane();
        cx.notify();
        let restarted = cx.background_executor().spawn(async move { task.restart() });
        cx.spawn(async move |this, cx| {
            let result = restarted.await;
            let _ = this.update(cx, |deck, cx| {
                // App quit can leave the entity alive after detaching it.
                if deck.task.is_none() {
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

    fn connect(&mut self, (client, mut rx): Connection, cx: &mut Context<Self>) {
        self.attachment += 1;
        let attachment = self.attachment;
        self.session = Session::new();
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
