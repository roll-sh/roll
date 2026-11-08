//! Unified diffs for Claude's file tools: from an `Edit`'s old/new strings before it runs, and
//! from the `structuredPatch` the CLI reports after (T3 Code's `claudeFileChangeText`).

use serde_json::Value;

/// Lines of context around a change.
const CONTEXT: usize = 3;
/// Above this many LCS cells the diff is one hunk replacing everything (bounded work).
const MAX_CELLS: usize = 1 << 20;

/// `--- a/path` / `+++ b/path` headers (`/dev/null` for a new file) and the hunks.
pub(super) fn with_headers(path: &str, added: bool, hunks: &str) -> String {
    let old = if added { "/dev/null".to_string() } else { format!("a/{path}") };
    format!("--- {old}\n+++ b/{path}\n{hunks}")
}

/// The hunks turning `old` into `new`, line numbers counted from the start of `old`.
pub(super) fn hunks(old: &str, new: &str) -> String {
    let a: Vec<&str> = split_lines(old);
    let b: Vec<&str> = split_lines(new);
    let ops = line_ops(&a, &b);
    render(&ops, &a, &b)
}

/// A `structuredPatch` (`[{oldStart, oldLines, newStart, newLines, lines}]`) as hunks; `None`
/// when it isn't one.
pub(super) fn structured_patch(patch: &Value) -> Option<String> {
    let hunks = patch.as_array()?;
    let mut out = String::new();
    for hunk in hunks {
        let n = |key: &str| hunk.get(key).and_then(Value::as_u64);
        let (os, ol, ns, nl) = (n("oldStart")?, n("oldLines")?, n("newStart")?, n("newLines")?);
        out.push_str(&format!("@@ -{os},{ol} +{ns},{nl} @@\n"));
        for line in hunk.get("lines")?.as_array()? {
            out.push_str(line.as_str()?);
            out.push('\n');
        }
    }
    Some(out)
}

fn split_lines(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    s.strip_suffix('\n').unwrap_or(s).split('\n').collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Same(usize, usize),
    Del(usize),
    Add(usize),
}

/// Longest-common-subsequence line diff; a full replacement when the inputs are too big.
fn line_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    // Common prefix and suffix first: edits are usually small changes in a larger snippet.
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut ops: Vec<Op> = (0..pre).map(|i| Op::Same(i, i)).collect();
    if am.len().saturating_mul(bm.len()) > MAX_CELLS {
        ops.extend((0..am.len()).map(|i| Op::Del(pre + i)));
        ops.extend((0..bm.len()).map(|j| Op::Add(pre + j)));
    } else {
        let (n, m) = (am.len(), bm.len());
        let mut lcs = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i * (m + 1) + j] = if am[i] == bm[j] {
                    lcs[(i + 1) * (m + 1) + j + 1] + 1
                } else {
                    lcs[(i + 1) * (m + 1) + j].max(lcs[i * (m + 1) + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && am[i] == bm[j] {
                ops.push(Op::Same(pre + i, pre + j));
                i += 1;
                j += 1;
            } else if i < n && (j == m || lcs[(i + 1) * (m + 1) + j] >= lcs[i * (m + 1) + j + 1]) {
                ops.push(Op::Del(pre + i));
                i += 1;
            } else {
                ops.push(Op::Add(pre + j));
                j += 1;
            }
        }
    }
    let (sa, sb) = (a.len() - suf, b.len() - suf);
    ops.extend((0..suf).map(|k| Op::Same(sa + k, sb + k)));
    ops
}

fn render(ops: &[Op], a: &[&str], b: &[&str]) -> String {
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, op)| !matches!(op, Op::Same(..))).map(|(i, _)| i).collect();
    let mut out = String::new();
    let mut k = 0;
    while k < changed.len() {
        // Grow the hunk while the next change is within two contexts of the last one.
        let start = changed[k].saturating_sub(CONTEXT);
        let mut last = changed[k];
        while k + 1 < changed.len() && changed[k + 1] <= last + 2 * CONTEXT {
            k += 1;
            last = changed[k];
        }
        let end = (last + CONTEXT + 1).min(ops.len());
        let slice = &ops[start..end];
        let old_start = position(ops, start, true);
        let new_start = position(ops, start, false);
        let old_lines = slice.iter().filter(|op| !matches!(op, Op::Add(_))).count();
        let new_lines = slice.iter().filter(|op| !matches!(op, Op::Del(_))).count();
        out.push_str(&format!(
            "@@ -{},{old_lines} +{},{new_lines} @@\n",
            if old_lines == 0 { old_start } else { old_start + 1 },
            if new_lines == 0 { new_start } else { new_start + 1 },
        ));
        for op in slice {
            let (sign, line) = match *op {
                Op::Same(i, _) => (' ', a[i]),
                Op::Del(i) => ('-', a[i]),
                Op::Add(j) => ('+', b[j]),
            };
            out.push(sign);
            out.push_str(line);
            out.push('\n');
        }
        k += 1;
    }
    out
}

/// How many old (or new) lines come before `ops[at]`.
fn position(ops: &[Op], at: usize, old: bool) -> usize {
    ops[..at].iter().filter(|op| if old { !matches!(op, Op::Add(_)) } else { !matches!(op, Op::Del(_)) }).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_edit_in_a_snippet() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nb\nc\nD\ne\nf\ng\nh\n";
        assert_eq!(hunks(old, new), "@@ -1,7 +1,7 @@\n a\n b\n c\n-d\n+D\n e\n f\n g\n");
        assert_eq!(hunks("x\n", ""), "@@ -1,1 +0,0 @@\n-x\n");
    }

    #[test]
    fn far_apart_changes_make_two_hunks() {
        let old: String = (0..20).map(|i| format!("{i}\n")).collect();
        let new = old.replace("2\n", "two\n").replace("17\n", "seventeen\n");
        let out = hunks(&old, &new);
        assert_eq!(out.matches("@@ -").count(), 2, "{out}");
        assert!(out.contains("-2\n+two\n") && out.contains("-17\n+seventeen\n"));
    }
}
