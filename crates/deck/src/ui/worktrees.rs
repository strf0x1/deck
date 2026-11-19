//! The window's worktree facts and one review pane. Git runs in deckd,
//! and pane requests carry a task ID so navigation cannot retarget them.

use super::root::Deck;
use deck_runtime::worktree::{SourcePreview, WorktreeDiff, WorktreeStatus};
use deck_runtime::{State, Turn};
use gpui::Context;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const REFRESH: Duration = Duration::from_millis(750);

type DisplayResult<T> = Result<T, String>;

#[derive(Default)]
pub(super) struct Worktrees {
    source: Option<(String, DisplayResult<Option<SourcePreview>>)>,
    facts: HashMap<String, (u64, WorktreeStatus)>,
    request: Option<(u64, Query)>,
    last: Option<(Query, Instant)>,
    next: u64,
    pub panel: Option<Pane>,
}

#[derive(Clone, PartialEq, Eq)]
struct Query {
    source: Option<String>,
    workers: Vec<Worker>,
}

#[derive(Clone, PartialEq, Eq)]
struct Worker {
    id: String,
    ready: bool,
    state: State,
    turn: Option<Turn>,
}

pub struct Pane {
    stamp: u64,
    pub id: String,
    pub diff: Option<DisplayResult<WorktreeDiff>>,
    pub reading: bool,
    pub confirm: bool,
    pub removing: bool,
    pub error: Option<String>,
}

impl Worktrees {
    fn stamp(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn put(&mut self, id: String, status: WorktreeStatus) {
        let seen = self
            .facts
            .get(&id)
            .filter(|(_, prior)| prior.merged && status.merged)
            .map_or_else(super::publishing::now_ms, |(seen, _)| *seen);
        self.facts.insert(id, (seen, status));
    }
}

impl Deck {
    pub fn worktree_source(&self) -> Option<&DisplayResult<Option<SourcePreview>>> {
        let (project, source) = self.worktrees.source.as_ref()?;
        (self.projects.selected_id() == Some(project)).then_some(source)
    }

    pub(super) fn local_worker_source(&self) -> bool {
        self.coordinator().map_or_else(|| self.next_backend() == "local", |task| task.backend == "local")
    }

    pub fn worktree_status(&self, id: &str) -> Option<&WorktreeStatus> {
        self.worktrees.facts.get(id).map(|(_, status)| status)
    }

    pub fn worktree_pane(&self) -> Option<&Pane> {
        self.worktrees.panel.as_ref()
    }

    pub(super) fn local_merged_at(&self, id: &str) -> Option<u64> {
        self.worktrees.facts.get(id).filter(|(_, status)| status.merged).map(|(at, _)| *at)
    }

    pub(super) fn refresh_worktrees(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(data) = self.data.clone() else { return };
        let project = self.projects.selected_id();
        let panel = self.worktrees.panel.as_ref().map(|p| p.id.as_str());
        let mut workers: Vec<_> = self
            .tasks
            .iter()
            .filter_map(|task| {
                let workspace = task.workspace.as_ref()?;
                (task.spec.project.as_deref() == project || panel == Some(task.id.as_str())).then(|| Worker {
                    id: task.id.clone(),
                    ready: workspace.ready,
                    state: task.state.clone(),
                    turn: task.turn.clone(),
                })
            })
            .collect();
        workers.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        let query = Query { source: project.filter(|_| self.local_worker_source()).map(str::to_owned), workers };
        if !force
            && (self.worktrees.request.as_ref().is_some_and(|(_, pending)| pending == &query)
                || self.worktrees.last.as_ref().is_some_and(|(last, at)| last == &query && at.elapsed() < REFRESH))
        {
            return;
        }
        if query.source.is_none() && query.workers.is_empty() {
            self.worktrees.request = None;
            return;
        }
        let stamp = self.worktrees.stamp();
        self.worktrees.request = Some((stamp, query.clone()));
        let read = query.clone();
        let task = cx.background_executor().spawn(async move {
            let daemon = deck_runtime::DaemonClient::connect(&data).map_err(|error| format!("{error:#}"))?;
            let source = read.source.map(|project| {
                let result = daemon.worktree_source(&project).map_err(|error| format!("{error:#}"));
                (project, result)
            });
            let statuses = read
                .workers
                .into_iter()
                .map(|Worker { id, .. }| {
                    let status = daemon.worktree_status(&id).map_err(|error| format!("{error:#}"));
                    (id, status)
                })
                .collect::<Vec<_>>();
            Ok::<_, String>((source, statuses))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |deck, cx| {
                if !deck.worktrees.request.as_ref().is_some_and(|(current, _)| *current == stamp) {
                    return;
                }
                deck.worktrees.request = None;
                deck.worktrees.last = Some((query.clone(), Instant::now()));
                match result {
                    Ok((source, statuses)) => {
                        if source.is_some() {
                            deck.worktrees.source = source;
                        }
                        for (id, result) in statuses {
                            match result {
                                Ok(status) => deck.worktrees.put(id, status),
                                Err(error) => {
                                    deck.worktrees.facts.remove(&id);
                                    if let Some(pane) = deck.worktrees.panel.as_mut().filter(|pane| pane.id == id) {
                                        pane.error.get_or_insert(error);
                                    }
                                }
                            }
                        }
                    }
                    Err(error) => {
                        if let Some(project) = query.source {
                            deck.worktrees.source = Some((project, Err(error.clone())));
                        }
                        if let Some(pane) = &mut deck.worktrees.panel {
                            pane.error.get_or_insert(error);
                        }
                    }
                }
                deck.rebuild_worktree_rows(cx);
            });
        })
        .detach();
    }

    fn rebuild_worktree_rows(&mut self, cx: &mut Context<Self>) {
        let (tasks, published) = (self.tasks.clone(), std::mem::take(&mut self.published));
        self.on_tasks(tasks, published, cx);
    }

    pub fn open_worktree(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.tasks.iter().any(|task| task.id == id && task.workspace.is_some()) {
            return;
        }
        let stamp = self.worktrees.stamp();
        self.worktrees.panel = Some(Pane {
            stamp,
            id: id.into(),
            diff: None,
            reading: false,
            confirm: false,
            removing: false,
            error: None,
        });
        self.close_publication(cx);
        if self.settings.is_open() {
            self.settings.toggle();
        }
        if self.access.is_open() {
            self.toggle_access(cx);
        }
        if self.limits_open() {
            self.toggle_limits(None, cx);
        }
        self.refresh_worktrees(true, cx);
        cx.notify();
    }

    pub fn close_worktree(&mut self, cx: &mut Context<Self>) {
        self.worktrees.panel = None;
        cx.notify();
    }

    pub fn review_worktree(&mut self, cx: &mut Context<Self>) {
        let (Some(daemon), Some(pane)) = (self.daemon.clone(), &mut self.worktrees.panel) else { return };
        if pane.reading || pane.removing {
            return;
        }
        pane.reading = true;
        let (id, stamp) = (pane.id.clone(), pane.stamp);
        let read = cx
            .background_executor()
            .spawn(async move { daemon.worktree_diff(&id).map_err(|error| format!("{error:#}")) });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |deck, cx| {
                if let Some(pane) = deck.worktrees.panel.as_mut().filter(|pane| pane.stamp == stamp) {
                    pane.reading = false;
                    pane.diff = Some(result);
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub fn confirm_worktree_removal(&mut self, confirm: bool, cx: &mut Context<Self>) {
        if let Some(pane) = &mut self.worktrees.panel {
            pane.confirm = confirm;
            pane.error = None;
            cx.notify();
        }
    }

    pub fn remove_worktree(&mut self, cx: &mut Context<Self>) {
        let (Some(daemon), Some(pane)) = (self.daemon.clone(), &mut self.worktrees.panel) else { return };
        if !pane.confirm || pane.removing {
            return;
        }
        pane.removing = true;
        pane.error = None;
        let (id, stamp) = (pane.id.clone(), pane.stamp);
        let request_id = id.clone();
        let remove = cx
            .background_executor()
            .spawn(async move { daemon.remove_worktree(&request_id).map_err(|error| format!("{error:#}")) });
        cx.spawn(async move |this, cx| {
            let result = remove.await;
            let _ = this.update(cx, |deck, cx| {
                deck.worktrees.request = None;
                let error = match result {
                    Ok(status) => {
                        deck.worktrees.put(id, status);
                        None
                    }
                    Err(error) => Some(error),
                };
                if let Some(pane) = deck.worktrees.panel.as_mut().filter(|pane| pane.stamp == stamp) {
                    pane.removing = false;
                    pane.confirm = false;
                    if error.is_none() {
                        pane.diff = None;
                    }
                    pane.error = error;
                }
                deck.refresh_worktrees(true, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub fn pick_worktree_source(&mut self, cx: &mut Context<Self>) {
        let (Some(daemon), Some(project)) = (self.daemon.clone(), self.projects.selected_id().map(str::to_owned))
        else {
            return;
        };
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose worker source checkout".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = receiver.await else { return };
            let Some(path) = paths.pop() else { return };
            let id = project.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let source = daemon.choose_worktree_source(&id, &path)?;
                    Ok::<_, anyhow::Error>((source, daemon.projects()?))
                })
                .await;
            let _ = this.update(cx, |deck, cx| {
                deck.worktrees.request = None;
                match result {
                    Ok((source, projects)) => {
                        deck.worktrees.source = Some((project, Ok(Some(source))));
                        deck.on_projects(projects, deck.projects.selected_id().map(str::to_owned), cx);
                    }
                    Err(error) if deck.projects.selected_id() == Some(&project) => {
                        deck.worktrees.source = Some((project, Err(format!("{error:#}"))));
                    }
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }
}
