//! What a change did to a preview's schema: the branch's objects against its parent's, one
//! line each (`table public.users`, `column public.users.email text not null`, an index's or a
//! constraint's definition), compared as sets. Shown on the preview, in the pull request comment
//! and the dashboard (docs/previews.md, "Schema changes").

use std::collections::BTreeSet;

/// The most lines a preview keeps: past it, one line says how many more there are.
pub const MAX_LINES: usize = 200;

/// `+ line` for what only the branch has, `- line` for what only the parent has; removals
/// first, each sorted. Capped at [`MAX_LINES`].
pub fn diff(parent: &[String], branch: &[String]) -> Vec<String> {
    let (p, b): (BTreeSet<&str>, BTreeSet<&str>) = (parent.iter().map(String::as_str).collect(), branch.iter().map(String::as_str).collect());
    let mut out: Vec<String> = p.difference(&b).map(|l| format!("- {l}")).chain(b.difference(&p).map(|l| format!("+ {l}"))).collect();
    if out.len() > MAX_LINES {
        let more = out.len() - (MAX_LINES - 1);
        out.truncate(MAX_LINES - 1);
        out.push(format!("… {more} more"));
    }
    out
}
