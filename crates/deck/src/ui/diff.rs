//! A tool's file change as lines kept, added and removed, with counts and a
//! short preview around each change.
//!
//! The design choice: edits an agent makes are small, so a plain
//! longest-common-subsequence over lines is enough once the shared head and
//! tail are trimmed. It is bounded: past [`MAX_CELLS`] the middle is shown
//! as removed-then-added rather than spending time on a perfect diff.

/// The largest LCS table worth filling (old × new changed lines).
pub const MAX_CELLS: usize = 1 << 22;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Same,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: Kind,
    pub text: String,
    /// 1-based line numbers in the old and new file, where the line exists.
    pub old: Option<usize>,
    pub new: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    pub lines: Vec<Line>,
    pub added: usize,
    pub removed: usize,
}

/// A row of a preview: a line, or a run of unchanged lines left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row<'a> {
    Line(&'a Line),
    Gap(usize),
}

pub fn diff(old: &str, new: &str) -> Diff {
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let head = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let tail = a[head..].iter().rev().zip(b[head..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (mid_a, mid_b) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);

    let mut ops: Vec<Kind> = vec![Kind::Same; head];
    ops.extend(middle(mid_a, mid_b));
    ops.extend(std::iter::repeat_n(Kind::Same, tail));

    let mut out = Diff::default();
    let (mut i, mut j) = (0, 0);
    for kind in ops {
        let (text, old, new) = match kind {
            Kind::Same => {
                i += 1;
                j += 1;
                (a[i - 1], Some(i), Some(j))
            }
            Kind::Removed => {
                i += 1;
                out.removed += 1;
                (a[i - 1], Some(i), None)
            }
            Kind::Added => {
                j += 1;
                out.added += 1;
                (b[j - 1], None, Some(j))
            }
        };
        out.lines.push(Line { kind, text: text.to_string(), old, new });
    }
    out
}

/// The edit script for the changed middle: LCS when it fits, else replace.
fn middle(a: &[&str], b: &[&str]) -> Vec<Kind> {
    let (n, m) = (a.len(), b.len());
    if n.saturating_mul(m) > MAX_CELLS {
        return std::iter::repeat_n(Kind::Removed, n).chain(std::iter::repeat_n(Kind::Added, m)).collect();
    }
    // lcs[i][j]: the LCS length of a[i..] and b[j..], flattened.
    let w = m + 1;
    let mut lcs = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * w + j] =
                if a[i] == b[j] { lcs[(i + 1) * w + j + 1] + 1 } else { lcs[(i + 1) * w + j].max(lcs[i * w + j + 1]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut ops = Vec::with_capacity(n + m);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push(Kind::Same);
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[(i + 1) * w + j] >= lcs[i * w + j + 1]) {
            // On a tie the removal comes first, as diffs read: `-` then `+`.
            ops.push(Kind::Removed);
            i += 1;
        } else {
            ops.push(Kind::Added);
            j += 1;
        }
    }
    ops
}

impl Diff {
    /// Changed lines with `context` unchanged lines around each, gaps
    /// between, cut to `max` rows; also how many rows were cut.
    pub fn preview(&self, context: usize, max: usize) -> (Vec<Row<'_>>, usize) {
        let near: Vec<bool> = (0..self.lines.len())
            .map(|i| {
                let lo = i.saturating_sub(context);
                let hi = (i + context + 1).min(self.lines.len());
                self.lines[lo..hi].iter().any(|l| l.kind != Kind::Same)
            })
            .collect();
        let mut rows = vec![];
        let mut skipped = 0;
        for (line, keep) in self.lines.iter().zip(&near) {
            if *keep {
                if skipped > 0 {
                    rows.push(Row::Gap(skipped));
                    skipped = 0;
                }
                rows.push(Row::Line(line));
            } else {
                skipped += 1;
            }
        }
        if skipped > 0 && !rows.is_empty() {
            rows.push(Row::Gap(skipped));
        }
        let cut = rows.len().saturating_sub(max);
        rows.truncate(max);
        (rows, cut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(d: &Diff) -> String {
        d.lines
            .iter()
            .map(|l| match l.kind {
                Kind::Same => ' ',
                Kind::Added => '+',
                Kind::Removed => '-',
            })
            .collect()
    }

    #[test]
    fn a_one_line_change_keeps_its_neighbours_and_numbers() {
        let d = diff("a\nb\nc\nd\n", "a\nB\nc\nd\nE\n");
        assert_eq!(kinds(&d), " -+  +");
        assert_eq!((d.added, d.removed), (2, 1));
        let added: Vec<_> =
            d.lines.iter().filter(|l| l.kind == Kind::Added).map(|l| (l.text.as_str(), l.new)).collect();
        assert_eq!(added, vec![("B", Some(2)), ("E", Some(5))]);
    }

    #[test]
    fn a_new_file_is_all_added() {
        let d = diff("", "x\ny");
        assert_eq!((kinds(&d).as_str(), d.added, d.removed), ("++", 2, 0));
    }

    #[test]
    fn the_preview_shows_context_and_gaps_then_cuts() {
        let old: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let named = |i: usize| match i {
            5 => "five".to_string(),
            15 => "fifteen".to_string(),
            i => i.to_string(),
        };
        let new: String = (1..=20).map(|i| format!("{}\n", named(i))).collect();
        let d = diff(&old, &new);
        let (rows, cut) = d.preview(1, 100);
        let shape: Vec<String> = rows
            .iter()
            .map(|r| match r {
                Row::Gap(n) => format!("…{n}"),
                Row::Line(l) => l.text.clone(),
            })
            .collect();
        assert_eq!(shape, ["…3", "4", "5", "five", "6", "…7", "14", "15", "fifteen", "16", "…4"]);
        assert_eq!(cut, 0);
        let (rows, cut) = d.preview(1, 4);
        assert_eq!((rows.len(), cut), (4, 7));
    }

    #[test]
    fn a_huge_change_falls_back_to_replace() {
        let old: String = (0..3000).map(|i| format!("a{i}\n")).collect();
        let new: String = (0..3000).map(|i| format!("b{i}\n")).collect();
        let d = diff(&old, &new);
        assert_eq!((d.added, d.removed), (3000, 3000));
        assert!(d.lines[..3000].iter().all(|l| l.kind == Kind::Removed));
    }
}
