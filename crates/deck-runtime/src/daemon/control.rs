//! `deckd`'s `control` operation (ADR 0011 §4, deck#152): a coordinator's
//! gateway forwards one operation, named with the coordinator's task id, and
//! this module decides it against the coordinator's policy, audits it on the
//! coordinator's record, and answers a reply or a coded refusal.
//!
//! The design choice: the service lock is held only to read or change
//! records, never across a `wait` or a prompt's turn start, so a coordinator
//! polling its workers never stalls a window. The rules themselves are pure
//! ([`crate::control`]); this module reads the facts they need.

use super::Service;
use crate::approvals::Boundary;
use crate::control::summary::{self, Facts, Snapshot};
use crate::control::{self, Code, Operation, Outcome, Refusal};
use crate::coordinator::Policy;
use crate::wire::PromptReply;
use crate::{Phase, Runtime, State, Task, TaskKind};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(50);

/// What an operation needs beyond the request.
#[derive(Clone)]
pub(crate) struct Context {
    pub service: Arc<Mutex<Service>>,
    pub boundary: Arc<Boundary>,
    pub reader: Arc<crate::pull_request::Reader>,
    pub runtime: Runtime,
    /// Workers whose brief is not delivered yet: a coordinator's prompt waits
    /// for it rather than racing it into the first turn.
    pub briefs: Arc<Mutex<HashSet<String>>>,
}

pub(crate) type Decided = Result<Value, Refusal>;

/// Decide one forwarded operation, audit it, and announce a cap refusal.
pub(crate) fn control(cx: &Context, coordinator: &str, request: Value) -> Outcome {
    let operation: Operation = match serde_json::from_value(request) {
        Ok(operation) => operation,
        Err(error) => return Outcome::Refused(Refusal::new(Code::BadRequest, format!("{error}"))),
    };
    let decided = operation.validate().and_then(|()| decide(cx, coordinator, &operation));
    let outcome = match &decided {
        Ok(_) => "ok",
        Err(refusal) => refusal.code.name(),
    };
    // A refusal for a task that is not a coordinator has no record to audit.
    if let Err(error) = cx.runtime.audit(coordinator, operation.name(), operation.target(), outcome)
        && cx.runtime.task(coordinator).is_ok_and(|task| task.spec.kind == TaskKind::Coordinator)
    {
        eprintln!("deckd: auditing {} for coordinator {coordinator}: {error:#}", operation.name());
    }
    match decided {
        Ok(reply) => Outcome::Reply(reply),
        Err(refusal) => {
            if let Some((cap, limit)) = refusal.cap {
                let notice = control::CapNotice { coordinator: coordinator.into(), cap: cap.field().into(), limit };
                if let Err(error) = cx.runtime.announce_cap(coordinator, notice) {
                    eprintln!("deckd: announcing coordinator {coordinator}'s cap: {error:#}");
                }
            }
            Outcome::Refused(refusal)
        }
    }
}

/// An internal failure is the host's, and retryable; the guest hears no
/// detail, the log does.
pub(super) fn internal(error: anyhow::Error) -> Refusal {
    eprintln!("deckd: control: {error:#}");
    Refusal::new(Code::Unavailable, "deckd could not complete the operation")
}

fn decide(cx: &Context, coordinator: &str, operation: &Operation) -> Decided {
    let policy = policy(cx, coordinator)?;
    match operation {
        Operation::Tasks { project } => tasks(cx, coordinator, &policy, project.as_deref()),
        Operation::Task { id } => Ok(json!({ "task": summarize(cx, &visible(cx, coordinator, &policy, id)?, true) })),
        Operation::Create { project, brief, title, budget } => {
            let project = control::create_project(&policy, project.as_deref())?;
            super::control_create::create(cx, coordinator, &policy, &project, brief, title.as_deref(), *budget)
        }
        Operation::Integrate { project, destination, contributors, brief, title, budget } => {
            let project = control::create_project(&policy, project.as_deref())?;
            super::control_integrate::integrate(
                cx,
                coordinator,
                &policy,
                super::control_integrate::Request {
                    project: &project,
                    destination,
                    contributors,
                    brief,
                    title: title.as_deref(),
                    budget: *budget,
                },
            )
        }
        Operation::Prompt { id, text } => prompt(cx, coordinator, &policy, id, text),
        Operation::Wait { id, after_turn, wait_ms } => {
            wait(cx, coordinator, &policy, id, after_turn.unwrap_or(0), *wait_ms)
        }
        Operation::Stop { id } => stop(cx, coordinator, &policy, id),
    }
}

/// The caller's policy: it must be a coordinator that was not destroyed.
fn policy(cx: &Context, coordinator: &str) -> Result<Policy, Refusal> {
    let not = || Refusal::new(Code::Refused, format!("task {coordinator} is not a coordinator"));
    let task = cx.runtime.task(coordinator).map_err(|_| not())?;
    if task.spec.kind != TaskKind::Coordinator || task.state == State::Destroyed {
        return Err(not());
    }
    cx.runtime.policy(coordinator).map_err(|_| Refusal::new(Code::Refused, "this coordinator has no policy"))
}

/// A task the caller may see, or `unknown` exactly as a missing one.
pub(super) fn visible(cx: &Context, coordinator: &str, policy: &Policy, id: &str) -> Result<Task, Refusal> {
    match cx.runtime.task(id) {
        Ok(task) if control::visible(policy, coordinator, &task) => Ok(task),
        _ => Err(control::unknown()),
    }
}

/// A task the caller may prompt or stop: never itself, nor any coordinator.
fn drivable(cx: &Context, coordinator: &str, policy: &Policy, id: &str) -> Result<Task, Refusal> {
    if id == coordinator {
        return Err(Refusal::new(Code::Refused, "a coordinator cannot act on itself"));
    }
    let task = visible(cx, coordinator, policy, id)?;
    control::may_drive(coordinator, &task)?;
    Ok(task)
}

fn summarize(cx: &Context, task: &Task, detailed: bool) -> Value {
    let pending = cx.boundary.pending(&task.id).map_or(0, |items| items.len());
    // No gateway read here: a listing of 200 tasks must not wait on GitHub.
    // The reader's last read is what the window last refreshed.
    let dir = cx.runtime_dir(&task.id);
    let live = cx.reader.state(&task.id, &dir, task.publication.clone(), None, false).live;
    let facts = Facts {
        pending_approvals: pending,
        pull_request: live.and_then(|live| live.pull_request),
        message: cx.runtime.last_message(&task.id),
        review_commit: task
            .workspace
            .as_ref()
            .filter(|_| detailed)
            .and_then(|ws| ws.review_commit(task).ok().flatten()),
    };
    summary::summary(task, &facts)
}

impl Context {
    fn runtime_dir(&self, id: &str) -> std::path::PathBuf {
        self.service.lock().unwrap_or_else(|e| e.into_inner()).data.join("tasks").join(id)
    }

    fn snapshot(&self, id: &str) -> Result<Snapshot, Refusal> {
        let task = self.runtime.task(id).map_err(internal)?;
        Ok(Snapshot::of(&task, self.boundary.pending(id).map_or(0, |items| items.len())))
    }
}

fn tasks(cx: &Context, coordinator: &str, policy: &Policy, project: Option<&str>) -> Decided {
    if let Some(project) = project {
        control::project_in_scope(policy, project)?;
    }
    let mut listed: Vec<(u64, Task)> = cx
        .runtime
        .tasks()
        .map_err(internal)?
        .into_iter()
        .filter(|task| control::visible(policy, coordinator, task))
        .filter(|task| project.is_none_or(|project| task.spec.project.as_deref() == Some(project)))
        .map(|task| (cx.runtime.created_ms(&task.id).unwrap_or(0), task))
        .collect();
    listed.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.id.cmp(&a.1.id)));
    let truncated = listed.len() > control::TASKS_LIMIT;
    let tasks = listed.iter().take(control::TASKS_LIMIT).map(|(_, task)| summarize(cx, task, false));
    summary::listing(projects(cx, policy), tasks, truncated).map_err(internal)
}

/// The coordinator's project set with the names the user sees (contract
/// 1.5.0 §13.3), so its first `tasks` already says what `create` takes.
fn projects(cx: &Context, policy: &Policy) -> Vec<Value> {
    let service = cx.service.lock().unwrap_or_else(|e| e.into_inner());
    let name = |id: &str| service.projects.get(id).map_or_else(|| id.to_owned(), |project| project.name.clone());
    policy.projects.iter().map(|id| json!({ "id": id, "name": name(id) })).collect()
}

fn prompt(cx: &Context, coordinator: &str, policy: &Policy, id: &str, text: &str) -> Decided {
    let task = drivable(cx, coordinator, policy, id)?;
    control::promptable(&task)?;
    // A just-created task has no session yet, and its brief comes first.
    let deadline = Instant::now() + Duration::from_millis(control::SESSION_WAIT_MS);
    loop {
        let task = cx.runtime.task(id).map_err(internal)?;
        control::promptable(&task)?;
        if let Some(reason) = task.integration.as_deref().and_then(crate::integration::Plan::reason) {
            return Err(Refusal::new(Code::Busy, reason));
        }
        let briefing = cx.briefs.lock().unwrap_or_else(|e| e.into_inner()).contains(id);
        if matches!(task.state, State::Stopped { .. }) && !briefing {
            resume_worker(cx, coordinator, id)?;
            break;
        }
        if task.state == State::Running && !briefing {
            break;
        }
        if Instant::now() >= deadline {
            return Err(Refusal::new(Code::Busy, "the task's session is not ready yet"));
        }
        std::thread::sleep(POLL);
    }
    match crate::daemon_turns::prompt_running(&cx.runtime, id, text, Some(coordinator)) {
        Ok(PromptReply::Started { turn }) => Ok(json!({ "turn": turn })),
        Ok(PromptReply::Busy) => {
            let task = cx.runtime.task(id).map_err(internal)?;
            let state = task.turn.as_ref().map_or(Phase::Working, |t| t.state);
            let state = if state == Phase::Waiting { "waiting" } else { "working" };
            let question = task
                .permissions
                .as_ref()
                .and_then(|p| p.pending.first())
                .map(|p| format!("; pending user permission: {:?}", p.subject))
                .unwrap_or_default();
            Err(Refusal::new(Code::Busy, format!("the task's turn is {state}{question}")))
        }
        Err(error) => {
            let task = cx.runtime.task(id).map_err(internal)?;
            control::promptable(&task)?;
            Err(internal(error))
        }
    }
}

/// Serialize admission and launch with worker creation, so two control
/// operations cannot consume the same concurrent or checkout slot.
fn resume_worker(cx: &Context, coordinator: &str, id: &str) -> Result<(), Refusal> {
    let service = cx.service.lock().unwrap_or_else(|e| e.into_inner());
    let task = service.runtime.task(id).map_err(internal)?;
    control::promptable(&task)?;
    if !matches!(task.state, State::Stopped { .. }) {
        return Ok(());
    }
    let policy = service.runtime.policy(coordinator).map_err(internal)?;
    let live = service
        .runtime
        .tasks()
        .map_err(internal)?
        .into_iter()
        .filter(|worker| worker.spec.created_by.as_deref() == Some(coordinator))
        .filter(|worker| matches!(worker.state, State::Created | State::Running))
        .count();
    let in_place = (task.backend == "local" && task.workspace.is_none())
        .then(|| super::control_create::in_checkout(&service.runtime, Some(&task.spec.cwd)));
    control::admit_resume(&policy, live, in_place)?;
    service.runtime.running_for_prompt(id).map_err(internal)?;
    Ok(())
}

fn wait(cx: &Context, coordinator: &str, policy: &Policy, id: &str, after_turn: u64, wait_ms: u64) -> Decided {
    if id == coordinator {
        return Err(Refusal::new(Code::Refused, "a coordinator cannot wait on itself"));
    }
    visible(cx, coordinator, policy, id)?;
    let mut notices = cx.runtime.subscribe();
    let start = cx.snapshot(id)?;
    let deadline = Instant::now() + Duration::from_millis(wait_ms.min(control::WAIT_LIMIT_MS));
    let mut now = start.clone();
    let timed_out = loop {
        if summary::woke(&start, &now, after_turn) {
            break false;
        }
        if Instant::now() >= deadline {
            break true;
        }
        // Notices wake it at once; the approval queue has none, so a short
        // poll covers a new pending item.
        std::thread::sleep(POLL);
        while notices.try_recv().is_ok() {}
        now = cx.snapshot(id)?;
    };
    let task = cx.runtime.task(id).map_err(internal)?;
    Ok(json!({ "timed_out": timed_out, "task": summarize(cx, &task, true) }))
}

fn stop(cx: &Context, coordinator: &str, policy: &Policy, id: &str) -> Decided {
    let task = drivable(cx, coordinator, policy, id)?;
    if task.state == State::Running {
        let service = cx.service.lock().unwrap_or_else(|e| e.into_inner());
        let stopped = service.runtime.stop(id).and_then(|()| service.boundary.clear_task(id));
        if let Err(error) = stopped {
            eprintln!("deckd: control stop of {id}: {error:#}");
        }
    }
    let task = cx.runtime.task(id).map_err(internal)?;
    Ok(json!({ "id": id, "state": summary::state_name(&task.state) }))
}
