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

/// The most risks named.
pub const MAX_RISKS: usize = 20;

/// What in a schema diff ([`diff`]'s lines) would hurt production when the change lands
/// (PlanetScale's deploy request warnings, Atlas's migration lint), read from Postgres's lines:
/// a table or a column that goes (a rename too: the version running while it deploys still reads
/// the old name), a column whose type changes (Postgres may rewrite the table under an exclusive
/// lock) and a column made `NOT NULL` (a scan of the table under that lock). Each a sentence
/// naming the object, at most [`MAX_RISKS`].
pub fn risks(lines: &[String]) -> Vec<String> {
    // `(section, table)` → kept, and `(section, table, column)` → (before, after) type specs;
    // `section` is the `# postgres:<ref>` header lines come under when there are several.
    type Tables<'a> = BTreeSet<(&'a str, String)>;
    type Columns<'a> = std::collections::BTreeMap<(&'a str, String, String), (Option<String>, Option<String>)>;
    let mut section = "";
    let (mut tables_gone, mut tables_new): (Tables, Tables) = Default::default();
    let mut columns: Columns = Default::default();
    for l in lines {
        if let Some(h) = l.strip_prefix("# ") {
            section = h;
            continue;
        }
        let Some((sign, rest)) = l.split_once(' ') else { continue };
        let gone = match sign {
            "-" => true,
            "+" => false,
            _ => continue,
        };
        if let Some(name) = rest.strip_prefix("table ") {
            (if gone { &mut tables_gone } else { &mut tables_new }).insert((section, name.to_string()));
        } else if let Some(col) = rest.strip_prefix("column ")
            && let Some((table, column, spec)) = column_line(col)
        {
            let e = columns.entry((section, table, column)).or_default();
            if gone { e.0 = Some(spec) } else { e.1 = Some(spec) }
        }
    }
    let mut out = Vec::new();
    for (s, t) in tables_gone.difference(&tables_new) {
        out.push(format!("drops table {t}{}: production's rows go with it", at(s)));
    }
    for ((s, table, column), (before, after)) in &columns {
        let table_goes = tables_gone.contains(&(*s, table.clone())) && !tables_new.contains(&(*s, table.clone()));
        match (before, after) {
            (Some(_), None) if !table_goes => out.push(format!("drops column {table}.{column}{}: the version running while it deploys may still read it", at(s))),
            (Some(b), Some(a)) => {
                let (bt, at_) = (type_of(b), type_of(a));
                if bt != at_ {
                    out.push(format!("changes {table}.{column}{} from {bt} to {at_}: Postgres may rewrite the table under an exclusive lock", at(s)));
                }
                if a.contains(" not null") && !b.contains(" not null") {
                    out.push(format!("makes {table}.{column}{} NOT NULL: Postgres scans the table under an exclusive lock (a CHECK … NOT VALID, validated first, doesn't)", at(s)));
                }
            }
            _ => {}
        }
    }
    out.truncate(MAX_RISKS);
    out
}

fn at(section: &str) -> String {
    if section.is_empty() { String::new() } else { format!(" ({section})") }
}

/// A column line's table (`schema.table`), column and the rest (`type[ not null][ default …]`):
/// names are SQL identifiers, quoted where they need it (`%I`).
fn column_line(s: &str) -> Option<(String, String, String)> {
    let mut names = Vec::new();
    let mut rest = s;
    for _ in 0..3 {
        let (name, after) = identifier(rest)?;
        names.push(name);
        rest = after;
        if names.len() < 3 {
            rest = rest.strip_prefix('.')?;
        }
    }
    let spec = rest.strip_prefix(' ')?.to_string();
    Some((format!("{}.{}", names[0], names[1]), names[2].clone(), spec))
}

/// One identifier at the start of `s` as written (quotes kept), and what follows it.
fn identifier(s: &str) -> Option<(String, &str)> {
    if let Some(q) = s.strip_prefix('"') {
        let mut end = 0;
        let b = q.as_bytes();
        while end < b.len() {
            if b[end] == b'"' {
                if b.get(end + 1) == Some(&b'"') {
                    end += 2;
                    continue;
                }
                return Some((format!("\"{}\"", &q[..end]), &q[end + 1..]));
            }
            end += 1;
        }
        None
    } else {
        let end = s.find(['.', ' ']).unwrap_or(s.len());
        (end > 0).then(|| (s[..end].to_string(), &s[end..]))
    }
}

/// A column spec's type, without ` not null` and ` default …`.
fn type_of(spec: &str) -> &str {
    let end = [spec.find(" not null"), spec.find(" default ")].into_iter().flatten().min().unwrap_or(spec.len());
    &spec[..end]
}

#[cfg(test)]
mod tests {
    /// A migration's dangerous steps, read from the diff: a dropped table and column (a rename
    /// is one), a type change and a column made NOT NULL; an added column or index isn't one.
    #[test]
    fn a_migration_s_risks_are_named() {
        let l = |s: &str| s.to_string();
        let r = super::risks(&[
            l("- column public.orders.total integer"),
            l("- column public.users.email text"),
            l("- column public.users.nick text"),
            l("- column public.\"Odd.Name\".\"a b\" text"),
            l("- table public.sessions"),
            l("- column public.sessions.id integer not null"),
            l("+ column public.orders.total bigint"),
            l("+ column public.users.email text not null default ''::text"),
            l("+ column public.users.handle text"),
            l("+ column public.users.age integer"),
            l("+ index CREATE INDEX users_age ON public.users USING btree (age)"),
        ]);
        assert_eq!(
            r,
            [
                "drops table public.sessions: production's rows go with it",
                "drops column public.\"Odd.Name\".\"a b\": the version running while it deploys may still read it",
                "changes public.orders.total from integer to bigint: Postgres may rewrite the table under an exclusive lock",
                "makes public.users.email NOT NULL: Postgres scans the table under an exclusive lock (a CHECK … NOT VALID, validated first, doesn't)",
                "drops column public.users.nick: the version running while it deploys may still read it",
            ]
        );
    }

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
