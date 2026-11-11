//! The prompt at the foot of the conversation: the composer field, the send
//! button, and a status row saying where the agent works (agent, backend,
//! directory, branch, tokens used).
//!
//! The design choice: the status row reads only what `deckd` already
//! reported for this window's task, plus the branch from the project's own
//! `HEAD` file, read when the task list refreshes rather than every frame.
//! It shows what is known and leaves the rest out; nothing here asks the
//! agent for anything.

use super::chrome::agent_label;
use super::composer::Field;
use super::panels::glyph;
use super::root::Deck;
use super::theme::{MONO, Tokens, glow, icon, radius, size, space, tint};
use gpui::{
    Context, Hsla, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement as _, Styled as _, div, prelude::FluentBuilder as _, px,
};
use std::path::{Path, PathBuf};

/// Where the window's task runs, from its record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub cwd: PathBuf,
    /// `local` or `vm`.
    pub backend: String,
    pub branch: Option<String>,
    pub tokens: u64,
    /// The task's pinned `provider/model` (ADR 0012 §2); `None` for a local
    /// task, whose provider is Construct's own config.
    pub model: Option<String>,
}

impl Workspace {
    pub fn of(task: &deck_runtime::Task) -> Self {
        Workspace {
            cwd: task.spec.cwd.clone(),
            backend: task.backend.clone(),
            branch: branch(&task.spec.cwd),
            tokens: task.usage.tokens,
            model: match &task.spec.model {
                Some(deck_runtime::settings::Pick::Choice(choice)) => {
                    Some(format!("{}/{}", choice.provider.name, choice.model))
                }
                Some(deck_runtime::settings::Pick::Selector(selector)) => Some(selector.clone()),
                None => None,
            },
        }
    }
}

/// The checked-out branch of the repository at `dir`, or a short commit id
/// when `HEAD` is detached. A linked worktree's `.git` is a file naming its
/// git directory.
pub fn branch(dir: &Path) -> Option<String> {
    let dot = dir.join(".git");
    let git = if dot.is_file() {
        let link = std::fs::read_to_string(&dot).ok()?;
        dir.join(link.strip_prefix("gitdir:")?.trim())
    } else {
        dot
    };
    let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(r) => Some(r.strip_prefix("refs/heads/").unwrap_or(r).to_string()),
        None => head.get(..7).map(str::to_string),
    }
}

/// `dir` with the home directory shown as `~`.
pub(super) fn tilde(dir: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if dir.starts_with(&home) && home.as_os_str().len() > 1 => {
            format!("~/{}", dir.strip_prefix(&home).map(|p| p.display().to_string()).unwrap_or_default())
        }
        _ => dir.display().to_string(),
    }
}

/// `1234567` as `1.2M`, the way a meter reads.
fn compact(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

pub fn prompt(deck: &Deck, cx: &mut Context<Deck>) -> impl IntoElement {
    // A project with no task takes a prompt: it creates the task (ADR 0010 §5).
    let starting = deck.awaiting_task();
    let ready = deck.session().can_prompt() || starting;
    let has_draft = !deck.editor.is_empty();
    // Keys go to an open card first, so the hint says where they go.
    let hint = if deck.session().ask.is_some() || deck.boundary().pending().is_some() {
        "Answer the question above first".to_string()
    } else if deck.new_session_state().is_creating() {
        "Starting the task…".to_string()
    } else if starting {
        format!("Start a session in {}…", deck.project_name().unwrap_or("this project"))
    } else if ready {
        "Tell the agent what to do…".to_string()
    } else if deck.project().is_none() {
        "Open a project to start".to_string()
    } else if deck.session().status == super::session::Status::Working {
        "Working… you can type ahead".to_string()
    } else {
        "Waiting for the agent…".to_string()
    };
    let caret: Hsla = tint(Tokens::accent(), if ready { 0.9 } else { 0.35 });
    let armed = ready && has_draft;
    let field = div()
        .id("composer")
        .debug_selector(|| "composer".into())
        .flex_1()
        .min_w_0()
        .text_size(size::scaled(size::BODY))
        .py(px(15.))
        .line_height(size::scaled(21.))
        .cursor_text()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                window.focus(&this.focus, cx);
                let at = this.composer.index_at(ev.position);
                if ev.click_count >= 2 {
                    this.editor.select_word(at);
                } else {
                    this.editor.move_to(at, ev.modifiers.shift);
                    this.selecting = true;
                }
                cx.stop_propagation();
                cx.notify();
            }),
        )
        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
            if this.selecting && ev.dragging() {
                this.editor.move_to(this.composer.index_at(ev.position), true);
                cx.notify();
            }
        }))
        .child(Field::new(cx.entity(), caret, hint));
    let send = div()
        .flex_none()
        .mb(px(8.))
        .size(px(36.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::CONTROL))
        .when(armed, |d| d.bg(Tokens::sweep(1.)).shadow(glow(Tokens::accent(), 0.8)).text_color(Tokens::ink()))
        .when(!armed, |d| d.bg(Tokens::raised()).text_color(Tokens::text_faint()))
        .child(glyph(icon::SEND, 13.));
    div()
        .flex()
        .flex_col()
        .rounded(px(radius::FIELD))
        .border_1()
        .border_color(if ready { tint(Tokens::accent(), 0.45) } else { Tokens::edge().into() })
        .bg(Tokens::sunken())
        .when(ready, |d| d.shadow(glow(Tokens::accent(), 0.22)))
        .overflow_hidden()
        .child(div().pl(px(16.)).pr(px(8.)).flex().items_end().gap(px(space::STACK)).child(field).child(send))
        .child(status_row(deck))
}

/// Agent · backend · directory · branch, with tokens and the key hint at the right.
fn status_row(deck: &Deck) -> impl IntoElement {
    // Short items keep their width; the path and branch give way first.
    // Each icon keeps its meaning's colour; the words stay quiet.
    let item = |g: &'static str, color: Hsla, text: String, shrinks: bool| {
        div()
            .flex()
            .items_center()
            .gap(px(space::TIGHT))
            .when(shrinks, |d| d.min_w_0().flex_shrink())
            .when(!shrinks, |d| d.flex_none())
            .child(glyph(g, 10.).text_color(tint(color, 0.85)))
            // Clipped, not truncated: a truncating text measures as its ellipsis
            // in a flex row and would collapse to "…" with room to spare.
            .child(div().overflow_hidden().whitespace_nowrap().child(text))
    };
    // The sealed VM is green; a local process has no such promise.
    let backend = |w: &Workspace| {
        if w.backend == "vm" {
            item(icon::LOCK, Tokens::lime(), w.backend.clone(), false)
        } else {
            item(icon::TERMINAL, Tokens::text_faint().into(), w.backend.clone(), false)
        }
    };
    let w = deck.workspace.as_ref();
    let agent =
        if deck.session().agent.is_empty() { agent_label(&deck.agent_cmd) } else { deck.session().agent.clone() };
    let project_dir = deck.project_checkout();
    let enter = if deck.editor.text().contains('\n') { "enter send · shift-enter newline" } else { "enter send" };
    div()
        .min_h(px(30.))
        .px(px(16.))
        .flex()
        .items_center()
        .gap(px(16.))
        .border_t_1()
        .border_color(Tokens::edge())
        .bg(Tokens::raised())
        .font_family(MONO)
        .text_size(size::scaled(size::SMALL))
        .text_color(Tokens::text_dim())
        .child(item(icon::AGENTS, Tokens::cyan(), agent, false))
        .children(w.map(backend))
        .children(w.map(|w| match &w.model {
            Some(model) => item(icon::BOLT, Tokens::cyan(), model.clone(), true),
            None if w.backend == "local" => {
                item(icon::BOLT, Tokens::text_faint().into(), "construct config".into(), false)
            }
            None => item(icon::BOLT, Tokens::text_faint().into(), "deckd default".into(), false),
        }))
        .children(w.map(|w| item(icon::PROJECT, Tokens::sky(), tilde(&w.cwd), true)))
        .children(
            w.is_none()
                .then_some(project_dir)
                .flatten()
                .map(|dir| item(icon::PROJECT, Tokens::sky(), tilde(&dir), true)),
        )
        .children(w.and_then(|w| w.branch.clone()).map(|b| item(icon::BRANCH, Tokens::violet(), b, true)))
        .child(div().flex_1())
        .children(
            w.filter(|w| w.tokens > 0)
                .map(|w| item(icon::GAUGE, Tokens::amber(), format!("{} tokens", compact(w.tokens)), false)),
        )
        .child(div().text_color(Tokens::text_faint()).child(enter))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_reads_head_in_a_checkout_and_a_linked_worktree() {
        let root = std::env::temp_dir().join(format!("deck-branch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(branch(&repo).as_deref(), Some("main"));

        let wt = root.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../repo/.git/worktrees/wt\n").unwrap();
        std::fs::write(repo.join(".git/worktrees/wt/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(branch(&wt).as_deref(), Some("0123456"), "a detached HEAD shows a short id");
        assert_eq!(branch(&root), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn counts_read_like_a_meter() {
        assert_eq!((compact(999), compact(12_345), compact(3_400_000)), ("999".into(), "12.3k".into(), "3.4M".into()));
    }
}
