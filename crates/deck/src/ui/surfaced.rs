//! What the coordinator's conversation surfaces for the user to decide
//! (ADR 0011 §9): its workers' pending approvals and permission questions,
//! and a `create` refused at a cap.
//!
//! The design choice: the rows come from the window's own attention items
//! (ADR 0010 §7) and `deckd`'s cap notice, never from the coordinator's text,
//! so a decision shows whether or not the coordinator mentions it, and goes
//! the moment it is answered. Every open decision in the project is listed,
//! oldest first, not only those since the coordinator's latest turn: a
//! decision that waits is still the user's to make.

use super::attention::{Item, Reason};
use deck_runtime::Task;
use deck_runtime::control::CapNotice;
use std::collections::HashMap;

/// A worker's open decision, as its row says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub task: String,
    pub title: String,
    /// What it asks, before the subject: `wants network:` or `asks to run`.
    pub asks: String,
    /// The host, repository or command.
    pub subject: String,
    /// Whether the subject is a network destination (orange) or a command.
    pub network: bool,
}

/// The open decisions of `coordinator`'s project, oldest first. `gateways`
/// names the gateway of each task's pending approval, when the window knows.
pub fn decisions(
    items: &[Item],
    tasks: &[Task],
    coordinator: &Task,
    gateways: &HashMap<String, String>,
) -> Vec<Decision> {
    let project = coordinator.spec.project_key();
    items
        .iter()
        .filter(|item| item.project == project && item.task != coordinator.id)
        .filter_map(|item| {
            let title = tasks.iter().find(|t| t.id == item.task).map(super::tasks::title)?;
            let (asks, subject, network) = match &item.reason {
                Reason::Approval { what } => {
                    let gateway = gateways.get(&item.task).map_or("access", |g| gateway(g));
                    (format!("wants {gateway}:"), what.clone(), true)
                }
                Reason::Question { what } => ("asks to run".to_string(), what.clone(), false),
                Reason::Finished { .. } | Reason::Failed { .. } => return None,
            };
            Some(Decision { task: item.task.clone(), title, asks, subject, network })
        })
        .collect()
}

/// A gateway's name as a person reads it.
fn gateway(name: &str) -> &str {
    match name {
        "web" => "network",
        "github" => "GitHub",
        "model" => "a model",
        other => other,
    }
}

/// The cap row's words: which limit the coordinator met.
pub fn limit_text(cap: &CapNotice) -> String {
    match cap.cap.as_str() {
        "creates_per_hour" => format!("Coordinator reached its limit of {} new sessions an hour", cap.limit),
        _ => format!("Coordinator reached its limit of {} running sessions", cap.limit),
    }
}

/// Whether the cap row still stands: a concurrency cap goes once fewer of
/// the coordinator's workers run than the limit (a session ended), and
/// either cap goes once the user raised it above the limit met.
pub fn limit_stands(cap: &CapNotice, tasks: &[Task], raised_to: Option<u32>) -> bool {
    if raised_to.is_some_and(|value| value > cap.limit) {
        return false;
    }
    if cap.cap == "max_concurrent" {
        let running = tasks
            .iter()
            .filter(|t| t.spec.created_by.as_deref() == Some(cap.coordinator.as_str()))
            .filter(|t| t.state == deck_runtime::State::Running)
            .count();
        return running >= cap.limit as usize;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::time::Instant;

    fn task(id: &str, project: &str, title: &str, state: Value, created_by: Option<&str>) -> Task {
        serde_json::from_value(json!({
            "id": id, "backend": "local", "title": title,
            "spec": { "argv": ["x"], "cwd": "/src", "project": project, "created_by": created_by },
            "state": state, "pid": null,
        }))
        .unwrap()
    }

    fn item(task: &str, project: &str, reason: Reason) -> Item {
        Item { task: task.into(), project: project.into(), reason, since: Instant::now() }
    }

    #[test]
    fn a_projects_open_decisions_are_surfaced_and_nothing_else() {
        let coordinator = task("c", "git:a", "coordinator", json!({ "state": "running" }), None);
        let tasks = vec![
            coordinator.clone(),
            task("w1", "git:a", "npm audit fix", json!({ "state": "running" }), Some("c")),
            task("w2", "git:a", "fix rail drag", json!({ "state": "running" }), None),
            task("x", "git:b", "elsewhere", json!({ "state": "running" }), None),
        ];
        let items = vec![
            item("w1", "git:a", Reason::Approval { what: "registry.npmjs.org".into() }),
            item("w2", "git:a", Reason::Question { what: "rm -rf build".into() }),
            item("w2", "git:a", Reason::Finished { turn: 2, files: None }),
            item("x", "git:b", Reason::Approval { what: "example.com".into() }),
            item("c", "git:a", Reason::Question { what: "ls".into() }),
        ];
        let gateways = HashMap::from([("w1".to_string(), "web".to_string())]);
        let rows = decisions(&items, &tasks, &coordinator, &gateways);
        assert_eq!(rows.len(), 2, "another project's, a finished turn and the coordinator's own are not: {rows:?}");
        assert_eq!(
            (rows[0].title.as_str(), rows[0].asks.as_str(), rows[0].subject.as_str(), rows[0].network),
            ("npm audit fix", "wants network:", "registry.npmjs.org", true)
        );
        assert_eq!((rows[1].asks.as_str(), rows[1].network), ("asks to run", false), "a user's task counts too");
    }

    #[test]
    fn the_cap_row_goes_once_raised_or_once_a_session_ends() {
        let cap = CapNotice { coordinator: "c".into(), cap: "max_concurrent".into(), limit: 2 };
        assert_eq!(limit_text(&cap), "Coordinator reached its limit of 2 running sessions");
        let running = |id: &str| task(id, "git:a", id, json!({ "state": "running" }), Some("c"));
        let mut tasks = vec![running("w1"), running("w2")];
        assert!(limit_stands(&cap, &tasks, None));
        assert!(limit_stands(&cap, &tasks, Some(2)), "saving the same value raises nothing");
        assert!(!limit_stands(&cap, &tasks, Some(3)));
        tasks[1].state = deck_runtime::State::Stopped { clean: true, reason: String::new() };
        assert!(!limit_stands(&cap, &tasks, None), "a session ended");
        let rate = CapNotice { cap: "creates_per_hour".into(), ..cap };
        assert_eq!(limit_text(&rate), "Coordinator reached its limit of 2 new sessions an hour");
        assert!(limit_stands(&rate, &[], None), "only raising it clears a rate");
    }
}
