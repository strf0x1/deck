//! Syntax highlighting as classes over byte ranges: comments, strings,
//! keywords, numbers, functions, types, constants.
//!
//! The design choice: syntect parses (its bundled syntaxes, pure-Rust
//! regex), but its themes are not used. Scopes fold into a handful of
//! [`Class`]es and `theme.rs` colours them, so code uses the same palette as
//! the rest of the window. Results are cached by language and text, since
//! the visible rows are drawn every frame and code does not change once
//! written.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::sync::{Arc, LazyLock, Mutex};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    Comment,
    String,
    Keyword,
    Number,
    Function,
    Type,
    Constant,
}

/// Highlighted ranges of one line, relative to the line's start.
pub type Line = Vec<(Range<usize>, Class)>;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// Scope prefixes to classes, most specific first.
static CLASSES: LazyLock<Vec<(Scope, Class)>> = LazyLock::new(|| {
    [
        ("comment", Class::Comment),
        ("string", Class::String),
        ("constant.numeric", Class::Number),
        ("constant.character", Class::String),
        ("constant", Class::Constant),
        ("entity.name.function", Class::Function),
        ("support.function", Class::Function),
        ("variable.function", Class::Function),
        ("entity.name", Class::Type),
        ("support.type", Class::Type),
        ("support.class", Class::Type),
        ("storage.type", Class::Keyword),
        ("storage", Class::Keyword),
        ("keyword", Class::Keyword),
    ]
    .into_iter()
    .filter_map(|(s, c)| Scope::new(s).ok().map(|s| (s, c)))
    .collect()
});

/// The cache: (language, text) hash to lines. Cleared whole when full.
static CACHE: LazyLock<Mutex<HashMap<u64, Arc<Vec<Line>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
const CACHE_MAX: usize = 512;

/// The syntax for a fence label or file extension (`rust`, `rs`, `py`, …).
fn syntax(lang: &str) -> Option<&'static SyntaxReference> {
    let lang = lang.trim().to_ascii_lowercase();
    if lang.is_empty() {
        return None;
    }
    SYNTAXES.find_syntax_by_token(&lang)
}

/// Whether `lang` names a syntax this build knows.
pub fn knows(lang: &str) -> bool {
    syntax(lang).is_some()
}

/// The language of a path, from its extension.
pub fn language_of(path: &str) -> &str {
    path.rsplit_once('.').map_or("", |(_, ext)| ext)
}

/// One entry per line of `text` (split on `\n`); empty lines when the
/// language is unknown.
pub fn lines(lang: &str, text: &str) -> Arc<Vec<Line>> {
    let mut h = DefaultHasher::new();
    (lang, text).hash(&mut h);
    let key = h.finish();
    if let Some(hit) = CACHE.lock().ok().and_then(|c| c.get(&key).cloned()) {
        return hit;
    }
    let out = Arc::new(parse(lang, text));
    if let Ok(mut cache) = CACHE.lock() {
        if cache.len() >= CACHE_MAX {
            cache.clear();
        }
        cache.insert(key, out.clone());
    }
    out
}

fn parse(lang: &str, text: &str) -> Vec<Line> {
    let count = text.split('\n').count();
    let Some(syntax) = syntax(lang) else { return vec![vec![]; count] };
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    text.split('\n')
        .map(|line| {
            // The newline-aware syntaxes expect each line with its newline.
            let with_nl = format!("{line}\n");
            let Ok(ops) = state.parse_line(&with_nl, &SYNTAXES) else { return vec![] };
            let mut out: Line = vec![];
            let mut at = 0;
            for (pos, op) in ops.into_iter().chain([(with_nl.len(), syntect::parsing::ScopeStackOp::Noop)]) {
                let end = pos.min(line.len());
                if end > at
                    && let Some(class) = classify(&stack)
                {
                    match out.last_mut() {
                        Some((r, c)) if *c == class && r.end == at => r.end = end,
                        _ => out.push((at..end, class)),
                    }
                }
                at = at.max(end);
                let _ = stack.apply(&op);
            }
            out
        })
        .collect()
}

/// The class of the innermost scope that has one.
fn classify(stack: &ScopeStack) -> Option<Class> {
    stack
        .as_slice()
        .iter()
        .rev()
        .find_map(|scope| CLASSES.iter().find(|(prefix, _)| prefix.is_prefix_of(*scope)).map(|(_, class)| *class))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(lang: &str, text: &str) -> Vec<Vec<(String, Class)>> {
        let lines = lines(lang, text);
        text.split('\n')
            .zip(lines.iter())
            .map(|(l, spans)| {
                spans.iter().map(|(r, c)| (l.get(r.clone()).unwrap_or_default().to_string(), *c)).collect()
            })
            .collect()
    }

    #[test]
    fn rust_gets_keywords_strings_numbers_and_comments() {
        let got = classes("rust", "fn main() {\n    let x = \"hi\"; // greet\n    42\n}");
        assert!(got[0].contains(&("fn".into(), Class::Keyword)), "{got:?}");
        assert!(got[0].contains(&("main".into(), Class::Function)), "{got:?}");
        assert!(got[1].iter().any(|(t, c)| t.contains("hi") && *c == Class::String), "{got:?}");
        assert!(got[1].iter().any(|(t, c)| t.contains("greet") && *c == Class::Comment), "{got:?}");
        assert!(got[2].contains(&("42".into(), Class::Number)), "{got:?}");
    }

    #[test]
    fn state_carries_across_lines_and_extensions_name_languages() {
        let got = classes("py", "s = \"\"\"one\ntwo\"\"\"");
        assert!(got[1].iter().any(|(t, c)| t.contains("two") && *c == Class::String), "{got:?}");
        assert_eq!(language_of("src/lib.rs"), "rs");
        assert!(knows("rs") && knows("Rust") && !knows("") && !knows("no-such-language"));
    }

    #[test]
    fn unknown_languages_are_plain_and_lines_line_up() {
        let got = lines("no-such-language", "a\nb\n");
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(Vec::is_empty));
    }
}
