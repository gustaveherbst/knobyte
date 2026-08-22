//! Minimal line diff used by the Inbox review view.

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiffLine {
    /// `"="` unchanged, `"+"` added, `"-"` removed.
    pub op: &'static str,
    pub text: String,
}

/// Maximum number of lines per side for the LCS diff; larger inputs fall back
/// to a whole-file replacement view to bound CPU and memory.
const MAX_LINES: usize = 3000;

/// Compute a line-oriented diff between `old` and `new` (LCS based).
pub fn line_diff(old: &str, new: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();

    if a.len() > MAX_LINES || b.len() > MAX_LINES {
        let mut out: Vec<DiffLine> = a.iter().map(|l| DiffLine { op: "-", text: l.to_string() }).collect();
        out.extend(b.iter().map(|l| DiffLine { op: "+", text: l.to_string() }));
        return out;
    }

    // Trim common prefix / suffix to keep the table small.
    let mut prefix = 0;
    while prefix < a.len() && prefix < b.len() && a[prefix] == b[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < a.len() - prefix && suffix < b.len() - prefix && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix] {
        suffix += 1;
    }
    let am = &a[prefix..a.len() - suffix];
    let bm = &b[prefix..b.len() - suffix];

    let n = am.len();
    let m = bm.len();
    let mut table = vec![0u32; (n + 1) * (m + 1)];
    let idx = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[idx(i, j)] = if am[i] == bm[j] {
                table[idx(i + 1, j + 1)] + 1
            } else {
                table[idx(i + 1, j)].max(table[idx(i, j + 1)])
            };
        }
    }

    let mut out: Vec<DiffLine> = a[..prefix].iter().map(|l| DiffLine { op: "=", text: l.to_string() }).collect();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if am[i] == bm[j] {
            out.push(DiffLine { op: "=", text: am[i].to_string() });
            i += 1;
            j += 1;
        } else if table[idx(i + 1, j)] >= table[idx(i, j + 1)] {
            out.push(DiffLine { op: "-", text: am[i].to_string() });
            i += 1;
        } else {
            out.push(DiffLine { op: "+", text: bm[j].to_string() });
            j += 1;
        }
    }
    out.extend(am[i..].iter().map(|l| DiffLine { op: "-", text: l.to_string() }));
    out.extend(bm[j..].iter().map(|l| DiffLine { op: "+", text: l.to_string() }));
    out.extend(a[a.len() - suffix..].iter().map(|l| DiffLine { op: "=", text: l.to_string() }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diffs_lines() {
        let d = line_diff("a\nb\nc", "a\nx\nc\nd");
        let ops: Vec<_> = d.iter().map(|l| format!("{}{}", l.op, l.text)).collect();
        assert_eq!(ops, vec!["=a", "-b", "+x", "=c", "+d"]);
        assert!(line_diff("", "new").iter().all(|l| l.op == "+"));
    }
}
