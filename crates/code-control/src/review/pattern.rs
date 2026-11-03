//! Path patterns with gitignore semantics, as CODEOWNERS uses them (and `.roll/review.yaml`'s
//! `paths` globs):
//!
//! - `*` and `?` match within one path segment, `[abc]` / `[a-z]` / `[!a]` a character class;
//!   `**` as a whole segment matches any number of segments.
//! - A pattern with a `/` anywhere but at the end is anchored at the root (`docs/*`,
//!   `/build/`); one without (`*.js`, `apps/`) matches at any depth.
//! - A trailing `/` matches directories only. A pattern that matches a directory matches
//!   everything under it, except one ending in `/*` (CODEOWNERS: `docs/*` owns
//!   `docs/a.md` but not `docs/sub/b.md`).

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    /// As written.
    pub text: String,
    segments: Vec<String>,
    dir_only: bool,
}

impl Pattern {
    /// `None` for an empty pattern.
    pub fn new(text: &str) -> Option<Pattern> {
        let raw = text.trim();
        if raw.is_empty() || raw == "/" {
            return None;
        }
        let dir_only = raw.ends_with('/');
        let body = raw.trim_end_matches('/');
        let anchored = body.starts_with('/') || body.contains('/');
        let body = body.trim_start_matches('/');
        if body.is_empty() {
            return None;
        }
        let mut segments: Vec<String> = Vec::new();
        if !anchored {
            segments.push("**".into());
        }
        for s in body.split('/').filter(|s| !s.is_empty()) {
            // `a/**/**/b` is `a/**/b`.
            if s == "**" && segments.last().is_some_and(|l| l == "**") {
                continue;
            }
            segments.push(s.to_string());
        }
        Some(Pattern { text: raw.to_string(), segments, dir_only })
    }

    /// Does the pattern match file `path` (or a directory that contains it)?
    pub fn matches(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return false;
        }
        // Every directory above the file, then (unless dir-only) the file itself. A pattern whose
        // last segment is a bare `*` only matches files directly in its directory.
        let last = if self.dir_only { parts.len() - 1 } else { parts.len() };
        let first = if self.segments.last().is_some_and(|s| s == "*") && self.segments.len() > 1 { last } else { 1 };
        (first..=last).any(|n| match_segments(&self.segments, &parts[..n]))
    }
}

/// Patterns against path segments in O(patterns × segments) (a table, not recursion: a
/// CODEOWNERS line like `a/**/b/**/c/**/…` can't blow up).
fn match_segments(pat: &[String], path: &[&str]) -> bool {
    let mut ok = vec![false; path.len() + 1];
    ok[0] = true;
    for p in pat {
        let mut next = vec![false; path.len() + 1];
        if p == "**" {
            let mut seen = false;
            for j in 0..=path.len() {
                seen |= ok[j];
                next[j] = seen;
            }
        } else {
            for j in 1..=path.len() {
                next[j] = ok[j - 1] && wildcard(p.as_bytes(), path[j - 1].as_bytes());
            }
        }
        ok = next;
        if !ok.iter().any(|x| *x) {
            return false;
        }
    }
    ok[path.len()]
}

/// `*`, `?` and `[...]` within one segment.
fn wildcard(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut star_s) = (None::<usize>, 0);
    while si < s.len() {
        if pi < p.len() {
            match p[pi] {
                b'*' => {
                    star = Some(pi);
                    star_s = si;
                    pi += 1;
                    continue;
                }
                b'?' => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                b'[' => {
                    if let Some((hit, next)) = class(&p[pi..], s[si]) {
                        if hit {
                            pi += next;
                            si += 1;
                            continue;
                        }
                    } else if s[si] == b'[' {
                        // An unclosed `[` is literal.
                        pi += 1;
                        si += 1;
                        continue;
                    }
                }
                b'\\' if pi + 1 < p.len() => {
                    if p[pi + 1] == s[si] {
                        pi += 2;
                        si += 1;
                        continue;
                    }
                }
                c if c == s[si] => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some(st) => {
                pi = st + 1;
                star_s += 1;
                si = star_s;
            }
            None => return false,
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

/// A `[...]` class at the start of `p`: whether `c` is in it and the class's length.
fn class(p: &[u8], c: u8) -> Option<(bool, usize)> {
    let end = p.iter().skip(1).position(|&b| b == b']').map(|i| i + 1)?;
    if end == 1 {
        return None;
    }
    let mut body = &p[1..end];
    let negate = matches!(body.first(), Some(b'!') | Some(b'^'));
    if negate {
        body = &body[1..];
    }
    let mut hit = false;
    let mut i = 0;
    while i < body.len() {
        if i + 2 < body.len() && body[i + 1] == b'-' {
            if body[i] <= c && c <= body[i + 2] {
                hit = true;
            }
            i += 3;
        } else {
            if body[i] == c {
                hit = true;
            }
            i += 1;
        }
    }
    Some((hit != negate, end + 1))
}

/// Any of `patterns` matches `path`.
pub fn any_match(patterns: &[Pattern], path: &str) -> bool {
    patterns.iter().any(|p| p.matches(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn many_double_stars_stay_fast() {
        let deep = (0..40).map(|i| format!("d{i}")).collect::<Vec<_>>().join("/") + "/x.rs";
        let spread = (0..15).map(|i| format!("**/q{i}")).collect::<Vec<_>>().join("/");
        let t = std::time::Instant::now();
        assert!(!Pattern::new(&format!("/{spread}")).unwrap().matches(&deep));
        assert!(Pattern::new("/**/d3/**/d20/**/x.rs").unwrap().matches(&deep));
        assert!(!Pattern::new("/**/d20/**/d3/**/x.rs").unwrap().matches(&deep));
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "{:?}", t.elapsed());
    }

    fn m(p: &str, path: &str) -> bool {
        Pattern::new(p).unwrap().matches(path)
    }

    #[test]
    fn codeowners_examples() {
        // From GitHub's CODEOWNERS documentation.
        assert!(m("*", "a/b/c.txt"));
        assert!(m("*.js", "src/app/index.js"));
        assert!(!m("*.js", "src/app/index.jsx"));
        assert!(m("*.go", "main.go"));
        assert!(m("/build/logs/", "build/logs/today.log"));
        assert!(!m("/build/logs/", "x/build/logs/today.log"));
        assert!(m("docs/*", "docs/getting-started.md"));
        assert!(!m("docs/*", "docs/build-app/troubleshooting.md"));
        assert!(!m("docs/*", "x/docs/a.md"));
        assert!(m("apps/", "apps/web/index.ts"));
        assert!(m("apps/", "deep/apps/web/index.ts"));
        assert!(!m("apps/", "apps"), "a trailing slash matches directories only");
        assert!(m("/docs/", "docs/a/b.md"));
        assert!(m("**/logs", "build/logs/a.log"));
        assert!(m("**/logs", "logs/a.log"));
        assert!(m("/scripts/", "scripts/x.sh"));
        assert!(m("/apps/github", "apps/github"));
        assert!(m("/apps/github", "apps/github/x.rs"));
        assert!(!m("/apps/github", "apps/githubber"));
    }

    #[test]
    fn double_star_and_classes() {
        assert!(m("docs/**", "docs/a/b/c.md"));
        assert!(!m("docs/**", "x/docs/a.md"));
        assert!(m("**/*.md", "README.md"));
        assert!(m("**/*.md", "a/b/c.md"));
        assert!(m("a/**/b", "a/b"));
        assert!(m("a/**/b", "a/x/y/b"));
        assert!(m("a/**/b", "a/x/y/b/z.rs"));
        assert!(m("crates/*/src/auth/**", "crates/code-control/src/auth/token.rs"));
        assert!(!m("crates/*/src/auth/**", "crates/a/b/src/auth/token.rs"));
        assert!(m("file[0-9].txt", "file7.txt"));
        assert!(!m("file[!0-9].txt", "file7.txt"));
        assert!(m("file[!0-9].txt", "filex.txt"));
        assert!(m("?.rs", "a.rs"));
        assert!(!m("?.rs", "ab.rs"));
        assert!(m("*.lock", "Cargo.lock"));
        assert!(m("vendor/**", "vendor/x/y"));
        assert!(m("a*b*c", "aXXbYYc"));
        assert!(!m("a*b*c", "aXXbYY"));
        assert!(m("[ab", "[ab"), "an unclosed class is literal");
        assert!(Pattern::new("").is_none());
        assert!(Pattern::new("/").is_none());
    }
}
