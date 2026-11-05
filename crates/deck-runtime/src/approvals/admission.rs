//! Request admission and bounded holds. A card records a decision; each
//! operation must separately consume its Once allowance under the queue lock.
//! Requests sharing a card keep their original hold deadline when they ask again.

use super::{Approval, Boundary, Incoming, Inner, Scope, State, millis, now_ms, resolved};
use anyhow::{Result, bail};
use std::time::Duration;

#[cfg(test)]
#[path = "cancellation_tests.rs"]
mod cancellation_tests;

/// Detect a caller that disappeared without waiting for a human decision.
const CALLER_CHECK: Duration = Duration::from_millis(100);

impl Boundary {
    /// Ask for a scope, consuming any matching Once allowance atomically.
    /// Without a grant, share a pending card and hold only while a capable
    /// window is attached, up to the original request's hold deadline.
    pub fn submit(&self, request: Incoming) -> Result<Approval> {
        self.submit_while(request, || true)
    }

    /// The daemon supplies a nonblocking check that the reply's recipient
    /// still exists. A disconnected caller cannot claim an allowance which
    /// another held operation or a retry could use.
    pub(crate) fn submit_while(&self, request: Incoming, connected: impl Fn() -> bool) -> Result<Approval> {
        let mut inner = self.lock();
        let deadline = now_ms().saturating_add(millis(self.settings.hold));
        self.load_task(&mut inner, &request.task)?;
        loop {
            if !connected() {
                bail!("the approval request's caller disconnected");
            }
            let now = now_ms();
            self.prune(&mut inner, now);
            if let Some(scope) = self.take_grant(&mut inner, &request, now) {
                return Ok(resolved(&request, now, inner.version, scope));
            }
            let id = self.enqueue(&mut inner, &request, now);
            if inner.leases.get(&request.task).copied().unwrap_or(0) == 0 || now >= deadline {
                return Ok(inner.items[&id].clone());
            }
            *inner.holds.entry(id.clone()).or_default() += 1;
            let outcome = loop {
                if !connected() {
                    break Err(anyhow::anyhow!("the approval request's caller disconnected"));
                }
                let Some(item) = inner.items.get(&id).cloned() else {
                    break Err(anyhow::anyhow!("approval {id:?} was cleared while the request waited"));
                };
                let now = now_ms();
                if !item.state.pending() || now >= deadline {
                    break Ok(item);
                }
                let left = deadline.saturating_sub(now).min(item.expires_ms.saturating_sub(now)).max(1);
                let (guard, _) = self
                    .wake
                    .wait_timeout(inner, Duration::from_millis(left).min(CALLER_CHECK))
                    .unwrap_or_else(|error| error.into_inner());
                inner = guard;
                self.prune(&mut inner, now_ms());
            };
            if let Some(count) = inner.holds.get_mut(&id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    inner.holds.remove(&id);
                }
            }
            let mut item = outcome?;
            if item.state == State::Allowed(Scope::Once) {
                // A decision wakes every held request. Only a request that
                // claims an allowance can proceed; the rest need a fresh card.
                if !connected() {
                    bail!("the approval request's caller disconnected");
                }
                let Some(scope) = self.take_grant(&mut inner, &request, now_ms()) else { continue };
                item.state = State::Allowed(scope);
            }
            return Ok(item);
        }
    }

    fn enqueue(&self, inner: &mut Inner, request: &Incoming, now: u64) -> String {
        if let Some(item) = inner
            .items
            .values()
            .find(|item| item.state.pending() && item.expires_ms > now && item.same_request(request))
        {
            return item.id.clone();
        }
        inner.seq += 1;
        let id = format!("{:x}-{:x}", now, inner.seq);
        let item = Approval {
            id: id.clone(),
            task: request.task.clone(),
            project: request.project.clone(),
            session: request.session.clone(),
            gateway: request.gateway.clone(),
            operation: request.operation.clone(),
            destination: request.destination.clone(),
            credential: request.credential.clone(),
            details: request.details.clone(),
            reason: request.reason.clone(),
            created_ms: now,
            expires_ms: now.saturating_add(millis(self.settings.pending)),
            policy: inner.version,
            state: State::Pending,
            waiters: 0,
        };
        self.notify(inner, &item);
        inner.items.insert(id.clone(), item);
        id
    }
}
