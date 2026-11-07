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
use super::theme::{MONO, SANS, Tokens, radius, size, space, tint};
use gpui::{
    AnyElement, FontStyle, FontWeight, Hsla, IntoElement, ParentElement as _, StrikethroughStyle, Styled as _,
    StyledText, TextRun, UnderlineStyle, div, font, px, relative,
};

pub fn prose(text: &str) -> AnyElement {
    blocks(&parse(text), 10.)
}

fn blocks(list: &[Block], gap: f32) -> AnyElement {
    div().flex().flex_col().gap(px(gap)).children(list.iter().map(block)).into_any_element()
}

fn block(b: &Block) -> AnyElement {
    match b {
        Block::Paragraph(i) => div().line_height(relative(1.55)).child(styled(i)).into_any_element(),
        Block::Heading(level, i) => div()
            .pt(px(space::DETAIL))
            .font_weight(FontWeight::SEMIBOLD)
            .text_size(px(match level {
                1 => size::DISPLAY,
                2 => size::TITLE,
                _ => size::BODY,
            }))
            .child(styled(i))
            .into_any_element(),
        Block::Code { lang, text } => code(lang, text),
        Block::Quote(inner) => div()
            .pl(px(space::INSET))
            .border_l_2()
            .border_color(Tokens::edge_strong())
            .text_color(Tokens::text_dim())
            .child(blocks(inner, 8.))
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
                    .child(div().flex_none().min_w(px(16.)).text_color(Tokens::text_faint()).child(marker))
                    .child(div().flex_1().min_w_0().child(blocks(item, 6.)))
            }))
            .into_any_element(),
        Block::Rule => div().h(px(1.)).my(px(space::DETAIL)).bg(Tokens::edge()).into_any_element(),
    }
}

/// A fenced block: a faint language label, then the code, unwrapped lines kept.
pub fn code(lang: &str, text: &str) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .rounded(px(radius::CONTROL))
        .border_1()
        .border_color(Tokens::edge())
        .bg(Tokens::raised())
        .font_family(MONO)
        .text_size(px(size::LABEL))
        .children((!lang.is_empty()).then(|| {
            div()
                .px(px(space::INSET))
                .pt(px(7.))
                .text_size(px(size::MICRO))
                .text_color(Tokens::text_faint())
                .child(lang.to_string())
        }))
        .child(div().px(px(space::INSET)).py(px(9.)).line_height(relative(1.45)).child(code_text(
            text,
            &highlight::lines(lang, text),
            Tokens::text().into(),
        )))
        .into_any_element()
}

/// `text` in the mono face with each highlighted range in its class's
/// colour and the rest in `base`. `lines` has one entry per `\n`-split line.
pub fn code_text(text: &str, lines: &[highlight::Line], base: Hsla) -> StyledText {
    let run = |len: usize, color: Hsla| TextRun { len, font: font(MONO), color, ..TextRun::default() };
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
    StyledText::new(text.to_string()).with_runs(runs)
}

/// One run per styled range, and plain runs between them.
fn styled(i: &Inline) -> StyledText {
    let run = |len: usize, mark: Mark| {
        let mut f = font(if mark.code { MONO } else { SANS });
        if mark.bold {
            f.weight = FontWeight::SEMIBOLD;
        }
        if mark.italic {
            f.style = FontStyle::Italic;
        }
        let color = if mark.link { Tokens::accent() } else { Tokens::text().into() };
        TextRun {
            len,
            font: f,
            color,
            background_color: mark.code.then(|| tint(Tokens::text().into(), 0.08)),
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
    StyledText::new(i.text.clone()).with_runs(runs)
}
