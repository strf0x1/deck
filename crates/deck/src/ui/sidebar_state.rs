//! The window's sidebar inputs: the needs section and the project groups as
//! `sidebar.rs` takes them, built from the last fold and task list (ADR 0010
//! §1, §7, §9).
//!
//! The design choice: this is the one place that reads `deckd`'s records for
//! the sidebar's counts, and it counts a task the way the sessions column
//! lists one — unsettled and held by the project — so the rows stay a pure
//! function of plain inputs.

use super::attention;
use super::root::Deck;
use super::sidebar::{NeedIn, ProjectIn};
use deck_runtime::State;
use std::collections::HashMap;

impl Deck {
    /// What the sidebar lists: every attention item with its task's title and
    /// reason in a word, and every visible project with its unsettled task
    /// count, in the order the sidebar shows them.
    pub fn sidebar_inputs(&mut self) -> (Vec<NeedIn>, Vec<ProjectIn>) {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for record in &self.tasks {
            let settled = attention::settled(record, self.published.get(&record.id)).is_some();
            if record.state == State::Destroyed || settled {
                continue;
            }
            if let Some(id) = self.projects.task_project(record) {
                *counts.entry(id).or_default() += 1;
            }
        }
        let names: HashMap<String, String> =
            self.projects().iter().map(|project| (project.id.clone(), project.name.clone())).collect();
        let projects = self
            .projects()
            .iter()
            .map(|project| ProjectIn {
                id: project.id.clone(),
                name: project.name.clone(),
                tasks: counts.get(&project.id).copied().unwrap_or(0),
            })
            .collect();
        let needs = self
            .needs_you
            .iter()
            .map(|item| NeedIn {
                task: item.task.clone(),
                project: item.project.clone(),
                // A hidden or forgotten project still names something the eye can use.
                project_name: names.get(&item.project).cloned().unwrap_or_else(|| item.project.clone()),
                title: self.task_title(&item.task),
                word: item.reason.word(),
                text: item.reason.text(),
            })
            .collect();
        (needs, projects)
    }
}
