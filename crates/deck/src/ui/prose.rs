//! Agent prose drawn from [`markdown`](super::markdown) blocks: paragraphs
//! and headings as styled text, code in rounded mono boxes, quotes behind a
//! rule, lists with hanging markers.
//!
//! The design choice: prose stays unboxed, so the eye tells the agent's
//! words from its work (the boxed tool cards) without reading either; only
//! code, which is machine text, gets a box, and is highlighted
//! (`highlight.rs`). Inline styles become per-run fonts, so inline code is
//! set in the mono face, not just tinted.

use super::highlight;
use super::markdown::{Block, Inline, Mark, parse};
use super::selectable::{RowRuns, Text};
use super::theme::{Tokens, radius, size, space, tint};
use super::typography::{CodeFont, Typography};
use gpui::{
    AnyElement, FontStyle, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, StrikethroughStyle, Styled as _, TextRun, UnderlineStyle, div, font, px, relative,
};

/// An agent message; each paragraph, heading and code block is the next of
/// `runs`, in the order `runs::prose` lists them.
pub fn prose(text: &str, typography: &Typography, runs: &RowRuns) -> AnyElement {
    div().text_size(size::scaled(size::BODY)).child(blocks(&parse(text), 10., typography, runs)).into_any_element()
}

fn blocks(list: &[Block], gap: f32, typography: &Typography, runs: &RowRuns) -> AnyElement {
    div().flex().flex_col().gap(px(gap)).children(list.iter().map(|b| block(b, typography, runs))).into_any_element()
}

fn block(b: &Block, typography: &Typography, runs: &RowRuns) -> AnyElement {
    match b {
        Block::Paragraph(i) => {
            div().line_height(relative(1.55)).child(runs.text(styled(i, typography))).into_any_element()
        }
        Block::Heading(level, i) => div()
            .pt(px(space::DETAIL))
            .font_weight(FontWeight::SEMIBOLD)
            .text_size(size::scaled(match level {
                1 => size::DISPLAY,
                2 => size::TITLE,
                _ => size::BODY,
            }))
            .child(runs.text(styled(i, typography)))
            .into_any_element(),
        Block::Code { lang, text } => code(lang, text, typography, runs),
        Block::Quote(inner) => div()
            .pl(px(space::INSET))
            .border_l_2()
            .border_color(tint(Tokens::accent(), 0.45))
            .text_color(Tokens::text_dim())
            .child(blocks(inner, 8., typography, runs))
            .into_any_element(),
        Block::List { start, items } => div()
            .flex()
            .flex_col()
            .gap(px(5.))
            .children(items.iter().enumerate().map(|(n, item)| {
                let marker = match start {
                    Some(first) => format!("{}.", first + n as u64),
                    None => "•".to_string(),
                };
                div()
                    .flex()
                    .gap(px(space::LINE))
                    .child(div().flex_none().min_w(px(16.)).text_color(tint(Tokens::accent(), 0.8)).child(marker))
                    .child(div().flex_1().min_w_0().child(blocks(item, 6., typography, runs)))
            }))
            .into_any_element(),
        Block::Rule => div().h(px(1.)).my(px(space::DETAIL)).bg(Tokens::edge()).into_any_element(),
        Block::Table { rows } => table(rows, typography, runs),
    }
}

/// Intrinsic column widths keep cells aligned; the viewport owns overflow.
fn table(rows: &[Vec<Inline>], typography: &Typography, runs: &RowRuns) -> AnyElement {
    let columns = rows.first().map_or(0, Vec::len);
    div()
        .id(runs.element_id("table"))
        .debug_selector(|| "agent-table".into())
        .flex()
        .w_full()
        .min_w_0()
        .overflow_x_scroll()
        .child(div().flex_none().grid().grid_cols_max_content(u16::try_from(columns).unwrap_or(u16::MAX)).children(
            rows.iter().enumerate().flat_map(|(r, row)| {
                row.iter().map(move |cell| {
                    let base = if r == 0 { Tokens::text() } else { Tokens::text_dim() };
                    div()
                        .debug_selector(move || if r == 0 { "table-header".into() } else { "table-cell".into() })
                        .px(px(space::INSET))
                        .py(px(space::LINE))
                        .border_1()
                        .border_color(Tokens::edge())
                        .whitespace_nowrap()
                        .child(runs.text(styled_with_base(cell, typography, base.into())))
                })
            }),
        ))
        .into_any_element()
}

/// A fenced block: a faint language label, then the code, unwrapped lines kept.
fn code(lang: &str, text: &str, typography: &Typography, runs: &RowRuns) -> AnyElement {
    div()
        .debug_selector(|| "agent-code".into())
        .flex()
        .flex_col()
        .rounded(px(radius::CONTROL))
        .border_1()
        .border_color(Tokens::edge())
        .bg(Tokens::sunken())
        .font_family(typography.code.family())
        .text_size(size::scaled(size::SMALL))
        .children((!lang.is_empty()).then(|| {
            div()
                .px(px(space::INSET))
                .pt(px(7.))
                .text_size(size::scaled(size::MICRO))
                .text_color(tint(Tokens::cyan(), 0.7))
                .child(lang.to_string())
        }))
        .child(div().px(px(space::INSET)).py(px(9.)).line_height(relative(1.45)).child(runs.text(code_text(
            text,
            &highlight::lines(lang, text),
            Tokens::text().into(),
            typography,
        ))))
        .into_any_element()
}

/// `text` in the mono face with each highlighted range in its class's
/// colour and the rest in `base`. `lines` has one entry per `\n`-split line.
pub fn code_text(text: &str, lines: &[highlight::Line], base: Hsla, typography: &Typography) -> Text {
    let run =
        |len: usize, color: Hsla| TextRun { len, font: font(typography.code.family()), color, ..TextRun::default() };
    // GPUI breaks shaping at colour changes. Masked code needs one run so
    // its contextual substitutions can read a key across syntax boundaries.
    if typography.code == CodeFont::ObelMasked {
        return Text::Styled(text.to_string(), vec![run(text.len(), base)]);
    }
    let mut runs = vec![];
    let mut at = 0;
    let mut start = 0;
    for (line, spans) in text.split('\n').zip(lines.iter().chain(std::iter::repeat(&vec![]))) {
        for (range, class) in spans {
            let (a, b) = (start + range.start, start + range.end);
            if a > at {
                runs.push(run(a - at, base));
            }
            runs.push(run(b - a, Tokens::code(*class)));
            at = b;
        }
        start += line.len() + 1;
    }
    if at < text.len() || runs.is_empty() {
        runs.push(run(text.len() - at, base));
    }
    Text::Styled(text.to_string(), runs)
}

/// One run per styled range, and plain runs between them.
fn styled(i: &Inline, typography: &Typography) -> Text {
    styled_with_base(i, typography, Tokens::text().into())
}

fn styled_with_base(i: &Inline, typography: &Typography, base: Hsla) -> Text {
    let run = |len: usize, mark: Mark| {
        let mut f = font(if mark.code { typography.code.value_family() } else { typography.interface.family() });
        if mark.bold {
            f.weight = FontWeight::SEMIBOLD;
        }
        if mark.italic {
            f.style = FontStyle::Italic;
        }
        let color = if mark.link {
            Tokens::sky()
        } else if mark.code {
            Tokens::cyan()
        } else if mark.bold {
            Tokens::emphasis()
        } else {
            base
        };
        TextRun {
            len,
            font: f,
            color,
            background_color: mark.code.then(|| Tokens::sunken().into()),
            underline: mark.link.then(|| UnderlineStyle {
                color: Some(tint(color, 0.5)),
                thickness: px(1.),
                wavy: false,
            }),
            strikethrough: mark.strike.then(|| StrikethroughStyle { thickness: px(1.), color: None }),
        }
    };
    let mut runs = vec![];
    let mut at = 0;
    for (range, mark) in &i.marks {
        if range.start > at {
            runs.push(run(range.start - at, Mark::default()));
        }
        runs.push(run(range.end - range.start, *mark));
        at = range.end;
    }
    if at < i.text.len() || runs.is_empty() {
        runs.push(run(i.text.len() - at, Mark::default()));
    }
    Text::Styled(i.text.clone(), runs)
}
