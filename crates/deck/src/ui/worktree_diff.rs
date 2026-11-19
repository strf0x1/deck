//! A worktree's review: the Git patch from deckd drawn as one card per
//! file, each with the edit cards' frame, header and diff rows.
//!
//! The design choice: no second diff look. The patch is read back into
//! `diff::Line`s (`patch.rs`) and drawn with `card_body::diff_line`, so the
//! sign column, the wash, syntax colours and gaps match an edit card's; a
//! file's header names its repository path and copies its absolute path in
//! the worktree, the one a developer opens in an editor.

use super::cards::{frame, kind};
use super::copy;
use super::diff;
use super::highlight;
use super::panels::icon_mark;
use super::patch::{File, Row};
use super::root::Deck;
use super::selectable::{Hits, RowRuns};
use super::theme::{Tokens, size, space};
use super::typography::Typography;
use deck_acp::ToolKind;
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _, div, px,
};
use std::path::Path;

/// Rows the review draws before it sends the user to the worktree.
const REVIEW_ROWS: usize = 800;

/// The hover group a file card is, revealing its copy button.
const FILE: &str = "worktree-file";

/// The patch as file cards under a heading. `root` is the worktree while
/// it exists; a removed one's paths copy as repository paths.
pub(super) fn patch(
    deck: &Deck,
    diff: &deck_runtime::worktree::WorktreeDiff,
    root: Option<&Path>,
    cx: &mut Context<Deck>,
) -> AnyElement {
    let files = super::patch::parse(&diff.text);
    let total: usize = files.iter().map(|f| f.rows.len()).sum();
    let mut left = REVIEW_ROWS;
    let mut cards = vec![];
    for (n, file) in files.iter().enumerate() {
        if left == 0 {
            break;
        }
        let shown = file.rows.len().min(left);
        left -= shown;
        cards.push(file_card(deck, n, file, shown, root, cx));
    }
    let heading = match files.len() {
        0 => "No tracked changes from the starting commit".to_string(),
        1 => "1 file changed from the starting commit".to_string(),
        n => format!("{n} files changed from the starting commit"),
    };
    div()
        .id("worktree-diff")
        .debug_selector(|| "worktree-diff".into())
        .flex()
        .flex_col()
        .gap(px(space::LINE))
        .child(div().font_weight(FontWeight::MEDIUM).text_color(Tokens::text_dim()).child(heading))
        .children(cards)
        .children(
            (diff.truncated || total > REVIEW_ROWS)
                .then(|| div().text_color(Tokens::amber()).child("Open the worktree to see the full diff.")),
        )
        .into_any_element()
}

/// One file as an edit card: header (mark, path, counts, notes, copy) over
/// its first `shown` rows.
fn file_card(
    deck: &Deck,
    n: usize,
    file: &File,
    shown: usize,
    root: Option<&Path>,
    cx: &mut Context<Deck>,
) -> AnyElement {
    let typography = &deck.typography;
    let deleted = file.notes.iter().any(|note| note == "Deleted");
    let (mark, _, hue) = kind(if deleted { ToolKind::Delete } else { ToolKind::Edit });
    let copied = root.map_or_else(|| file.path.clone(), |root| root.join(&file.path).display().to_string());
    let code =
        |text: String| div().font_family(typography.code.family()).text_size(size::scaled(size::SMALL)).child(text);
    let header = div()
        .debug_selector(move || format!("worktree-diff-file-{n}"))
        .h(px(space::HEADER))
        .overflow_hidden()
        .pl(px(space::TIGHT))
        .pr(px(space::INSET))
        .flex()
        .items_center()
        .gap(px(9.))
        .child(icon_mark(mark, hue, 22.))
        .child(code(file.path.clone()).flex_1().min_w_0().truncate().text_color(Tokens::text()))
        .children((!file.notes.is_empty()).then(|| {
            div()
                .flex_none()
                .text_size(size::scaled(size::SMALL))
                .text_color(Tokens::text_faint())
                .child(file.notes.join(" · "))
        }))
        .children((file.added > 0).then(|| code(format!("+{}", file.added)).flex_none().text_color(Tokens::lime())))
        .children((file.removed > 0).then(|| code(format!("−{}", file.removed)).flex_none().text_color(Tokens::red())))
        .child(copy::button_in(FILE, deck, format!("worktree-file-{n}"), copied, cx));
    let body = (shown > 0).then(|| rows(file, shown, typography));
    frame(false)
        .id(gpui::SharedString::from(format!("worktree-file-{n}")))
        .group(FILE)
        .child(header)
        .children(body)
        .into_any_element()
}

/// The file's rows as an edit card's body draws them.
fn rows(file: &File, shown: usize, typography: &Typography) -> gpui::Div {
    // The pane is not the transcript, so its text joins no selection.
    let runs = RowRuns::new("worktree-diff".into(), 0, None, Hits::default());
    let lang = highlight::language_of(&file.path);
    let (old, new) = (highlight::lines(lang, &file.old), highlight::lines(lang, &file.new));
    div()
        .flex()
        .flex_col()
        .border_t_1()
        .border_color(Tokens::edge())
        .bg(Tokens::sunken())
        .py(px(space::DETAIL))
        .children(file.rows[..shown].iter().map(|row| {
            let spans = match row {
                Row::Line(line, ix) if line.kind == diff::Kind::Removed => old.get(*ix),
                Row::Line(_, ix) => new.get(*ix),
                Row::Gap(_) => None,
            };
            super::card_body::diff_line(row.as_diff(), true, spans, typography, &runs)
        }))
}
