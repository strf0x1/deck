//! A Git unified patch read back into the edit cards' diff lines, one file
//! at a time, so a worktree review draws with the same rows as an edit.
//!
//! The design choice: line numbers come from the hunk headers and the lines
//! between hunks become gaps. A patch carries no whole file, so each side's
//! text is only its hunk lines, in order; a row's `ix` indexes that text,
//! which is what the view highlights.

use super::diff::{Kind, Line, Row as DiffRow};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A line and its index into its side's text (`old` when removed).
    Line(Line, usize),
    Gap(usize),
}

impl Row {
    pub fn as_diff(&self) -> DiffRow<'_> {
        match self {
            Row::Line(line, _) => DiffRow::Line(line),
            Row::Gap(n) => DiffRow::Gap(*n),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct File {
    pub path: String,
    /// What Git said about the file besides its lines: new, deleted,
    /// renamed, binary.
    pub notes: Vec<String>,
    pub rows: Vec<Row>,
    pub added: usize,
    pub removed: usize,
    pub old: String,
    pub new: String,
}

impl File {
    fn push(&mut self, kind: Kind, text: &str, old: Option<usize>, new: Option<usize>) {
        let side = if kind == Kind::Removed { &mut self.old } else { &mut self.new };
        let ix = side.matches('\n').count();
        side.push_str(text);
        side.push('\n');
        if kind == Kind::Same {
            self.old.push_str(text);
            self.old.push('\n');
        }
        match kind {
            Kind::Added => self.added += 1,
            Kind::Removed => self.removed += 1,
            Kind::Same => {}
        }
        self.rows.push(Row::Line(Line { kind, text: text.to_string(), old, new }, ix));
    }
}

/// Where a hunk is: the next line number on each side and the lines left.
struct Hunk {
    old: usize,
    new: usize,
    old_left: usize,
    new_left: usize,
}

pub fn parse(text: &str) -> Vec<File> {
    let mut files: Vec<File> = vec![];
    let mut hunk: Option<Hunk> = None;
    // The first old line not yet shown, for the gap before the next hunk.
    let mut shown = 1;
    for raw in text.lines() {
        if let Some(h) = hunk.as_mut().filter(|h| h.old_left + h.new_left > 0)
            && let Some(file) = files.last_mut()
        {
            let (kind, body) = match raw.split_at_checked(1) {
                Some(("\\", _)) => continue,
                Some((" ", body)) => (Kind::Same, body),
                Some(("+", body)) => (Kind::Added, body),
                Some(("-", body)) => (Kind::Removed, body),
                None => (Kind::Same, ""),
                Some(_) => {
                    hunk = None;
                    header(&mut files, &mut hunk, &mut shown, raw);
                    continue;
                }
            };
            let old = (kind != Kind::Added).then_some(h.old);
            let new = (kind != Kind::Removed).then_some(h.new);
            if kind != Kind::Added {
                h.old += 1;
                h.old_left = h.old_left.saturating_sub(1);
            }
            if kind != Kind::Removed {
                h.new += 1;
                h.new_left = h.new_left.saturating_sub(1);
            }
            shown = h.old;
            file.push(kind, body, old, new);
            continue;
        }
        header(&mut files, &mut hunk, &mut shown, raw);
    }
    files
}

/// A line outside a hunk: a file's start, its headers, or a hunk's.
fn header(files: &mut Vec<File>, hunk: &mut Option<Hunk>, shown: &mut usize, raw: &str) {
    if let Some(rest) = raw.strip_prefix("diff --git ") {
        let path = rest.rsplit_once(" b/").map_or(rest, |(_, b)| b);
        files.push(File { path: unquote(path), ..File::default() });
        *hunk = None;
        *shown = 1;
        return;
    }
    let Some(file) = files.last_mut() else { return };
    if let Some(range) = raw.strip_prefix("@@ ") {
        let Some(h) = hunk_header(range) else { return };
        if h.old > *shown {
            file.rows.push(Row::Gap(h.old - *shown));
        }
        *hunk = Some(h);
    } else if let Some(path) = raw.strip_prefix("+++ b/") {
        file.path = unquote(path);
    } else if let Some(path) = raw.strip_prefix("--- a/").filter(|_| file.path.is_empty()) {
        file.path = unquote(path);
    } else if raw.starts_with("new file mode") {
        file.notes.push("New file".into());
    } else if raw.starts_with("deleted file mode") {
        file.notes.push("Deleted".into());
    } else if let Some(from) = raw.strip_prefix("rename from ") {
        file.notes.push(format!("Renamed from {}", unquote(from)));
    } else if raw.starts_with("Binary files ") {
        file.notes.push("Binary file".into());
    }
}

/// `-a,b +c,d @@ …`, a count of one when it is left out.
fn hunk_header(range: &str) -> Option<Hunk> {
    let mut parts = range.split_whitespace();
    let side = |part: Option<&str>, sign: char| -> Option<(usize, usize)> {
        let part = part?.strip_prefix(sign)?;
        let (start, len) = part.split_once(',').unwrap_or((part, "1"));
        Some((start.parse().ok()?, len.parse().ok()?))
    };
    let (old, old_left) = side(parts.next(), '-')?;
    let (new, new_left) = side(parts.next(), '+')?;
    // A side with no lines names the line before it (0 for an empty file).
    let first = |start: usize, len: usize| if len == 0 { start + 1 } else { start };
    Some(Hunk { old: first(old, old_left), new: first(new, new_left), old_left, new_left })
}

fn unquote(path: &str) -> String {
    path.trim_matches('"').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(file: &File) -> Vec<String> {
        file.rows
            .iter()
            .map(|row| match row {
                Row::Gap(n) => format!("…{n}"),
                Row::Line(l, ix) => {
                    let sign = match l.kind {
                        Kind::Same => ' ',
                        Kind::Added => '+',
                        Kind::Removed => '-',
                    };
                    let n = |n: Option<usize>| n.map_or("_".into(), |n| n.to_string());
                    format!("{sign}{} {}/{} #{ix}", l.text, n(l.old), n(l.new))
                }
            })
            .collect()
    }

    #[test]
    fn hunks_keep_their_line_numbers_and_the_lines_between_become_gaps() {
        let text = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -3,3 +3,3 @@ fn main() {
 a
-b
+B
 c
@@ -10,2 +10,3 @@
 x
+y
 z
\\ No newline at end of file
";
        let files = parse(text);
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!((file.path.as_str(), file.added, file.removed), ("src/lib.rs", 2, 1));
        assert_eq!(
            shape(file),
            [
                "…2",
                " a 3/3 #0",
                "-b 4/_ #1",
                "+B _/4 #1",
                " c 5/5 #2",
                "…4",
                " x 10/10 #3",
                "+y _/11 #4",
                " z 11/12 #5"
            ]
        );
        assert_eq!(file.old, "a\nb\nc\nx\nz\n");
        assert_eq!(file.new, "a\nB\nc\nx\ny\nz\n");
    }

    #[test]
    fn new_deleted_renamed_and_binary_files_say_so() {
        let text = "\
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+one
+diff --git looks like a header
diff --git a/old.txt b/old.txt
deleted file mode 100644
--- a/old.txt
+++ /dev/null
@@ -1 +0,0 @@
-gone
diff --git a/a.png b/b.png
similarity index 90%
rename from a.png
rename to b.png
Binary files a/a.png and b/b.png differ
";
        let files = parse(text);
        let summary: Vec<_> = files.iter().map(|f| (f.path.as_str(), f.notes.join(", "), f.added, f.removed)).collect();
        assert_eq!(
            summary,
            [
                ("new.txt", "New file".to_string(), 2, 0),
                ("old.txt", "Deleted".to_string(), 0, 1),
                ("b.png", "Renamed from a.png, Binary file".to_string(), 0, 0),
            ]
        );
        assert_eq!(shape(&files[0]), ["+one _/1 #0", "+diff --git looks like a header _/2 #1"]);
        assert_eq!(shape(&files[1]), ["-gone 1/_ #0"]);
    }

    #[test]
    fn a_patch_cut_mid_hunk_keeps_what_arrived() {
        let files = parse("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,5 +1,5 @@\n a\n-b\n+B");
        assert_eq!(shape(&files[0]), [" a 1/1 #0", "-b 2/_ #1", "+B _/2 #1"]);
        assert!(parse("").is_empty());
    }
}
