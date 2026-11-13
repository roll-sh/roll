//! What a change did to a preview's schema: the branch's objects against its parent's, one
//! line each (`table public.users`, `column public.users.email text not null`, an index's or a
//! constraint's definition), compared as sets. Shown on the preview, in the pull request comment
//! and the dashboard (docs/previews.md, "Schema changes").
//!
//! The names come from the change's own migrations (a fork's, too), so every line is made
//! printable here, once: control characters escaped (a name with a newline can't fake a line,
//! an escape sequence can't reach a terminal) and long lines cut.

use std::collections::BTreeSet;

/// The most lines a preview keeps, across its Postgres branches: past it, one line says how many
/// more there are.
pub const MAX_LINES: usize = 200;
/// The longest line kept (characters).
pub const MAX_LINE: usize = 300;

/// `+ line` for what only the branch has, `- line` for what only the parent has; removals
/// first, each sorted, each [`printable`]. Capped at [`MAX_LINES`].
pub fn diff(parent: &[String], branch: &[String]) -> Vec<String> {
    let (p, b): (BTreeSet<&str>, BTreeSet<&str>) = (parent.iter().map(String::as_str).collect(), branch.iter().map(String::as_str).collect());
    let out: Vec<String> = p.difference(&b).map(|l| format!("- {}", printable(l))).chain(b.difference(&p).map(|l| format!("+ {}", printable(l)))).collect();
    cap(out)
}

/// At most [`MAX_LINES`]: the last one says how many more there were.
pub fn cap(mut lines: Vec<String>) -> Vec<String> {
    if lines.len() > MAX_LINES {
        let more = lines.len() - (MAX_LINES - 1);
        lines.truncate(MAX_LINES - 1);
        lines.push(format!("… {more} more"));
    }
    lines
}

/// `s` with control characters escaped (`\n`, `\u{1b}`) and cut to [`MAX_LINE`] characters.
pub fn printable(s: &str) -> String {
    let mut out = String::new();
    for (n, c) in s.chars().enumerate() {
        if n >= MAX_LINE {
            out.push('…');
            break;
        }
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    /// A fork names a table so it looks like another line, or carries a terminal escape: neither
    /// survives.
    #[test]
    fn names_from_migrations_stay_one_printable_line() {
        let d = super::diff(&[], &["table public.\"x\n- table public.users\u{1b}]52;c;cHduZWQ=\u{7}\"".into(), "x".repeat(1000)]);
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|l| !l.chars().any(char::is_control) && l.chars().count() <= super::MAX_LINE + 3), "{d:?}");
        assert!(d.iter().any(|l| l.contains("\\n- table public.users\\u{1b}")), "{d:?}");
    }
}
