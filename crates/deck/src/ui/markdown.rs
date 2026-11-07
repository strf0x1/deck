//! Agent prose as blocks: paragraphs, headings, code, quotes, lists and
//! rules, each paragraph a string with styled ranges.
//!
//! The design choice: parse to a small pure tree with pulldown-cmark, and
//! keep inline styling as byte ranges over one string, which is the shape
//! GPUI's `StyledText` draws. A streaming reply is re-parsed as it grows;
//! an unclosed fence is a code block to the end, which is what a half-sent
//! reply should look like. HTML and tables are shown as their text.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::ops::Range;

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Paragraph(Inline),
    Heading(u8, Inline),
    Code {
        lang: String,
        text: String,
    },
    Quote(Vec<Block>),
    /// `start` is the first number of an ordered list.
    List {
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Rule,
}

/// Text with styled ranges; ranges are ordered and do not overlap.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inline {
    pub text: String,
    pub marks: Vec<(Range<usize>, Mark)>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mark {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
    pub link: bool,
}

impl Inline {
    fn push(&mut self, text: &str, mark: Mark) {
        let start = self.text.len();
        self.text.push_str(text);
        if mark == Mark::default() || text.is_empty() {
            return;
        }
        match self.marks.last_mut() {
            Some((r, m)) if *m == mark && r.end == start => r.end = self.text.len(),
            _ => self.marks.push((start..self.text.len(), mark)),
        }
    }
}

pub fn parse(src: &str) -> Vec<Block> {
    let mut b = Builder { frames: vec![Frame::Blocks(vec![])], ..Builder::default() };
    for event in Parser::new_ext(src, Options::ENABLE_STRIKETHROUGH) {
        b.event(event);
    }
    b.flush();
    while b.frames.len() > 1 {
        b.close();
    }
    match b.frames.pop() {
        Some(Frame::Blocks(blocks)) => blocks,
        _ => vec![],
    }
}

/// Open containers. `Blocks` is the root, a quote, or a list item.
#[derive(Debug)]
enum Frame {
    Blocks(Vec<Block>),
    Quote(Vec<Block>),
    List { start: Option<u64>, items: Vec<Vec<Block>> },
}

#[derive(Default)]
struct Builder {
    frames: Vec<Frame>,
    inline: Option<Inline>,
    heading: Option<u8>,
    code: Option<(String, String)>,
    bold: u8,
    italic: u8,
    strike: u8,
    link: u8,
}

impl Builder {
    fn mark(&self) -> Mark {
        Mark { bold: self.bold > 0, italic: self.italic > 0, strike: self.strike > 0, link: self.link > 0, code: false }
    }

    fn text(&mut self, text: &str, mark: Mark) {
        if let Some((_, code)) = &mut self.code {
            code.push_str(text);
        } else {
            self.inline.get_or_insert_with(Inline::default).push(text, mark);
        }
    }

    fn block(&mut self, block: Block) {
        match self.frames.last_mut() {
            Some(Frame::Blocks(v) | Frame::Quote(v)) => v.push(block),
            // Text directly in a list (no item open) cannot happen; keep it anyway.
            Some(Frame::List { items, .. }) => items.push(vec![block]),
            None => self.frames.push(Frame::Blocks(vec![block])),
        }
    }

    /// A tight list item's text arrives without a paragraph; close it as one.
    fn flush(&mut self) {
        if let Some(inline) = self.inline.take() {
            match self.heading.take() {
                Some(level) => self.block(Block::Heading(level, inline)),
                None => self.block(Block::Paragraph(inline)),
            }
        }
    }

    /// Close the innermost container into its parent.
    fn close(&mut self) {
        self.flush();
        let block = match self.frames.pop() {
            Some(Frame::Quote(v)) => Block::Quote(v),
            Some(Frame::List { start, items }) => Block::List { start, items },
            Some(Frame::Blocks(v)) => {
                // A list item: it joins the list below it.
                if let Some(Frame::List { items, .. }) = self.frames.last_mut() {
                    items.push(v);
                    return;
                }
                return self.frames.push(Frame::Blocks(v));
            }
            None => return,
        };
        self.block(block);
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) => self.text(&t, self.mark()),
            Event::Code(t) => self.text(&t, Mark { code: true, ..self.mark() }),
            Event::InlineMath(t) | Event::DisplayMath(t) => self.text(&t, Mark { code: true, ..self.mark() }),
            Event::SoftBreak => self.text(" ", self.mark()),
            Event::HardBreak => self.text("\n", self.mark()),
            Event::Rule => {
                self.flush();
                self.block(Block::Rule);
            }
            Event::FootnoteReference(t) => self.text(&format!("[{t}]"), self.mark()),
            Event::TaskListMarker(done) => self.text(if done { "☑ " } else { "☐ " }, self.mark()),
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.flush(),
            Tag::Heading { level, .. } => {
                self.flush();
                self.heading = Some(level as u8);
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or_default().to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.frames.push(Frame::Quote(vec![]));
            }
            Tag::List(start) => {
                self.flush();
                self.frames.push(Frame::List { start, items: vec![] });
            }
            Tag::Item => self.frames.push(Frame::Blocks(vec![])),
            Tag::Strong => self.bold += 1,
            Tag::Emphasis => self.italic += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { .. } => self.link += 1,
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) => self.flush(),
            TagEnd::CodeBlock => {
                if let Some((lang, mut text)) = self.code.take() {
                    if text.ends_with('\n') {
                        text.pop();
                    }
                    self.block(Block::Code { lang, text });
                }
            }
            TagEnd::BlockQuote(_) | TagEnd::List(_) | TagEnd::Item => self.close(),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => self.link = self.link.saturating_sub(1),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para(text: &str) -> Block {
        Block::Paragraph(Inline { text: text.into(), marks: vec![] })
    }

    #[test]
    fn inline_styles_become_ranges_over_one_string() {
        let [Block::Paragraph(p)] = &parse("Run **`cargo test`** then *read* ~~it~~ [docs](x).")[..] else { panic!() };
        assert_eq!(p.text, "Run cargo test then read it docs.");
        let bold_code = Mark { bold: true, code: true, ..Mark::default() };
        assert_eq!(
            p.marks,
            vec![
                (4..14, bold_code),
                (20..24, Mark { italic: true, ..Mark::default() }),
                (25..27, Mark { strike: true, ..Mark::default() }),
                (28..32, Mark { link: true, ..Mark::default() }),
            ]
        );
    }

    #[test]
    fn blocks_nest_and_tight_items_are_paragraphs() {
        let src = "# Plan\n\n1. read\n2. edit\n   - nested\n\n> quoted\n\n---\n\n```rust\nfn x() {}\n```\n";
        assert_eq!(
            parse(src),
            vec![
                Block::Heading(1, Inline { text: "Plan".into(), marks: vec![] }),
                Block::List {
                    start: Some(1),
                    items: vec![
                        vec![para("read")],
                        vec![para("edit"), Block::List { start: None, items: vec![vec![para("nested")]] }],
                    ],
                },
                Block::Quote(vec![para("quoted")]),
                Block::Rule,
                Block::Code { lang: "rust".into(), text: "fn x() {}".into() },
            ]
        );
    }

    #[test]
    fn a_half_streamed_reply_still_parses() {
        assert_eq!(
            parse("Here:\n```sh\ncargo te"),
            vec![para("Here:"), Block::Code { lang: "sh".into(), text: "cargo te".into() }]
        );
        assert_eq!(parse("**bol"), vec![para("**bol")]);
        assert_eq!(parse("line one\nline two"), vec![para("line one line two")]);
    }
}
