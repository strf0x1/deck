//! The window's side of the access panel: asking the daemon for this task's
//! credential listing and for a revoke, off the UI thread.
//!
//! The design choice: the daemon is the authority. A click only asks; the
//! panel changes when the daemon's reply (the new listing) arrives, so it
//! never shows a value as revoked that the daemon has not revoked.

use super::root::Deck;
use gpui::Context;

impl Deck {
    pub fn toggle_access(&mut self, cx: &mut Context<Self>) {
        if self.access.toggle() {
            self.load_credentials(cx);
        }
        cx.notify();
    }

    fn load_credentials(&mut self, cx: &mut Context<Self>) {
        let Some(task) = self.task.clone() else { return };
        let listed = cx.background_executor().spawn(async move { task.credentials() });
        cx.spawn(async move |this, cx| {
            let result = listed.await.map_err(|e| format!("listing credentials: {e:#}"));
            let _ = this.update(cx, |deck, cx| {
                deck.access.listed(result);
                cx.notify();
            });
        })
        .detach();
    }

    /// Open the revoke confirmation for `name`; nothing is sent yet.
    pub fn ask_revoke(&mut self, name: &str, cx: &mut Context<Self>) {
        self.access.ask_revoke(name);
        cx.notify();
    }

    pub fn cancel_revoke(&mut self, cx: &mut Context<Self>) {
        self.access.cancel();
        cx.notify();
    }

    /// Send the confirmed revoke. The panel updates from the daemon's reply.
    pub fn confirm_revoke(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.access.begin_revoke() else { return };
        let Some(task) = self.task.clone() else {
            self.access.revoke_done(Err("no task is attached to revoke through".into()));
            return cx.notify();
        };
        cx.notify();
        let done = cx.background_executor().spawn(async move {
            task.revoke_credential(&name)?;
            task.credentials()
        });
        cx.spawn(async move |this, cx| {
            let result = done.await.map_err(|e| format!("revoking credential: {e:#}"));
            let _ = this.update(cx, |deck, cx| {
                deck.access.revoke_done(result);
                cx.notify();
            });
        })
        .detach();
    }

    /// Apply a daemon answer to a revoke, for tests that have no daemon.
    pub fn on_revoke_done(
        &mut self,
        result: Result<Vec<deck_runtime::broker::Listed>, String>,
        cx: &mut Context<Self>,
    ) {
        self.access.revoke_done(result);
        cx.notify();
    }

    /// Apply a listing, for tests that have no daemon.
    pub fn on_credentials(
        &mut self,
        result: Result<Vec<deck_runtime::broker::Listed>, String>,
        cx: &mut Context<Self>,
    ) {
        self.access.listed(result);
        cx.notify();
    }
}
