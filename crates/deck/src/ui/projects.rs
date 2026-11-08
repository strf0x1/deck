//! The window's project rail state: the visible projects, which one is open, and the sessions column's filter (ADR 0010 §1, §3, §9).
//!
//! The design choice: `deckd` owns the project list, so this holds the last
//! reply and the selection; a task recorded before `Spec.project` existed is
//! placed by resolving its checkout once and caching the answer.

use deck_runtime::project::{self, Project};
use deck_runtime::{State, Task};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// What the rail shows and which project's tasks the column lists.
#[derive(Default)]
pub struct Projects {
    /// Visible projects, most recently opened first.
    list: Vec<Project>,
    selected: Option<String>,
    /// A checkout's project id, resolved once for a task that has none.
    resolved: HashMap<PathBuf, Option<String>>,
    /// Why the last open failed, shown in the header until the next try.
    error: Option<String>,
}

impl Projects {
    /// Replace the rail's projects and select `select` when it is visible.
    pub fn set(&mut self, mut list: Vec<Project>, select: Option<String>) {
        list.retain(|project| !project.hidden);
        list.sort_by(|a, b| b.opened_ms.cmp(&a.opened_ms).then_with(|| a.id.cmp(&b.id)));
        let select = select.filter(|id| list.iter().any(|project| &project.id == id));
        self.list = list;
        self.selected = select;
    }

    /// Add or replace `project` and select it: a freshly opened project sorts
    /// to the top by its open time.
    pub fn add(&mut self, mut project: Project) {
        project.hidden = false;
        let id = project.id.clone();
        match self.list.iter().position(|p| p.id == id) {
            Some(index) => self.list[index] = project,
            None => self.list.push(project),
        }
        self.list.sort_by(|a, b| b.opened_ms.cmp(&a.opened_ms).then_with(|| a.id.cmp(&b.id)));
        self.selected = Some(id);
        self.error = None;
    }

    pub fn list(&self) -> &[Project] {
        &self.list
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    pub fn selected(&self) -> Option<&Project> {
        let id = self.selected.as_deref()?;
        self.list.iter().find(|project| project.id == id)
    }

    /// Select a project already in the list; false for an unknown id.
    pub fn select(&mut self, id: &str) -> bool {
        if !self.list.iter().any(|project| project.id == id) {
            return false;
        }
        self.selected = Some(id.to_string());
        true
    }

    pub fn select_index(&mut self, index: usize) -> Option<String> {
        let id = self.list.get(index)?.id.clone();
        self.select(&id).then_some(id)
    }

    /// The selected project's index in the rail.
    pub fn index(&self) -> Option<usize> {
        let id = self.selected.as_deref()?;
        self.list.iter().position(|project| project.id == id)
    }

    /// The selected project's first existing checkout.
    pub fn checkout(&self) -> Option<PathBuf> {
        let project = self.selected()?;
        project.checkouts.iter().find(|checkout| checkout.is_dir()).or_else(|| project.checkouts.first()).cloned()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }

    pub fn clear_error(&mut self) {
        self.error = None;
    }

    /// The project a task belongs to: its record, or the checkout resolved
    /// once for a record written before projects were recorded.
    pub fn task_project(&mut self, task: &Task) -> Option<String> {
        if let Some(id) = &task.spec.project {
            return Some(id.clone());
        }
        if let Some(resolved) = self.resolved.get(&task.spec.cwd) {
            return resolved.clone();
        }
        let resolved = project::identify(&task.spec.cwd).ok().map(|identity| identity.id);
        self.resolved.insert(task.spec.cwd.clone(), resolved.clone());
        resolved
    }

    /// Whether `task` is in the selected project.
    pub fn holds(&mut self, task: &Task) -> bool {
        let Some(selected) = self.selected.clone() else { return false };
        self.task_project(task).as_deref() == Some(selected.as_str())
    }

    /// The selected project's most recent unsettled task: newest by record.
    /// A task for `same` command and directory wins, so an explicit launch
    /// finds the session it started there (issue #141).
    pub fn newest_task(&mut self, tasks: &[Task], same: Option<(&[String], &Path)>) -> Option<String> {
        let selected = self.selected.clone()?;
        let unsettled: Vec<&Task> = tasks
            .iter()
            .filter(|task| {
                task.state != State::Destroyed
                    && task.done_ms.is_none()
                    && self.task_project(task).as_deref() == Some(selected.as_str())
            })
            .collect();
        let matching = unsettled
            .iter()
            .copied()
            .filter(|task| same.is_some_and(|(argv, cwd)| task.spec.argv == argv && task.spec.cwd == cwd));
        let pool: Vec<&Task> = if matching.clone().next().is_some() { matching.collect() } else { unsettled };
        // Task ids begin with the creation time in milliseconds, hex-encoded.
        pool.into_iter().max_by(|a, b| a.id.cmp(&b.id)).map(|task| task.id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn project(id: &str, name: &str, opened_ms: u64) -> Project {
        Project {
            id: id.into(),
            name: name.into(),
            checkouts: vec![PathBuf::from(format!("/src/{name}"))],
            argv: vec!["construct".into(), "acp".into()],
            opened_ms,
            hidden: false,
        }
    }

    fn task(id: &str, cwd: &str, project: Option<&str>, state: Value) -> Task {
        let project = project.map_or(Value::Null, |p| json!(p));
        serde_json::from_value(json!({
            "id": id, "backend": "local", "spec": { "argv": ["construct", "acp"], "cwd": cwd, "project": project },
            "state": state, "pid": null,
        }))
        .unwrap()
    }

    #[test]
    fn visible_projects_sort_by_open_time_and_selection_ignores_hidden_or_unknown() {
        let hidden = Project { hidden: true, ..project("path:/b", "b", 30) };
        let mut p = Projects::default();
        p.set(vec![project("path:/a", "a", 10), hidden, project("path:/c", "c", 20)], Some("path:/c".into()));
        let shown: Vec<(&str, u64)> = p.list().iter().map(|x| (x.name.as_str(), x.opened_ms)).collect();
        assert_eq!(shown, vec![("c", 20), ("a", 10)]);
        assert_eq!(p.selected_id(), Some("path:/c"));
        assert_eq!(p.index(), Some(0));
        assert!(p.select("path:/a"));
        assert_eq!((p.selected_id(), p.index()), (Some("path:/a"), Some(1)));
        assert!(!p.select("path:/b"), "a hidden project is not selectable");
        assert!(!p.select("path:/nope"));
    }

    #[test]
    fn adding_a_project_selects_it_and_sorts_it_first() {
        let mut p = Projects::default();
        p.set(vec![project("path:/a", "a", 10)], None);
        p.add(project("path:/b", "b", 20));
        assert_eq!(p.selected_id(), Some("path:/b"));
        assert_eq!(p.list()[0].name, "b");
    }

    #[test]
    fn a_task_without_a_project_is_placed_by_its_checkout_once() {
        let dir = std::env::temp_dir().join(format!("deck-projects-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = std::fs::canonicalize(&dir).unwrap();
        let id = format!("path:{}", path.display());
        let mut p = Projects::default();
        p.set(vec![Project { id: id.clone(), checkouts: vec![path.clone()], ..project("unused", "b", 1) }], Some(id));
        let cwd = path.to_str().unwrap();
        let older = task("0001-a", cwd, None, json!({ "state": "stopped", "clean": true, "reason": "" }));
        assert!(p.holds(&older));
        let cached = p.resolved.len();
        p.holds(&older);
        assert_eq!(p.resolved.len(), cached, "the checkout is resolved once");
        let elsewhere = task("0002-b", "/src/other", None, json!({ "state": "running" }));
        assert!(!p.holds(&elsewhere));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_newest_unsettled_task_of_the_project_wins_and_a_matching_launch_prefers_its_own() {
        let mut p = Projects::default();
        p.set(vec![project("path:/a", "a", 1)], Some("path:/a".into()));
        let tasks = vec![
            task("0001-old", "/src/a", Some("path:/a"), json!({ "state": "running" })),
            task("0002-new", "/src/a", Some("path:/a"), json!({ "state": "running" })),
            task("0003-done", "/src/a", Some("path:/a"), json!({ "state": "running" })),
            task("0004-destroyed", "/src/a", Some("path:/a"), json!({ "state": "destroyed" })),
            task("0005-elsewhere", "/src/b", Some("path:/b"), json!({ "state": "running" })),
        ];
        let mut tasks = tasks;
        tasks[2].done_ms = Some(9);
        assert_eq!(p.newest_task(&tasks, None).as_deref(), Some("0002-new"));
        let argv = ["construct".to_string(), "acp".to_string()];
        assert_eq!(p.newest_task(&tasks, Some((&argv, Path::new("/src/a")))).as_deref(), Some("0002-new"));
        assert_eq!(p.newest_task(&tasks, Some((&argv, Path::new("/nowhere")))).as_deref(), Some("0002-new"));
    }

    #[test]
    fn only_an_existing_checkout_is_used_and_errors_clear_on_a_new_project() {
        let mut p = Projects::default();
        p.set(vec![project("path:/a", "missing-checkout", 1)], Some("path:/a".into()));
        assert_eq!(p.checkout(), Some(PathBuf::from("/src/missing-checkout")), "the only checkout, even when absent");
        p.set_error("shallow".into());
        assert_eq!(p.error(), Some("shallow"));
        p.add(project("path:/b", "b", 2));
        assert_eq!(p.error(), None);
    }
}
