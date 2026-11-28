//! Commands for tasks in the selected data root.

use crate::{DaemonClient, Phase, State, TaskKind};
use anyhow::{Result, bail};

mod memory;

const USAGE: &str = "deck task list | resume <id>";

/// List or resume tasks, starting the daemon if needed.
pub fn run(args: &[String]) -> Result<()> {
    match args {
        [command] if command == "list" => list(),
        [command, id] if command == "resume" && !id.is_empty() => {
            let client = DaemonClient::connect_or_start_for_cli(&crate::data_root()?)?;
            let task = client.resume(id)?;
            println!("resumed task {}", task.id);
            Ok(())
        }
        _ => bail!("usage: {USAGE}"),
    }
}

fn list() -> Result<()> {
    let client = DaemonClient::connect_or_start_for_cli(&crate::data_root()?)?;
    let projects = client.projects()?;
    let tasks = client.tasks()?;
    if tasks.is_empty() {
        println!("No tasks.");
        return Ok(());
    }

    println!("ID\tPROJECT\tKIND\tBACKEND\tSTATE\tPHASE\tMEMORY (PROCESS)");
    for task in tasks {
        let project = task
            .spec
            .project
            .as_ref()
            .and_then(|id| projects.iter().find(|project| &project.id == id).map(|project| project.name.as_str()))
            .or(task.spec.project.as_deref())
            .unwrap_or("-");
        let kind = match task.spec.kind {
            TaskKind::Task => "task",
            TaskKind::Coordinator => "coordinator",
        };
        let state = match task.state {
            State::Created => "created",
            State::Running => "running",
            State::Stopped { .. } => "stopped",
            State::Failed { .. } => "failed",
            State::Destroyed => "destroyed",
        };
        let phase = match (matches!(task.state, State::Running), task.turn.as_ref().map(|turn| turn.state)) {
            (true, Some(Phase::Working)) => "working",
            (true, Some(Phase::Waiting)) => "waiting",
            (true, Some(Phase::Idle)) => "idle",
            _ => "-",
        };
        let memory =
            task.pid.filter(|_| task.backend == "vm" && matches!(task.state, State::Running)).and_then(memory::process);
        let memory = memory.map_or_else(|| "-".into(), |bytes| format!("{:.1} MiB", bytes as f64 / 1_048_576.0));
        println!("{}\t{}\t{}\t{}\t{}\t{}\t{}", task.id, project, kind, task.backend, state, phase, memory);
    }
    println!("Idle deadlines and keep-running settings are not available yet.");
    Ok(())
}
