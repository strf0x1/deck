//! A coordinator's `create` (ADR 0011 §4, deck#152 R3): record a worker in a
//! project of its scope with that project's agent command, start it, and
//! deliver the brief as its first prompt, `from` the coordinator.
//!
//! The design choice: the checks and the record happen under one service
//! lock, so two concurrent creates cannot both pass a cap, and the reply is
//! the id as soon as the task is recorded. Starting it and delivering the
//! brief happen on a thread of their own: an agent that fails to start leaves
//! the worker `failed` in its summary, which wakes a `wait`, and nothing here
//! waits on the agent.

use super::control::{Context, Decided, internal};
use crate::control::{self, Requested};
use crate::coordinator::Policy;
use crate::wire::PromptReply;
use crate::{Spec, State};
use serde_json::json;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long the brief may wait for the worker's session.
const BRIEF_WAIT: Duration = Duration::from_millis(control::SESSION_WAIT_MS);

pub(super) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

pub(super) fn create(
    cx: &Context,
    coordinator: &str,
    policy: &Policy,
    project: &str,
    brief: &str,
    title: Option<&str>,
    requested: Option<Requested>,
) -> Decided {
    let id = {
        let service = cx.service.lock().unwrap_or_else(|e| e.into_inner());
        let recorded = service.projects.get(project).cloned();
        let workers: Vec<(State, u64)> = service
            .runtime
            .tasks()
            .map_err(internal)?
            .into_iter()
            .filter(|task| task.spec.created_by.as_deref() == Some(coordinator))
            .map(|task| {
                let created = service.runtime.created_ms(&task.id).unwrap_or(0);
                (task.state, created)
            })
            .collect();
        let argv = recorded.as_ref().map(|project| project.argv.as_slice());
        let checkout = recorded.as_ref().and_then(|p| p.checkouts.first());
        // A worker runs where its coordinator runs (ADR 0016).
        let backend = service.runtime.task(coordinator).map_err(internal)?.backend;
        let workspace = match (backend.as_str(), checkout) {
            ("local", Some(path)) => crate::worktree::Workspace::capture_project(path, project)
                .map_err(|error| control::Refusal::new(control::Code::Refused, format!("{error:#}")))?,
            _ => None,
        };
        let in_place = (backend == "local" && workspace.is_none()).then(|| in_checkout(&service.runtime, checkout));
        let budget = control::admit_create(policy, argv, requested, &workers, in_place, now_ms())?;
        let (Some(recorded), Some(cwd)) = (&recorded, checkout) else {
            return Err(control::Refusal::new(control::Code::Refused, "the project has no checkout Deck knows"));
        };
        let spec = Spec {
            argv: recorded.argv.clone(),
            cwd: cwd.clone(),
            project: Some(project.to_owned()),
            created_by: Some(coordinator.to_owned()),
            backend: Some(backend),
            ..Default::default()
        };
        let id = service.runtime.create_task(spec, None, workspace.map(Box::new)).map_err(internal)?;
        let titled = title
            .map(str::to_owned)
            .or_else(|| brief.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_owned));
        let recorded = service
            .runtime
            .set_budget(&id, budget.requests, budget.tokens)
            .and_then(|()| titled.map_or(Ok(()), |title| service.runtime.title(&id, &title)));
        if let Err(error) = recorded {
            eprintln!("deckd: recording worker {id}'s budget or title: {error:#}");
        }
        id
    };
    cx.briefs.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone());
    let (cx2, worker, from, brief) = (cx.clone(), id.clone(), coordinator.to_owned(), brief.to_owned());
    let started = std::thread::Builder::new().name("deckd-worker-start".into()).spawn(move || {
        start_and_brief(&cx2, &worker, &from, &brief);
        cx2.briefs.lock().unwrap_or_else(|e| e.into_inner()).remove(&worker);
    });
    if let Err(error) = started {
        cx.briefs.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        eprintln!("deckd: starting worker {id}: {error}");
    }
    Ok(json!({ "id": id }))
}

/// Live workers, any coordinator's, running in `checkout` on the local
/// backend: they would share it with the next one (deck#200).
pub(super) fn in_checkout(runtime: &crate::Runtime, checkout: Option<&std::path::PathBuf>) -> usize {
    let Some(checkout) = checkout else { return 0 };
    runtime.tasks().map_or(0, |tasks| {
        tasks
            .iter()
            .filter(|task| task.spec.created_by.is_some() && task.backend == "local" && task.spec.cwd == *checkout)
            .filter(|task| matches!(task.state, State::Created | State::Running))
            .count()
    })
}

/// Start the worker, then send the brief once its holder can take it. A
/// failed start is already on the worker's record.
fn start_and_brief(cx: &Context, id: &str, from: &str, brief: &str) {
    let launched = cx.service.lock().unwrap_or_else(|e| e.into_inner()).launch(id, false);
    if let Err(error) = launched {
        eprintln!("deckd: worker {id} did not start: {error:#}");
        return;
    }
    deliver_brief(cx, id, from, brief);
}

pub(super) fn deliver_brief(cx: &Context, id: &str, from: &str, brief: &str) {
    let deadline = Instant::now() + BRIEF_WAIT;
    loop {
        match crate::daemon_turns::prompt_running(&cx.runtime, id, brief, Some(from)) {
            Ok(PromptReply::Started { .. }) => return,
            Ok(PromptReply::Busy) => {}
            Err(error) => {
                if cx.runtime.task(id).is_ok_and(|task| task.state != State::Running) {
                    eprintln!("deckd: worker {id} stopped before its brief: {error:#}");
                    return;
                }
                if Instant::now() >= deadline {
                    eprintln!("deckd: worker {id}'s brief was not delivered: {error:#}");
                    return;
                }
            }
        }
        if Instant::now() >= deadline {
            eprintln!("deckd: worker {id} stayed busy; its brief was not delivered");
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
