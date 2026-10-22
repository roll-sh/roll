//! Line-level helpers for review threads: which lines a diff touches, carrying a thread's lines
//! from one revision of a file to the next, and suggestions (```` ```suggestion ```` blocks).

/// Lines (1-based) a diff removed from the old text and added to the new.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changed {
    pub removed: Vec<usize>,
    pub added: Vec<usize>,
}

pub fn changed_lines(old: &str, new: &str) -> Changed {
    let mut out = Changed::default();
    let patch = diffy::create_patch(old, new);
    for h in patch.hunks() {
        let (or, nr) = (h.old_range(), h.new_range());
        let mut o = if or.is_empty() { or.start() + 1 } else { or.start() };
        let mut n = if nr.is_empty() { nr.start() + 1 } else { nr.start() };
        for l in h.lines() {
            match l {
                diffy::Line::Context(_) => {
                    o += 1;
                    n += 1;
                }
                diffy::Line::Delete(_) => {
                    out.removed.push(o);
                    o += 1;
                }
                diffy::Line::Insert(_) => {
                    out.added.push(n);
                    n += 1;
                }
            }
        }
    }
    out
}

/// Where each old line (1-based index; `[0]` unused) is in the new text, `None` if removed.
fn line_map(old: &str, new: &str) -> Vec<Option<usize>> {
    let old_count = count_lines(old);
    let c = changed_lines(old, new);
    let removed: std::collections::HashSet<usize> = c.removed.iter().copied().collect();
    let mut added = c.added.iter().copied().peekable();
    let mut map = vec![None; old_count + 1];
    let mut n = 1;
    for (o, slot) in map.iter_mut().enumerate().skip(1) {
        // Skip new lines that were inserted before this old line.
        while added.peek() == Some(&n) {
            added.next();
            n += 1;
        }
        if removed.contains(&o) {
            continue;
        }
        *slot = Some(n);
        n += 1;
    }
    map
}

pub fn count_lines(s: &str) -> usize {
    if s.is_empty() { 0 } else { s.lines().count() }
}

/// Carry lines `start..=end` of `old` over to `new`: `Some((start, end))` when every line in the
/// range is unchanged and still together, `None` (outdated) otherwise.
pub fn carry(old: &str, new: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    if old == new {
        return Some((start, end));
    }
    if start == 0 || end < start || end > count_lines(old) {
        return None;
    }
    let map = line_map(old, new);
    let mut prev = None;
    for l in start..=end {
        let m = map.get(l).copied().flatten()?;
        if let Some(p) = prev
            && m != p + 1
        {
            return None;
        }
        prev = Some(m);
    }
    let s = map[start]?;
    Some((s, s + (end - start)))
}

/// The first ```` ```suggestion ```` block in a comment: the lines that should replace the
/// commented ones (`Some("")` deletes them).
pub fn suggestion(body: &str) -> Option<String> {
    let mut lines = body.lines();
    let mut fence = None;
    for l in lines.by_ref() {
        let t = l.trim_start();
        let ticks = t.chars().take_while(|&c| c == '`').count();
        if ticks >= 3 && t[ticks..].trim() == "suggestion" {
            fence = Some(ticks);
            break;
        }
    }
    let ticks = fence?;
    let mut out = Vec::new();
    for l in lines {
        let t = l.trim();
        if t.len() >= ticks && t.chars().all(|c| c == '`') {
            return Some(out.join("\n"));
        }
        out.push(l.trim_end_matches('\r'));
    }
    // An unclosed fence isn't a suggestion.
    None
}

/// Replace lines `start..=end` (1-based) of `text` with `with` (lines separated by `\n`).
/// `None` when the range isn't in the text.
pub fn replace_lines(text: &str, start: usize, end: usize, with: &str) -> Option<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if start == 0 || end < start || end > lines.len() {
        return None;
    }
    let crlf = lines[start - 1].ends_with("\r\n");
    let nl = if crlf { "\r\n" } else { "\n" };
    // Keep "no newline at end of file" when the range ends the file without one.
    let last_had_newline = lines[end - 1].ends_with('\n');
    let mut out = String::with_capacity(text.len() + with.len());
    for l in &lines[..start - 1] {
        out.push_str(l);
    }
    let replacement: Vec<&str> = if with.is_empty() { Vec::new() } else { with.split('\n').collect() };
    for (i, r) in replacement.iter().enumerate() {
        out.push_str(r.trim_end_matches('\r'));
        if i + 1 < replacement.len() || last_had_newline {
            out.push_str(nl);
        }
    }
    for l in &lines[end..] {
        out.push_str(l);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed() {
        let c = changed_lines("a\nb\nc\nd\n", "a\nB\nc\nx\nd\n");
        assert_eq!(c.removed, vec![2]);
        assert_eq!(c.added, vec![2, 4]);
        let c = changed_lines("", "a\nb\n");
        assert_eq!(c.added, vec![1, 2]);
        assert!(c.removed.is_empty());
        let c = changed_lines("a\nb\n", "");
        assert_eq!(c.removed, vec![1, 2]);
        let c = changed_lines("a\nb\nc\n", "x\na\nb\nc\n");
        assert_eq!(c.added, vec![1]);
        assert!(changed_lines("same\n", "same\n").added.is_empty());
    }

    #[test]
    fn carry_forward() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        // Lines inserted above: the thread moves down.
        let new = "0a\n0b\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        assert_eq!(carry(old, new, 5, 6), Some((7, 8)));
        // Lines removed above: it moves up.
        assert_eq!(carry(old, "1\n4\n5\n6\n7\n8\n9\n10\n", 5, 6), Some((3, 4)));
        // A change far below doesn't matter.
        assert_eq!(carry(old, "1\n2\n3\n4\n5\n6\n7\n8\n9\nTEN\n", 5, 6), Some((5, 6)));
        // A line in the range changed: outdated.
        assert_eq!(carry(old, "1\n2\n3\n4\nfive\n6\n7\n8\n9\n10\n", 5, 6), None);
        // A line inserted inside the range: outdated.
        assert_eq!(carry(old, "1\n2\n3\n4\n5\nx\n6\n7\n8\n9\n10\n", 5, 6), None);
        // Deleted file / out of range.
        assert_eq!(carry(old, "", 5, 5), None);
        assert_eq!(carry(old, old, 5, 5), Some((5, 5)));
        assert_eq!(carry(old, new, 11, 11), None);
        assert_eq!(carry(old, new, 0, 1), None);
    }

    #[test]
    fn suggestions() {
        assert_eq!(suggestion("Try this:\n```suggestion\nlet x = 2;\n```\nthanks"), Some("let x = 2;".into()));
        assert_eq!(suggestion("```suggestion\na\nb\n```"), Some("a\nb".into()));
        assert_eq!(suggestion("```suggestion\n```"), Some("".into()), "an empty block deletes the lines");
        assert_eq!(suggestion("````suggestion\n```\n````"), Some("```".into()), "longer fences can hold backticks");
        assert_eq!(suggestion("```rust\nlet x = 2;\n```"), None);
        assert_eq!(suggestion("```suggestion\nunclosed"), None);
        assert_eq!(suggestion("plain comment"), None);
    }

    #[test]
    fn replacing() {
        assert_eq!(replace_lines("a\nb\nc\n", 2, 2, "B").unwrap(), "a\nB\nc\n");
        assert_eq!(replace_lines("a\nb\nc\n", 1, 2, "x\ny\nz").unwrap(), "x\ny\nz\nc\n");
        assert_eq!(replace_lines("a\nb\nc\n", 2, 3, "").unwrap(), "a\n");
        assert_eq!(replace_lines("a\nb", 2, 2, "B").unwrap(), "a\nB", "no newline at the end stays that way");
        assert_eq!(replace_lines("a\r\nb\r\n", 1, 1, "A").unwrap(), "A\r\nb\r\n");
        assert_eq!(replace_lines("a\n", 2, 2, "x"), None);
        assert_eq!(replace_lines("a\n", 0, 1, "x"), None);
    }
}
