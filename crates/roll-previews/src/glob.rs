//! Branch globs for preview rules: `*` matches any run of characters but `/`, `**` any run at
//! all, `?` one character. Matched greedily with one backtrack point per star (linear in
//! practice, never exponential).

/// Whether `name` matches `pattern`. A pattern of `*` alone matches every branch.
pub fn matches(pattern: &str, name: &str) -> bool {
    if pattern == "*" || pattern == "**" {
        return true;
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    // (pattern index after the star, name index it resumes from, star crosses `/`)
    let mut star: Option<(usize, usize, bool)> = None;
    let (mut i, mut j) = (0, 0);
    while j < n.len() {
        if i < p.len() && p[i] == '*' {
            let deep = i + 1 < p.len() && p[i + 1] == '*';
            i += if deep { 2 } else { 1 };
            star = Some((i, j, deep));
        } else if i < p.len() && (p[i] == '?' || p[i] == n[j]) && !(p[i] == '?' && n[j] == '/') {
            i += 1;
            j += 1;
        } else if let Some((si, sj, deep)) = star {
            // The star takes one more character, unless that's a `/` it can't cross.
            if !deep && n[sj] == '/' {
                return false;
            }
            star = Some((si, sj + 1, deep));
            i = si;
            j = sj + 1;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|c| *c == '*')
}

/// Whether any of `patterns` matches `name`.
pub fn any(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| matches(p, name))
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn stars_and_slashes() {
        assert!(matches("*", "feat/x"));
        assert!(matches("feat/*", "feat/login"));
        assert!(!matches("feat/*", "feat/a/b"));
        assert!(matches("feat/**", "feat/a/b"));
        assert!(matches("release-?", "release-1"));
        assert!(!matches("release-?", "release-12"));
        assert!(matches("*-preview", "big-preview"));
        assert!(!matches("main", "maine"));
        assert!(matches("a*b*c", "aXbYc"));
        assert!(!matches("a*b*c", "aXbY"));
    }
}
