//! The access panel's effective grants and confirmed revocation (ADR 0004 §5).
//!
//! The design choice: project rules are visible only through a task in that
//! project. Removing one also removes matching task grants for the project's
//! tasks, so an earlier task allowance cannot silently preserve the access.

use super::*;

#[cfg(test)]
mod tests;

impl Boundary {
    pub fn access_grants(&self, task: &str, project: &str) -> Result<Vec<Grant>> {
        let mut inner = self.lock();
        self.load_task(&mut inner, task)?;
        self.prune(&mut inner, now_ms());
        let mut grants = inner.grants.get(task).cloned().unwrap_or_default();
        grants.extend(inner.projects.iter().filter(|grant| grant.project.as_deref() == Some(project)).cloned());
        grants.retain(|grant| grant.expires_ms.is_none_or(|expires| expires > now_ms()));
        grants.sort_by(|a, b| {
            (a.scope == Scope::Project, &a.gateway, &a.destination, &a.operation, &a.id).cmp(&(
                b.scope == Scope::Project,
                &b.gateway,
                &b.destination,
                &b.operation,
                &b.id,
            ))
        });
        Ok(grants)
    }

    /// Called only after the panel confirms the scope. `peers` comes from
    /// the runtime's task records, never from a guest or a request's payload.
    pub fn revoke_access_grant(&self, task: &str, project: &str, peers: &[String], id: &str) -> Result<()> {
        let mut inner = self.lock();
        self.load_task(&mut inner, task)?;
        if inner.grants.get(task).is_some_and(|grants| grants.iter().any(|grant| grant.id == id)) {
            drop(inner);
            return self.revoke(task, id);
        }
        let Some(rule) =
            inner.projects.iter().find(|grant| grant.id == id && grant.project.as_deref() == Some(project)).cloned()
        else {
            bail!("unknown grant {id:?} for this task or project");
        };
        let mut revoked = Vec::new();
        for peer in peers {
            self.load_task(&mut inner, peer)?;
            let grants = inner.grants.entry(peer.clone()).or_default();
            let (removed, kept): (Vec<_>, Vec<_>) = grants.iter().cloned().partition(|grant| covers(&rule, grant));
            if !removed.is_empty() {
                save_task(&self.data, peer, &kept)?;
                *grants = kept;
                revoked.extend(removed);
            }
        }
        let kept: Vec<_> = inner.projects.iter().filter(|grant| grant.id != id).cloned().collect();
        save_projects(&self.data, &kept, inner.version)?;
        inner.projects = kept;
        revoked.push(rule);
        // An audit failure must not leave the project rule or a peer's
        // matching allowance active, so finish narrowing policy first.
        audit_revoked(&self.data, &revoked, now_ms())
    }
}

fn covers(rule: &Grant, grant: &Grant) -> bool {
    let destination = rule.destination == grant.destination
        || (rule.gateway == GITHUB && rule.destination == types::github_repository(&grant.destination));
    let operation = rule.operation == grant.operation
        || (rule.gateway == "web" && rule.operation == "connect" && grant.operation == "fetch");
    rule.gateway == grant.gateway && operation && destination
}
