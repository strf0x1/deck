//! The sidebar's rows and its narrow glyphs as plain data: what it lists open,
//! and what it shows closed, for every project and the selected one's tasks
//! (ADR 0010 §1, §7, §9 as amended 2026-11-09).
//!
//! The design choice: the view gets no state from here, only the rows to draw
//! and where each lives in its input, so what the sidebar shows is a pure
//! function of the window's needs, projects and selection that a unit test can
//! pin down without GPUI.

/// One project as the sidebar shows it: its group header and its task count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIn {
    pub id: String,
    pub name: String,
    pub tasks: usize,
}

/// One attention item as the sidebar shows it: its task's title, the reason in
/// a word, and the full reason for hover and the item it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeedIn {
    pub task: String,
    pub project: String,
    pub title: String,
    pub word: &'static str,
    pub text: String,
}

/// One row of the open sidebar, top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// "Needs you", with the number of items below it.
    NeedsHeader {
        count: usize,
    },
    Need {
        index: usize,
        title: String,
        word: &'static str,
        text: String,
    },
    /// "Nothing needs you": the section never disappears (§7).
    Nothing,
    Project {
        index: usize,
        id: String,
        name: String,
        tasks: usize,
        attention: bool,
        selected: bool,
    },
    /// Where the selected project's task list goes.
    Tasks,
    Settled {
        count: usize,
    },
}

/// The open sidebar: "Needs you" first, then each project, the selected one's
/// tasks under its header, then the settled fold.
pub fn rows(needs: &[NeedIn], projects: &[ProjectIn], selected: Option<&str>, settled: usize) -> Vec<Row> {
    let mut rows = Vec::with_capacity(needs.len() + projects.len() * 2 + 3);
    rows.push(Row::NeedsHeader { count: needs.len() });
    if needs.is_empty() {
        rows.push(Row::Nothing);
    } else {
        rows.extend(needs.iter().enumerate().map(|(index, need)| Row::Need {
            index,
            title: need.title.clone(),
            word: need.word,
            text: need.text.clone(),
        }));
    }
    for (index, project) in projects.iter().enumerate() {
        let selected = selected == Some(project.id.as_str());
        rows.push(Row::Project {
            index,
            id: project.id.clone(),
            name: project.name.clone(),
            tasks: project.tasks,
            attention: needs.iter().any(|need| need.project == project.id),
            selected,
        });
        if selected {
            rows.push(Row::Tasks);
        }
    }
    if settled > 0 {
        rows.push(Row::Settled { count: settled });
    }
    rows
}

/// One glyph of the narrow sidebar: a project's initial letter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glyph {
    pub index: usize,
    pub letter: char,
    pub attention: bool,
    pub selected: bool,
}

/// The narrow sidebar: the Needs-you count, shown only above zero, and one
/// glyph per project in the given order.
pub fn narrow(needs: &[NeedIn], projects: &[ProjectIn], selected: Option<&str>) -> (usize, Vec<Glyph>) {
    let glyphs = projects
        .iter()
        .enumerate()
        .map(|(index, project)| Glyph {
            index,
            letter: initial(&project.name),
            attention: needs.iter().any(|need| need.project == project.id),
            selected: selected == Some(project.id.as_str()),
        })
        .collect();
    (needs.len(), glyphs)
}

/// The name's first alphanumeric character, uppercased, or `?`.
fn initial(name: &str) -> char {
    name.chars().find(|c| c.is_alphanumeric()).and_then(|c| c.to_uppercase().next()).unwrap_or('?')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str, name: &str, tasks: usize) -> ProjectIn {
        ProjectIn { id: id.into(), name: name.into(), tasks }
    }

    fn need(task: &str, project: &str, title: &str) -> NeedIn {
        NeedIn {
            task: task.into(),
            project: project.into(),
            title: title.into(),
            word: "asks",
            text: "asks to run make".into(),
        }
    }

    fn project_row(row: &Row) -> (usize, &str, bool, bool) {
        match row {
            Row::Project { index, id, attention, selected, .. } => (*index, id.as_str(), *attention, *selected),
            _ => panic!("not a project row: {row:?}"),
        }
    }

    #[test]
    fn the_open_sidebar_lists_the_needs_section_then_projects_with_the_selected_ones_tasks() {
        let projects = [project("git:a", "alpha", 2), project("path:/b", "beta", 0)];
        let rows = rows(&[], &projects, Some("path:/b"), 0);
        assert_eq!(
            rows,
            vec![
                Row::NeedsHeader { count: 0 },
                Row::Nothing,
                Row::Project {
                    index: 0,
                    id: "git:a".into(),
                    name: "alpha".into(),
                    tasks: 2,
                    attention: false,
                    selected: false,
                },
                Row::Project {
                    index: 1,
                    id: "path:/b".into(),
                    name: "beta".into(),
                    tasks: 0,
                    attention: false,
                    selected: true,
                },
                Row::Tasks,
            ]
        );
    }

    #[test]
    fn needs_keep_their_order_and_light_the_project_they_belong_to() {
        let needs = [need("t2", "git:a", "second"), need("t1", "path:/b", "first")];
        let projects = [project("git:a", "alpha", 1), project("path:/b", "beta", 1)];
        let rows = rows(&needs, &projects, None, 0);
        let titles: Vec<&str> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Need { title, .. } => Some(title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(titles, ["second", "first"], "oldest first, as given");
        assert!(matches!(rows.first(), Some(Row::NeedsHeader { count: 2 })));
        let projects: Vec<(usize, &str, bool, bool)> =
            rows.iter().filter(|r| matches!(r, Row::Project { .. })).map(project_row).collect();
        assert_eq!(projects, [(0, "git:a", true, false), (1, "path:/b", true, false)]);
    }

    #[test]
    fn an_unselected_sidebar_has_no_tasks_row() {
        let projects = [project("git:a", "alpha", 1)];
        let rows = rows(&[], &projects, None, 0);
        assert!(!rows.contains(&Row::Tasks), "no project is open to list tasks under");
        assert_eq!(project_row(&rows[2]), (0, "git:a", false, false));
    }

    #[test]
    fn the_settled_fold_closes_the_list_only_when_it_has_something() {
        let projects = [project("git:a", "alpha", 1)];
        assert_eq!(rows(&[], &projects, Some("git:a"), 3).last(), Some(&Row::Settled { count: 3 }));
        let none = rows(&[], &projects, Some("git:a"), 0);
        assert!(!none.iter().any(|row| matches!(row, Row::Settled { .. })));
    }

    #[test]
    fn the_narrow_sidebar_shows_the_count_and_one_letter_per_project() {
        let needs = [need("t1", "git:a", "first")];
        let projects = [
            project("git:a", "alpha", 1),
            project("git:b", "9 lives", 0),
            project("git:c", "·dot", 0),
            project("path:/d", "", 0),
        ];
        let (count, glyphs) = narrow(&needs, &projects, Some("git:b"));
        assert_eq!(count, 1);
        let seen: Vec<(usize, char, bool, bool)> =
            glyphs.iter().map(|g| (g.index, g.letter, g.attention, g.selected)).collect();
        assert_eq!(
            seen,
            [(0, 'A', true, false), (1, '9', false, true), (2, 'D', false, false), (3, '?', false, false)]
        );
    }
}
