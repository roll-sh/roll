//! SQL is written once, in the SQLite dialect the code always used, and translated for Postgres:
//!
//! - placeholders: `?` and `?N` become `$N` (a bare `?` is one more than the largest number so
//!   far, as in SQLite);
//! - `INSERT OR IGNORE INTO …` becomes `INSERT INTO … ON CONFLICT DO NOTHING`.
//!
//! Anything else must already be portable. `INSERT OR REPLACE` isn't (SQLite deletes the old row
//! first), so it's refused: write an `ON CONFLICT (…) DO UPDATE` instead.
//!
//! Schemas ([`pg_ddl`]) are translated too, so both backends are made from one definition:
//! `INTEGER` → `BIGINT`, `REAL` → `DOUBLE PRECISION`, `BLOB` → `BYTEA`, `TEXT` → `TEXT COLLATE
//! "C"` (byte order, like SQLite), `INTEGER PRIMARY KEY AUTOINCREMENT` → an identity column, and
//! every rowid table gets a `rowid` identity column, so `ORDER BY rowid` keeps insertion order.

use crate::{Error, Result};

/// Split `sql` into runs of code (`true`) and the literals, quoted identifiers and comments that
/// must be left alone (`false`).
fn spans(sql: &str) -> Vec<(bool, &str)> {
    let mut out = Vec::new();
    let b = sql.as_bytes();
    let (mut i, mut start) = (0, 0);
    while i < b.len() {
        let end = match b[i] {
            q @ (b'\'' | b'"' | b'`') => {
                let mut j = i + 1;
                loop {
                    if j >= b.len() {
                        break j;
                    }
                    if b[j] == q {
                        if j + 1 < b.len() && b[j + 1] == q {
                            j += 2;
                            continue;
                        }
                        break j + 1;
                    }
                    j += 1;
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-') => sql[i..].find('\n').map(|n| i + n).unwrap_or(b.len()),
            b'/' if b.get(i + 1) == Some(&b'*') => sql[i + 2..].find("*/").map(|n| i + 2 + n + 2).unwrap_or(b.len()),
            _ => {
                i += 1;
                continue;
            }
        };
        if start < i {
            out.push((true, &sql[start..i]));
        }
        out.push((false, &sql[i..end]));
        i = end;
        start = end;
    }
    if start < b.len() {
        out.push((true, &sql[start..]));
    }
    out
}

/// Translate one statement (or several, `;`-separated) for Postgres.
pub fn pg_sql(sql: &str) -> Result<String> {
    let mut out = String::with_capacity(sql.len() + 32);
    let mut max = 0usize;
    let mut err = None;
    for (is_code, code) in spans(sql) {
        if !is_code {
            out.push_str(code);
            continue;
        }
        {
            let b = code.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'?' {
                    let mut j = i + 1;
                    while j < b.len() && b[j].is_ascii_digit() {
                        j += 1;
                    }
                    let n = if j > i + 1 {
                        match code[i + 1..j].parse::<usize>() {
                            Ok(n) if n > 0 => n,
                            _ => {
                                err = Some(Error::Other(format!("bad placeholder {}", &code[i..j])));
                                0
                            }
                        }
                    } else {
                        max + 1
                    };
                    max = max.max(n);
                    out.push('$');
                    out.push_str(&n.to_string());
                    i = j;
                } else {
                    let ch = code[i..].chars().next().unwrap();
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
        }
    }
    if let Some(e) = err {
        return Err(e);
    }
    rewrite_inserts(&out)
}

/// Uppercase words of the code parts of `sql`, with their byte offsets.
fn words(sql: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut off = 0;
    for (is_code, part) in spans(sql) {
        let len = part.len();
        if is_code {
            let mut start = None;
            for (i, ch) in part.char_indices().chain(std::iter::once((part.len(), ' '))) {
                if ch.is_ascii_alphanumeric() || ch == '_' {
                    start.get_or_insert(i);
                } else if let Some(s) = start.take() {
                    out.push((off + s, part[s..i].to_ascii_uppercase()));
                }
            }
        } else {
            out.push((off, String::new()));
        }
        off += len;
    }
    out
}

fn rewrite_inserts(sql: &str) -> Result<String> {
    let w = words(sql);
    let mut edits: Vec<(usize, usize, &str)> = Vec::new();
    for (k, (at, word)) in w.iter().enumerate() {
        if word != "INSERT" || w.get(k + 1).map(|x| x.1.as_str()) != Some("OR") {
            continue;
        }
        let (or_at, what) = (&w[k + 1].0, w.get(k + 2).map(|x| x.1.as_str()).unwrap_or(""));
        match what {
            "IGNORE" => {
                let end = w[k + 2].0 + "IGNORE".len();
                edits.push((*or_at, end, ""));
                // ON CONFLICT DO NOTHING goes at the end of this statement, before any RETURNING.
                let stmt_end = sql[*at..].find(';').map(|i| at + i).unwrap_or(sql.len());
                let returning = w.iter().skip(k).find(|(o, x)| *o < stmt_end && x == "RETURNING").map(|(o, _)| *o);
                let pos = returning.unwrap_or(stmt_end);
                edits.push((pos, pos, " ON CONFLICT DO NOTHING "));
            }
            _ => return Err(Error::Other(format!("INSERT OR {what} isn't portable; use INSERT … ON CONFLICT: {sql}"))),
        }
    }
    if edits.is_empty() {
        return Ok(sql.to_string());
    }
    edits.sort_by_key(|e| (e.0, e.1));
    let mut out = String::with_capacity(sql.len() + 32);
    let mut pos = 0;
    for (from, to, with) in edits {
        out.push_str(sql[pos..from].trim_end_matches(' '));
        out.push_str(with);
        pos = to;
    }
    out.push_str(&sql[pos..]);
    Ok(out)
}

/// Translate a SQLite schema (`CREATE TABLE`/`CREATE INDEX` statements, PRAGMAs ignored) for
/// Postgres. Returns the statements one by one.
pub fn pg_ddl(ddl: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for stmt in split_statements(ddl) {
        let upper = stmt.trim_start().to_ascii_uppercase();
        if upper.starts_with("PRAGMA") || upper.is_empty() {
            continue;
        }
        let mut s = String::with_capacity(stmt.len() + 64);
        let is_table = upper.starts_with("CREATE TABLE");
        let mut without_rowid = false;
        let mut has_int_pk = false;
        // Whether the last token was a name (or a quoted one, or a literal): only a word that
        // follows one is in a type's place, so a column *named* `text` stays a name.
        let mut after_word = false;
        for (is_code, code) in spans(&stmt) {
            if !is_code {
                s.push_str(code);
                if !code.starts_with("--") && !code.starts_with("/*") {
                    after_word = true;
                }
                continue;
            }
            {
                let mut c = code.to_string();
                let up = c.to_ascii_uppercase();
                if let Some(i) = up.find("WITHOUT ROWID") {
                    without_rowid = true;
                    c.replace_range(i..i + "WITHOUT ROWID".len(), "");
                }
                let up = c.to_ascii_uppercase();
                if up.contains("INTEGER PRIMARY KEY") {
                    has_int_pk = true;
                }
                s.push_str(&retype(&c, &mut after_word));
            }
        }
        if is_table && !without_rowid && !has_int_pk {
            // A rowid table: give it the column SQLite has implicitly.
            let close = s.rfind(')').ok_or_else(|| Error::Other(format!("bad CREATE TABLE: {stmt}")))?;
            s.insert_str(close, ",\n    rowid BIGINT GENERATED ALWAYS AS IDENTITY");
        }
        out.push(pg_sql(s.trim())?);
    }
    Ok(out)
}

/// Statements of a script, split on `;` outside literals and comments (comments dropped).
pub fn split_statements(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for (is_code, code) in spans(script) {
        if !is_code {
            if !code.starts_with("--") && !code.starts_with("/*") {
                cur.push_str(code);
            }
            continue;
        }
        {
            let mut rest = code;
            while let Some(i) = rest.find(';') {
                cur.push_str(&rest[..i]);
                let s = std::mem::take(&mut cur);
                if !s.trim().is_empty() {
                    out.push(s.trim().to_string());
                }
                rest = &rest[i + 1..];
            }
            cur.push_str(rest);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Column types, word by word. `INTEGER PRIMARY KEY AUTOINCREMENT` becomes an identity column.
/// A type word only counts where a type goes, right after a name (`after_word`, carried from
/// span to span): `text TEXT` is a column named `text` of type `TEXT`.
fn retype(code: &str, after_word: &mut bool) -> String {
    const AUTO: &str = "INTEGER PRIMARY KEY AUTOINCREMENT";
    const MARK: &str = "\u{1}";
    let mut code = code.to_string();
    while let Some(i) = code.to_ascii_uppercase().find(AUTO) {
        code.replace_range(i..i + AUTO.len(), MARK);
    }
    let mut out = String::with_capacity(code.len() + 16);
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String, after_word: &mut bool| {
        if !word.is_empty() {
            let typed = match word.to_ascii_uppercase().as_str() {
                _ if !*after_word => None,
                "INTEGER" => Some("BIGINT"),
                "REAL" => Some("DOUBLE PRECISION"),
                "BLOB" => Some("BYTEA"),
                "TEXT" => Some("TEXT COLLATE \"C\""),
                _ => None,
            };
            out.push_str(typed.unwrap_or(word.as_str()));
            word.clear();
            *after_word = true;
        }
    };
    for ch in code.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            word.push(ch);
        } else {
            flush(&mut word, &mut out, after_word);
            if !ch.is_whitespace() {
                *after_word = false;
            }
            out.push(ch);
        }
    }
    flush(&mut word, &mut out, after_word);
    out.replace(MARK, "BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders() {
        assert_eq!(pg_sql("SELECT a FROM t WHERE x = ? AND y = ?").unwrap(), "SELECT a FROM t WHERE x = $1 AND y = $2");
        assert_eq!(pg_sql("UPDATE t SET a = ?1 WHERE b < ?1 AND (?2 IS NULL OR c = ?2)").unwrap(), "UPDATE t SET a = $1 WHERE b < $1 AND ($2 IS NULL OR c = $2)");
        // A bare ? after ?3 is ?4, as in SQLite.
        assert_eq!(pg_sql("SELECT ?3, ?").unwrap(), "SELECT $3, $4");
        assert_eq!(pg_sql("SELECT '?', \"a?\" -- ?\n, ?").unwrap(), "SELECT '?', \"a?\" -- ?\n, $1");
        assert_eq!(pg_sql("SELECT 'it''s ?', ?").unwrap(), "SELECT 'it''s ?', $1");
        assert!(pg_sql("SELECT ?0").is_err());
    }

    #[test]
    fn inserts() {
        assert_eq!(pg_sql("INSERT OR IGNORE INTO t (a) VALUES (?)").unwrap(), "INSERT INTO t (a) VALUES ($1) ON CONFLICT DO NOTHING ");
        assert_eq!(
            pg_sql("insert or ignore into t (a) values (?) returning a").unwrap(),
            "insert into t (a) values ($1) ON CONFLICT DO NOTHING returning a"
        );
        assert!(pg_sql("INSERT OR REPLACE INTO t (a) VALUES (?)").is_err());
        assert_eq!(pg_sql("SELECT 'INSERT OR REPLACE'").unwrap(), "SELECT 'INSERT OR REPLACE'");
    }

    #[test]
    fn ddl() {
        let s = pg_ddl(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS a (repo TEXT NOT NULL, id BLOB NOT NULL, n INTEGER NOT NULL DEFAULT 0, usd REAL, PRIMARY KEY (repo, id)) WITHOUT ROWID;
             -- a comment; with a semicolon
             CREATE TABLE IF NOT EXISTS b (seq INTEGER PRIMARY KEY AUTOINCREMENT, note TEXT DEFAULT 'TEXT; INTEGER');
             CREATE TABLE IF NOT EXISTS c (id TEXT PRIMARY KEY, at INTEGER);
             CREATE INDEX IF NOT EXISTS c_at ON c (at);",
        )
        .unwrap();
        assert_eq!(s.len(), 4, "{s:#?}");
        assert_eq!(
            s[0],
            "CREATE TABLE IF NOT EXISTS a (repo TEXT COLLATE \"C\" NOT NULL, id BYTEA NOT NULL, n BIGINT NOT NULL DEFAULT 0, usd DOUBLE PRECISION, PRIMARY KEY (repo, id))"
        );
        assert_eq!(
            s[1],
            "CREATE TABLE IF NOT EXISTS b (seq BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, note TEXT COLLATE \"C\" DEFAULT 'TEXT; INTEGER')"
        );
        assert_eq!(s[2], "CREATE TABLE IF NOT EXISTS c (id TEXT COLLATE \"C\" PRIMARY KEY, at BIGINT,\n    rowid BIGINT GENERATED ALWAYS AS IDENTITY)");
        assert_eq!(s[3], "CREATE INDEX IF NOT EXISTS c_at ON c (at)");
    }

    #[test]
    fn columns_named_like_types_stay_names() {
        let s = pg_ddl(
            "CREATE TABLE IF NOT EXISTS l (id TEXT PRIMARY KEY, text TEXT NOT NULL,
                 -- a comment between columns
                 \"full\" INTEGER NOT NULL DEFAULT 0, real REAL, blob BLOB, UNIQUE (text, real)) WITHOUT ROWID",
        )
        .unwrap();
        assert_eq!(
            s[0],
            "CREATE TABLE IF NOT EXISTS l (id TEXT COLLATE \"C\" PRIMARY KEY, text TEXT COLLATE \"C\" NOT NULL,\n                 \n                 \"full\" BIGINT NOT NULL DEFAULT 0, real DOUBLE PRECISION, blob BYTEA, UNIQUE (text, real))"
        );
    }
}
