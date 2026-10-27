//! Path globs for reviewer settings (`ignore`, `instructions[].path`, `auto.deep_paths`):
//! `*` matches within one path segment, `**` any number of segments, `?` one character. A
//! pattern without `/` matches the file name anywhere (`*.lock`), like gitignore.

pub fn matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim().trim_start_matches("./");
    if pattern.is_empty() {
        return false;
    }
    let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
    if !pattern.contains('/') {
        let name = path.rsplit('/').next().unwrap_or(path);
        return seg_match(pattern.as_bytes(), name.as_bytes());
    }
    // `dir/` means everything under dir.
    let owned;
    let pattern = if let Some(p) = pattern.strip_suffix('/') {
        owned = format!("{p}/**");
        owned.as_str()
    } else {
        pattern
    };
    let pat: Vec<&str> = pattern.split('/').collect();
    let segs: Vec<&str> = path.split('/').collect();
    match_segs(&pat, &segs)
}

pub fn any(patterns: &[String], path: &str) -> bool {
    patterns.iter().any(|p| matches(p, path))
}

/// Segment patterns against path segments, in O(patterns × segments): a table, not recursion,
/// so `**/**/**/…` can't explode (the pattern is a caller's, e.g. search's `path` filter).
fn match_segs(pat: &[&str], segs: &[&str]) -> bool {
    // ok[j]: the patterns so far match segs[..j].
    let mut ok = vec![false; segs.len() + 1];
    ok[0] = true;
    for p in pat {
        let mut next = vec![false; segs.len() + 1];
        if *p == "**" {
            // Any number of segments: reachable from any earlier match.
            let mut seen = false;
            for j in 0..=segs.len() {
                seen |= ok[j];
                next[j] = seen;
            }
        } else {
            for j in 1..=segs.len() {
                next[j] = ok[j - 1] && seg_match(p.as_bytes(), segs[j - 1].as_bytes());
            }
        }
        ok = next;
        if !ok.iter().any(|x| *x) {
            return false;
        }
    }
    ok[segs.len()]
}

/// `*` and `?` within one segment (iterative, no backtracking blowup).
fn seg_match(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None::<usize>, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn globs() {
        assert!(matches("*.lock", "Cargo.lock"));
        assert!(matches("*.lock", "a/b/pnpm.lock"));
        assert!(!matches("*.lock", "a/lock.rs"));
        assert!(matches("vendor/**", "vendor/x/y.rs"));
        assert!(matches("vendor/", "vendor/x/y.rs"));
        assert!(!matches("vendor/**", "src/vendor/x.rs"));
        assert!(matches("**/migrations/**", "db/migrations/001.sql"));
        assert!(matches("**/migrations/**", "migrations/001.sql"));
        assert!(matches("crates/*/src/auth/**", "crates/api/src/auth/token.rs"));
        assert!(!matches("crates/*/src/auth/**", "crates/api/x/src/auth/token.rs"));
        assert!(matches("docs/*.md", "docs/a.md"));
        assert!(!matches("docs/*.md", "docs/x/a.md"));
        assert!(matches("src/?.rs", "src/a.rs"));
        assert!(matches("/README.md", "README.md"));
        assert!(!matches("", "x"));
    }

    #[test]
    fn many_double_stars_stay_fast() {
        let deep = (0..40).map(|i| format!("d{i}")).collect::<Vec<_>>().join("/") + "/x.rs";
        let pat = std::iter::repeat_n("**", 30).collect::<Vec<_>>().join("/") + "/nope.rs";
        let t = std::time::Instant::now();
        assert!(!matches(&pat, &deep));
        assert!(matches(&(std::iter::repeat_n("**", 30).collect::<Vec<_>>().join("/") + "/x.rs"), &deep));
        assert!(matches("**/d3/**/d20/**/*.rs", &deep));
        assert!(!matches("**/d20/**/d3/**", &deep));
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "{:?}", t.elapsed());
    }
}
