//! Reserve VM integration workers under the daemon's worker admission lock.
//! Prepare the checkout off that lock, then launch through the ordinary
//! worker path once the task record contains every pin and a ready checkout.

use super::control::{Context, Decided, internal};
use super::control_create::{deliver_brief, now_ms};
use crate::control::{self, Code, Refusal, Requested};
use crate::coordinator::Policy;
use crate::integration::{self, Destination, Input, Phase, Plan, Reference, Target, remote};
use crate::{Spec, State, Task, TaskKind};
use anyhow::{Context as _, Result};
use serde_json::json;
use std::fmt::Write as _;
use std::path::Path;

pub(super) struct Request<'a> {
    pub project: &'a str,
    pub destination: &'a Destination,
    pub contributors: &'a [String],
    pub brief: &'a str,
    pub title: Option<&'a str>,
    pub budget: Option<Requested>,
}

pub(super) fn integrate(cx: &Context, coordinator: &str, policy: &Policy, request: Request<'_>) -> Decided {
    let (task, dispatch) = {
        let service = cx.service.lock().unwrap_or_else(|e| e.into_inner());
        let project =
            service.projects.get(request.project).ok_or_else(|| refused("Deck does not know this project."))?;
        let cwd = project.checkouts.first().ok_or_else(|| refused("The project has no checkout Deck knows."))?;
        if matches!(request.destination, Destination::LocalBranch { .. }) {
            return Err(refused("Local integration requires managed contributor worktrees (Deck #207)."));
        }
        if !service.runtime.backends().contains(&"vm") {
            return Err(refused("Deck has no VM backend configured for this destination."));
        }
        let overrides = crate::github::Override::from_env().map_err(internal)?;
        let mut github = crate::github::resolve(cwd, overrides.as_ref())
            .map_err(internal)?
            .ok_or_else(|| refused("The project has no GitHub repository configured."))?;
        github.branches.clear();
        let reference = match request.destination {
            Destination::NewPullRequest {} => Reference::Base { branch: github.base.clone() },
            Destination::ExistingPullRequest { number } => Reference::Pull { number: *number },
            Destination::LocalBranch { .. } => unreachable!("local integration returned earlier"),
        };
        let target = Target { repository: github.repository.clone(), reference };
        let tasks = service.runtime.tasks().map_err(internal)?;
        let matching = tasks.iter().find(|task| {
            integration::reserves(&task.state) && task.integration.as_ref().is_some_and(|plan| plan.target == target)
        });
        let task = if let Some(matching) = matching {
            let same = matching.spec.created_by.as_deref() == Some(coordinator)
                && matching.spec.project.as_deref() == Some(request.project)
                && matching
                    .integration
                    .as_ref()
                    .is_some_and(|plan| plan.inputs.iter().map(|input| &input.id).eq(request.contributors));
            if !same {
                let message = if control::visible(policy, coordinator, matching) {
                    format!("Integration task {} already holds this destination.", matching.id)
                } else {
                    "An integration task already holds this destination.".into()
                };
                return Err(Refusal::new(Code::Busy, message));
            }
            matching.clone()
        } else {
            let inputs = request
                .contributors
                .iter()
                .map(|id| {
                    let source = tasks.iter().find(|task| task.id == *id).ok_or_else(control::unknown)?;
                    Ok(Input { id: id.clone(), commit: published(source, request.project, &target.repository)? })
                })
                .collect::<Result<Vec<_>, Refusal>>()?;
            let workers: Vec<(State, u64)> = tasks
                .iter()
                .filter(|task| task.spec.created_by.as_deref() == Some(coordinator))
                .map(|task| (task.state.clone(), service.runtime.created_ms(&task.id).unwrap_or(0)))
                .collect();
            let budget = control::admit_create(policy, Some(&project.argv), request.budget, &workers, None, now_ms())?;
            let spec = Spec {
                argv: project.argv.clone(),
                cwd: cwd.clone(),
                project: Some(request.project.into()),
                created_by: Some(coordinator.into()),
                backend: Some("vm".into()),
                github: Some(github),
                ..Spec::default()
            };
            let plan = Plan { target, brief: request.brief.into(), inputs, base_commit: None, phase: Phase::Preparing };
            let id = service.runtime.create_with_plan(spec, Some(Box::new(plan))).map_err(internal)?;
            if let Err(error) = service.runtime.set_budget(&id, budget.requests, budget.tokens) {
                let _ = service.runtime.destroy(&id);
                return Err(internal(error));
            }
            if let Err(error) = service.runtime.title(&id, request.title.unwrap_or("Integrate worker changes")) {
                eprintln!("deckd: recording integration task title: {error:#}");
            }
            service.runtime.task(&id).map_err(internal)?
        };
        let first_prompt_missing = task.state == State::Running
            && task.turn.as_ref().is_none_or(|turn| turn.number == 0)
            && service.runtime.prompts(&task.id).is_ok_and(|prompts| prompts.is_empty());
        let dispatch = (task.state == State::Created || first_prompt_missing)
            && cx.briefs.lock().unwrap_or_else(|e| e.into_inner()).insert(task.id.clone());
        (task, dispatch)
    };
    let id = task.id.clone();
    if dispatch {
        let worker = cx.clone();
        let start = std::thread::Builder::new().name("deckd-integration".into()).spawn(move || {
            if let Err(error) = prepare_and_launch(&worker, &task)
                && worker.runtime.task(&task.id).is_ok_and(|task| task.state != State::Destroyed)
            {
                eprintln!("deckd: preparing integration {}: {error:#}", task.id);
            }
            worker.briefs.lock().unwrap_or_else(|e| e.into_inner()).remove(&task.id);
        });
        if let Err(error) = start {
            cx.briefs.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            return Err(internal(error.into()));
        }
    }
    Ok(json!({ "id": id }))
}

fn refused(reason: &str) -> Refusal {
    Refusal::new(Code::Refused, reason)
}

fn published(task: &Task, project: &str, repository: &str) -> Result<Option<String>, Refusal> {
    if task.spec.project.as_deref() != Some(project) {
        return Err(control::unknown());
    }
    if task.spec.kind != TaskKind::Task {
        return Err(refused("A coordinator cannot be a contributor."));
    }
    if task.backend != "vm" || !task.spec.github.as_ref().is_some_and(|github| github.repository == repository) {
        return Err(refused("A contributor is from an incompatible repository or backend."));
    }
    Ok(task.own_branch_commit.as_ref().filter(|sha| integration::is_commit(sha)).cloned())
}

fn prepare_and_launch(cx: &Context, task: &Task) -> Result<()> {
    let mut plan = task.integration.as_deref().context("Integration task has no plan")?.clone();
    if plan.phase != Phase::Ready {
        plan.phase = Phase::Preparing;
        cx.runtime.record_integration(&task.id, plan.clone())?;
        if let Err(error) = prepare(cx, task, &mut plan) {
            plan.waiting(&error.to_string());
            return cx.runtime.record_integration(&task.id, plan);
        }
        plan.phase = Phase::Ready;
        cx.runtime.record_integration(&task.id, plan.clone())?;
    }
    let coordinator = task.spec.created_by.as_deref().context("Integration has no coordinator")?;
    let brief = worker_brief(cx, &plan);
    {
        let mut service = cx.service.lock().unwrap_or_else(|e| e.into_inner());
        let current = cx.runtime.task(&task.id)?;
        if current.state == State::Created {
            service.launch(&task.id, false)?;
        } else if current.state != State::Running || current.turn.as_ref().is_some_and(|turn| turn.number > 0) {
            return Ok(());
        }
    }
    deliver_brief(cx, &task.id, coordinator, &brief);
    Ok(())
}

fn prepare(cx: &Context, task: &Task, plan: &mut Plan) -> Result<()> {
    let project = task.spec.project.as_deref().context("Integration has no project")?;
    let github = task.spec.github.as_ref().context("Integration has no GitHub policy")?;
    for input in &mut plan.inputs {
        if input.commit.is_none() {
            let source =
                cx.runtime.task(&input.id).with_context(|| format!("Contributor {} is unavailable.", input.id))?;
            input.commit =
                published(&source, project, &plan.target.repository).map_err(|err| anyhow::anyhow!(err.message))?;
        }
    }
    cx.runtime.record_integration(&task.id, plan.clone())?;
    if let Some(missing) = plan.inputs.iter().find(|input| input.commit.is_none()) {
        anyhow::bail!("Contributor {} has no published task branch.", missing.id);
    }
    let (broker, path) = {
        let service = cx.service.lock().unwrap_or_else(|e| e.into_inner());
        (service.broker.clone(), service.data.join("tasks").join(&task.id).join(remote::IMPORT))
    };
    let token = match broker.value(&github.credential) {
        Ok(token) => Some(token),
        Err(crate::broker::Refusal::Missing) => None,
        Err(error) => anyhow::bail!("GitHub credential unavailable ({}).", error.code()),
    };
    if plan.base_commit.is_none() {
        plan.base_commit = Some(remote::starting_commit(&plan.target, token.as_deref())?);
        cx.runtime.record_integration(&task.id, plan.clone())?;
    }
    remote::prepare(Path::new(&path), plan, &crate::github::task_branch(&task.id), token.as_deref())?;
    if token.is_some() && broker.value(&github.credential).ok() != token {
        anyhow::bail!("The GitHub credential changed during integration preparation.");
    }
    Ok(())
}

fn worker_brief(cx: &Context, plan: &Plan) -> String {
    let mut brief = format!(
        "Integrate these pinned commits in order on your current task branch. Keep each commit in the result's ancestry. Resolve conflicts, run the combined tests, and report your resolutions and test results. If a decision is needed, describe it and pause.\n\nStarting commit: {}\n",
        plan.base_commit.as_deref().unwrap_or_default()
    );
    for input in &plan.inputs {
        let messages = cx.runtime.prompts(&input.id).unwrap_or_default();
        let context = messages.first().map_or("", |prompt| prompt.text.as_str());
        let _ = writeln!(
            brief,
            "- Task {}: {}\n  Original brief for context: {}",
            input.id,
            input.commit.as_deref().unwrap_or_default(),
            json!(context.chars().take(1000).collect::<String>())
        );
    }
    let publication = match plan.target.reference {
        Reference::Base { .. } => "Publish your task branch and open a draft PR after the tests pass.",
        Reference::Pull { .. } => {
            "Publish only your task branch for review after the tests pass. Leave the existing PR and its head untouched."
        }
    };
    let _ = writeln!(brief, "\n{publication}\n\nIntegration brief:\n{}", plan.brief);
    brief
}
