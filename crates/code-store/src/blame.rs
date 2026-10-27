//! Blame: which changeset last changed each line of a file, following first-parent history
//! (`git blame --first-parent`). Computed on demand from the changesets that touch the path and
//! a line diff against each one's first parent; callers cache the result per (head, path).

use crate::ids::ChangesetId;
use crate::path::RepoPath;
use crate::{Store, git};
use serde::Serialize;

/// Largest file blame reads.
pub const MAX_BLAME_BYTES: usize = 1 << 20;
/// Changesets walked looking for ones that touch the path.
const MAX_SCAN: usize = 200_000;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct BlameRange {
    /// 1-based, inclusive.
    pub start: u64,
    pub end: u64,
    pub changeset: ChangesetId,
}

#[derive(Clone, Debug, Serialize)]
pub struct Blame {
    pub lines: u64,
    pub ranges: Vec<BlameRange>,
    /// False when the walk stopped early (`max_changesets` or the scan budget): the lines left
    /// are attributed to the oldest changeset reached, which changed them or something older did.
    pub complete: bool,
    pub binary: bool,
}

impl Store {
    /// Blame `path` at `at`. `None` when there's no file there. `max_changesets` bounds how many
    /// changesets that touch the path are diffed.
    pub async fn blame(&self, repo: &str, at: &ChangesetId, path: &str, max_changesets: usize) -> anyhow::Result<Option<Blame>> {
        let rp = RepoPath::new(path.trim_matches('/'))?;
        let Some(file) = self.read_file(repo, at, rp.as_str()).await? else { return Ok(None) };
        if git::is_binary(&file.data) || file.data.len() > MAX_BLAME_BYTES {
            return Ok(Some(Blame { lines: 0, ranges: Vec::new(), complete: true, binary: true }));
        }
        let text = String::from_utf8_lossy(&file.data).into_owned();
        let n = split_lines(&text).len();
        let mut owner: Vec<Option<ChangesetId>> = vec![None; n];
        // (line in the file at `at`, the same line in the version being looked at)
        let mut tracked: Vec<(usize, usize)> = (0..n).map(|i| (i, i)).collect();
        let mut cur = *at;
        let mut cur_text = text;
        let mut complete = true;
        let mut steps = 0;
        let mut scanned = 0;
        while !tracked.is_empty() {
            // The next changeset at or below `cur` on the first-parent chain that touches the path.
            let touching = loop {
                scanned += 1;
                let m = self.meta_or_404(repo, &cur)?;
                if m.cs.files.contains_key(&rp) {
                    break Some(m);
                }
                match m.cs.parents.first() {
                    Some(p) if scanned < MAX_SCAN => cur = *p,
                    _ => break None,
                }
            };
            let Some(m) = touching else {
                // Ran out of history (or budget) without finding where the lines came from.
                complete = scanned < MAX_SCAN && complete;
                for (orig, _) in tracked.drain(..) {
                    owner[orig] = Some(cur);
                }
                break;
            };
            let id = m.id;
            steps += 1;
            let parent = m.cs.parents.first().copied();
            let old = match parent {
                Some(p) => self.read_file(repo, &p, rp.as_str()).await?.filter(|f| !git::is_binary(&f.data) && f.data.len() <= MAX_BLAME_BYTES),
                None => None,
            };
            let Some(old) = old else {
                for (orig, _) in tracked.drain(..) {
                    owner[orig] = Some(id);
                }
                break;
            };
            let old_text = String::from_utf8_lossy(&old.data).into_owned();
            let map = line_map(&old_text, &cur_text);
            let mut next = Vec::with_capacity(tracked.len());
            for (orig, line) in tracked.drain(..) {
                match map.get(line).copied().flatten() {
                    Some(old_line) => next.push((orig, old_line)),
                    None => owner[orig] = Some(id),
                }
            }
            tracked = next;
            let Some(p) = parent else { break };
            if steps >= max_changesets.max(1) && !tracked.is_empty() {
                complete = false;
                for (orig, _) in tracked.drain(..) {
                    owner[orig] = Some(id);
                }
                break;
            }
            cur = p;
            cur_text = old_text;
        }
        let mut ranges: Vec<BlameRange> = Vec::new();
        for (i, o) in owner.into_iter().enumerate() {
            let cs = o.unwrap_or(*at);
            let line = i as u64 + 1;
            match ranges.last_mut() {
                Some(r) if r.changeset == cs && r.end + 1 == line => r.end = line,
                _ => ranges.push(BlameRange { start: line, end: line, changeset: cs }),
            }
        }
        Ok(Some(Blame { lines: n as u64, ranges, complete, binary: false }))
    }
}

/// Lines with their terminators, as diffy sees them.
fn split_lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

/// For each line of `new`, the line of `old` it's unchanged from (a Myers line diff), if any.
pub(crate) fn line_map(old: &str, new: &str) -> Vec<Option<usize>> {
    let new_len = split_lines(new).len();
    let mut map = vec![None; new_len];
    let patch = diffy::create_patch(old, new);
    let (mut o, mut n) = (0usize, 0usize);
    for h in patch.hunks() {
        let r = h.old_range();
        let start = if r.is_empty() { r.start() } else { r.start() - 1 };
        while o < start && n < new_len {
            map[n] = Some(o);
            o += 1;
            n += 1;
        }
        for l in h.lines() {
            match l {
                diffy::Line::Context(_) => {
                    if n < new_len {
                        map[n] = Some(o);
                    }
                    o += 1;
                    n += 1;
                }
                diffy::Line::Delete(_) => o += 1,
                diffy::Line::Insert(_) => n += 1,
            }
        }
    }
    while n < new_len {
        map[n] = Some(o);
        o += 1;
        n += 1;
    }
    map
}

#[cfg(test)]
mod tests {
    use super::line_map;

    #[test]
    fn maps_unchanged_lines() {
        assert_eq!(line_map("a\nb\nc\n", "a\nb\nc\n"), vec![Some(0), Some(1), Some(2)]);
        assert_eq!(line_map("a\nb\nc\n", "a\nx\nb\nc\n"), vec![Some(0), None, Some(1), Some(2)]);
        assert_eq!(line_map("a\nb\nc\n", "a\nc\n"), vec![Some(0), Some(2)]);
        assert_eq!(line_map("", "a\nb\n"), vec![None, None]);
        assert_eq!(line_map("a\nb\n", "b\n"), vec![Some(1)]);
        assert_eq!(line_map("a\nb", "a\nb\nc"), vec![Some(0), None, None], "a changed last line (no newline) is new");
        let old: String = (0..40).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l20\n", "changed\n");
        let m = line_map(&old, &new);
        assert_eq!(m[19], Some(19));
        assert_eq!(m[20], None);
        assert_eq!(m[39], Some(39));
    }
}
