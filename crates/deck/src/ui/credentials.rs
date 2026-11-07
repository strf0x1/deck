//! Access panel operations run off the UI thread and update from daemon replies.
//!
//! The design choice: credentials and grants load independently, so a Keychain
//! dialog cannot delay grant controls. Each listing has a request ticket to
//! discard stale replies. Only grant listings refresh periodically while the
//! panel is open; closing it cancels that timer.

use super::access::Revoke;
use super::root::Deck;
use deck_runtime::broker::Listed;
use gpui::Context;
use std::time::Duration;

impl Deck {
    pub fn toggle_access(&mut self, cx: &mut Context<Self>) {
        if self.access.toggle() {
            self.refresh_access(cx);
            let executor = cx.background_executor().clone();
            self.access_refresh = Some(cx.spawn(async move |this, cx| {
                loop {
                    executor.timer(Duration::from_secs(30)).await;
                    let open = this
                        .update(cx, |deck, cx| {
                            if !deck.access.is_open() {
                                return false;
                            }
                            deck.load_grants(cx);
                            true
                        })
                        .unwrap_or(false);
                    if !open {
                        break;
                    }
                }
            }));
        } else {
            self.access_refresh = None;
        }
        cx.notify();
    }

    pub fn refresh_access(&mut self, cx: &mut Context<Self>) {
        self.load_grants(cx);
        let Some(task) = self.task.clone() else {
            return;
        };
        let ticket = self.access.begin_credentials();
        let listed = cx.background_executor().spawn(async move { task.credentials() });
        cx.spawn(async move |this, cx| {
            let result = listed.await.map_err(|e| format!("listing credentials: {e:#}"));
            let _ = this.update(cx, |deck, cx| {
                deck.on_credentials(ticket, result, cx);
            });
        })
        .detach();
    }

    /// Apply a credential listing only if it still belongs to the current request.
    pub fn on_credentials(&mut self, ticket: u64, result: Result<Vec<Listed>, String>, cx: &mut Context<Self>) {
        self.access.credentials_listed(ticket, result);
        cx.notify();
    }

    pub(super) fn load_grants(&mut self, cx: &mut Context<Self>) {
        let Some(task) = self.task.clone() else {
            return;
        };
        let ticket = self.access.begin_grants();
        let listed = cx.background_executor().spawn({
            let task = task.clone();
            async move { task.grants() }
        });
        cx.spawn(async move |this, cx| {
            let result = listed.await.map_err(|e| format!("listing access: {e:#}"));
            let _ = this.update(cx, |deck, cx| {
                deck.access.grants_listed(ticket, result);
                cx.notify();
            });
        })
        .detach();
        // The mode is the task's live project policy, not a grant; it is read
        // with the same refresh so a project switched to `ask` shows the new
        // mode on the next open or 30-second refresh.
        let mode_ticket = self.access.begin_approvals();
        let mode = cx.background_executor().spawn(async move { task.github_approvals() });
        cx.spawn(async move |this, cx| {
            let result = mode.await.map_err(|e| format!("reading the GitHub approval mode: {e:#}"));
            let _ = this.update(cx, |deck, cx| {
                deck.access.approvals_listed(mode_ticket, result);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn ask_revoke(&mut self, name: &str, cx: &mut Context<Self>) {
        self.access.ask_credential(name);
        cx.notify();
    }

    pub fn ask_revoke_grant(&mut self, id: &str, cx: &mut Context<Self>) {
        self.access.ask_grant(id);
        cx.notify();
    }

    pub fn cancel_revoke(&mut self, cx: &mut Context<Self>) {
        self.access.cancel();
        cx.notify();
    }

    pub fn review_approval(&mut self, id: &str, cx: &mut Context<Self>) {
        self.boundary.select(id);
        self.toggle_access(cx);
    }

    /// Permit another OS read for a paused credential, then refresh the
    /// listing so the row shows the new state.
    pub fn retry_credential(&mut self, name: &str, cx: &mut Context<Self>) {
        if !self.access.begin_retry(name) {
            return cx.notify();
        }
        let Some(task) = self.task.clone() else {
            self.access.retry_done(Err("no task is attached to retry through".into()));
            return cx.notify();
        };
        cx.notify();
        let name = name.to_string();
        let done = cx.background_executor().spawn(async move { task.retry_credential(&name) });
        cx.spawn(async move |this, cx| {
            let result = done.await.map_err(|e| format!("retrying the credential: {e:#}"));
            let _ = this.update(cx, |deck, cx| {
                deck.access.retry_done(result);
                deck.refresh_access(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn confirm_revoke(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.access.begin_revoke() else {
            return;
        };
        let Some(task) = self.task.clone() else {
            self.access.revoke_done(Err("no task is attached to revoke through".into()));
            return cx.notify();
        };
        cx.notify();
        let done = cx.background_executor().spawn(async move {
            match target {
                Revoke::Credential(name) => task.revoke_credential(&name),
                Revoke::Grant(grant) => task.revoke_grant(&grant.id),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = done.await.map_err(|e| format!("revoking access: {e:#}"));
            let _ = this.update(cx, |deck, cx| {
                deck.access.revoke_done(result);
                deck.refresh_access(cx);
                cx.notify();
            });
        })
        .detach();
    }
}
