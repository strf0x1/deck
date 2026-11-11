//! A tool card's body: the diff of an edit, or the tail of a command's
//! output, each cut to a preview until the card is opened.
//!
//! The design choice: the sign column and a faint wash say what changed;
//! the code itself keeps its syntax colours (each side highlighted whole,
//! so multi-line strings and comments stay right), dimmed where unchanged.

use super::cards::{CAP, PREVIEW, TAIL, mono, more};
use super::diff::{self, Row as DiffRow};
use super::highlight;
use super::prose;
use super::theme::{Tokens, size, space, tint};
use super::transcript;
use super::typography::Typography;
use gpui::{
    AnyElement, Div, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _, div,
    prelude::FluentBuilder as _, px,
};

pub fn diff(changes: &[transcript::Change], open: bool, typography: &Typography) -> AnyElement {
    let mut cut_total = 0;
    let mut out =
        div().flex().flex_col().border_t_1().border_color(Tokens::edge()).bg(Tokens::sunken()).py(px(space::DETAIL));
    for (n, change) in changes.iter().enumerate() {
        let (rows, cut) = change.diff.preview(2, if open { CAP } else { PREVIEW });
        cut_total += cut;
        if changes.len() > 1 {
            out = out.child(
                mono()
                    .px(px(space::INSET))
                    .pt(px(if n == 0 { 2. } else { 8. }))
                    .pb(px(space::HAIR))
                    .text_color(Tokens::text_dim())
                    .child(change.path.clone()),
            );
        }
        // Each side highlighted whole, so a line inside a multi-line string
        // or comment keeps its context; diff line numbers index these.
        let lang = highlight::language_of(&change.path);
        let (old, new) = (highlight::lines(lang, &change.old), highlight::lines(lang, &change.new));
        out = out.children(rows.into_iter().map(|r| diff_line(r, change.numbered, &old, &new, typography)));
    }
    let toggle = (cut_total > 0 || open).then(|| more(cut_total, "lines", open));
    div().flex().flex_col().child(out).children(toggle).into_any_element()
}

fn diff_line(
    row: DiffRow<'_>,
    numbered: bool,
    old: &[highlight::Line],
    new: &[highlight::Line],
    typography: &Typography,
) -> Div {
    let line = match row {
        DiffRow::Gap(n) => {
            return mono()
                .px(px(space::INSET))
                .py(px(1.))
                .text_color(Tokens::text_faint())
                .child(format!("⋯ {n} unchanged line{}", if n == 1 { "" } else { "s" }));
        }
        DiffRow::Line(l) => l,
    };
    let (sign, fg, bg): (&str, Hsla, Option<Hsla>) = match line.kind {
        diff::Kind::Same => (" ", Tokens::text_dim().into(), None),
        diff::Kind::Added => ("+", Tokens::lime(), Some(tint(Tokens::lime(), 0.1))),
        diff::Kind::Removed => ("−", Tokens::red(), Some(tint(Tokens::red(), 0.1))),
    };
    let number =
        if numbered { line.new.or(line.old).map(|n| n.to_string()).unwrap_or_default() } else { String::new() };
    // The sign carries add/remove; the text keeps its syntax colours, a
    // little dimmer on unchanged lines.
    let spans = match line.kind {
        diff::Kind::Removed => line.old.and_then(|n| old.get(n - 1)),
        _ => line.new.and_then(|n| new.get(n - 1)),
    };
    let text = prose::code_text(
        &line.text,
        spans.map(std::slice::from_ref).unwrap_or_default(),
        Tokens::text().into(),
        typography,
    );
    mono()
        .flex()
        .px(px(space::INSET))
        .line_height(size::scaled(18.))
        .when_some(bg, |d, bg| d.bg(bg))
        // A diff built from an edit's arguments has no file line numbers to show.
        .when(numbered, |d| d.child(div().flex_none().w(px(34.)).text_color(Tokens::text_faint()).child(number)))
        .child(div().flex_none().w(px(14.)).text_color(fg).child(sign))
        .child(div().flex_1().min_w_0().when(line.kind == diff::Kind::Same, |d| d.opacity(0.7)).child(text))
}

/// An execute card's full command, above its output: the header shows a
/// multiline command as one line, so an opened card is where it is read.
pub fn command(text: &str, selector: String, typography: &Typography) -> AnyElement {
    mono()
        .font_family(typography.code.family())
        .flex()
        .flex_col()
        .px(px(space::INSET))
        .py(px(7.))
        .border_t_1()
        .border_color(Tokens::edge())
        .bg(Tokens::sunken())
        .line_height(size::scaled(17.))
        .text_color(Tokens::text())
        .debug_selector(move || selector)
        .children(text.trim_end_matches('\n').lines().map(|l| div().child(l.to_string())))
        .into_any_element()
}

pub fn output(text: &str, open: bool, typography: &Typography) -> Option<AnyElement> {
    if text.trim().is_empty() {
        return None;
    }
    let (lines, hidden) = transcript::tail(text, if open { CAP } else { TAIL });
    let body = mono()
        .debug_selector(|| "tool-output".into())
        .font_family(typography.code.family())
        .flex()
        .flex_col()
        .px(px(space::INSET))
        .py(px(7.))
        .border_t_1()
        .border_color(Tokens::edge())
        .bg(Tokens::sunken())
        .line_height(size::scaled(17.))
        .text_color(Tokens::text_dim())
        .children(lines.into_iter().map(|l| div().child(l.to_string())));
    let toggle = (hidden > 0 || open).then(|| more(hidden, "earlier lines", open));
    Some(div().flex().flex_col().child(body).children(toggle).into_any_element())
}
