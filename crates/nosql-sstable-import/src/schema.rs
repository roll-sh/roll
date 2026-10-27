//! A backup's CQL schema: statements split out of `schema.cql` files (and Medusa's / ScyllaDB
//! Manager's full schema), classified, and put in the order an import runs them: keyspaces,
//! types, functions, tables and their dropped columns before the data; indexes and
//! materialized views after it, so the engine builds them from the imported rows.

/// What a statement creates or changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StmtKind {
    Keyspace,
    Type,
    Function,
    Aggregate,
    Table,
    /// `ALTER TABLE … DROP … USING TIMESTAMP` (Cassandra's record of dropped columns) and
    /// other `ALTER`s.
    Alter,
    Index,
    View,
    Other,
}

impl StmtKind {
    /// Created after the data (derived from it).
    pub fn is_derived(&self) -> bool {
        matches!(self, StmtKind::Index | StmtKind::View)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stmt {
    pub kind: StmtKind,
    /// The keyspace and object name it targets (for indexes: the table's keyspace and the
    /// index name; for views: the view).
    pub keyspace: Option<String>,
    pub name: Option<String>,
    /// For indexes and views: the base table.
    pub base: Option<String>,
    pub text: String,
}

/// Split CQL text into statements (quotes, `$$` bodies and comments respected).
pub fn split(cql: &str) -> Vec<String> {
    let b = cql.as_bytes();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' | b'"' => {
                let start = i;
                i += 1;
                while i < b.len() {
                    if b[i] == c {
                        if i + 1 < b.len() && b[i + 1] == c {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i = (i + 1).min(b.len());
                cur.push_str(&cql[start..i]);
                continue;
            }
            b'$' if b.get(i + 1) == Some(&b'$') => {
                let start = i;
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'$' && b[i + 1] == b'$') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                cur.push_str(&cql[start..i]);
                continue;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                cur.push(' ');
                continue;
            }
            b';' => {
                let s = cur.trim();
                if !s.is_empty() {
                    out.push(s.to_string());
                }
                cur.clear();
                i += 1;
                continue;
            }
            _ => {}
        }
        let ch_len = cql[i..].chars().next().map_or(1, char::len_utf8);
        cur.push_str(&cql[i..i + ch_len]);
        i += ch_len;
    }
    let s = cur.trim();
    if !s.is_empty() {
        out.push(s.to_string());
    }
    out
}

/// Tokens: words, quoted identifiers (unquoted), punctuation.
fn tokens(s: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if c.is_whitespace() {
            continue;
        }
        if c == '"' {
            let mut v = String::new();
            while let Some((_, d)) = it.next() {
                if d == '"' {
                    if it.peek().map(|x| x.1) == Some('"') {
                        it.next();
                        v.push('"');
                        continue;
                    }
                    break;
                }
                v.push(d);
            }
            out.push((v, true));
        } else if c.is_alphanumeric() || c == '_' {
            let mut end = i + c.len_utf8();
            while let Some((j, d)) = it.peek().copied() {
                if d.is_alphanumeric() || d == '_' {
                    end = j + d.len_utf8();
                    it.next();
                } else {
                    break;
                }
            }
            out.push((s[i..end].to_string(), false));
        } else {
            out.push((c.to_string(), false));
        }
        if out.len() > 64 {
            break;
        }
    }
    out
}

fn ident((t, quoted): &(String, bool)) -> String {
    if *quoted { t.clone() } else { t.to_ascii_lowercase() }
}

/// `[ks.]name` at token `at`: (keyspace, name, next index).
fn qualified(t: &[(String, bool)], at: usize) -> (Option<String>, Option<String>, usize) {
    let Some(first) = t.get(at) else { return (None, None, at) };
    if t.get(at + 1).is_some_and(|x| x.0 == "." && !x.1) {
        (Some(ident(first)), t.get(at + 2).map(ident), at + 3)
    } else {
        (None, Some(ident(first)), at + 1)
    }
}

fn kw(t: &[(String, bool)], at: usize, words: &[&str]) -> bool {
    words.iter().enumerate().all(|(i, w)| t.get(at + i).is_some_and(|x| !x.1 && x.0.eq_ignore_ascii_case(w)))
}

/// Classify a statement.
pub fn classify(text: &str) -> Stmt {
    let t = tokens(text);
    let mut s = Stmt { kind: StmtKind::Other, keyspace: None, name: None, base: None, text: text.to_string() };
    let mut at = 1;
    if kw(&t, 0, &["CREATE"]) {
        if kw(&t, at, &["OR", "REPLACE"]) {
            at += 2;
        }
        let (kind, n) = if kw(&t, at, &["KEYSPACE"]) || kw(&t, at, &["SCHEMA"]) {
            (StmtKind::Keyspace, 1)
        } else if kw(&t, at, &["TYPE"]) {
            (StmtKind::Type, 1)
        } else if kw(&t, at, &["FUNCTION"]) {
            (StmtKind::Function, 1)
        } else if kw(&t, at, &["AGGREGATE"]) {
            (StmtKind::Aggregate, 1)
        } else if kw(&t, at, &["TABLE"]) || kw(&t, at, &["COLUMNFAMILY"]) {
            (StmtKind::Table, 1)
        } else if kw(&t, at, &["MATERIALIZED", "VIEW"]) {
            (StmtKind::View, 2)
        } else if kw(&t, at, &["CUSTOM", "INDEX"]) {
            (StmtKind::Index, 2)
        } else if kw(&t, at, &["INDEX"]) {
            (StmtKind::Index, 1)
        } else {
            return s;
        };
        s.kind = kind;
        at += n;
        if kw(&t, at, &["IF", "NOT", "EXISTS"]) {
            at += 3;
        }
        if kind == StmtKind::Keyspace {
            s.name = t.get(at).map(ident);
            s.keyspace = s.name.clone();
            return s;
        }
        if kind == StmtKind::Index {
            // CREATE INDEX [name] ON [ks.]table …
            let mut on = at;
            while on < t.len() && !kw(&t, on, &["ON"]) {
                on += 1;
            }
            s.name = (on > at).then(|| ident(&t[at]));
            let (ks, table, _) = qualified(&t, on + 1);
            s.keyspace = ks;
            s.base = table;
            return s;
        }
        let (ks, name, next) = qualified(&t, at);
        s.keyspace = ks;
        s.name = name;
        if kind == StmtKind::View {
            let mut from = next;
            while from < t.len() && !kw(&t, from, &["FROM"]) {
                from += 1;
            }
            s.base = qualified(&t, from + 1).1;
        }
        return s;
    }
    if kw(&t, 0, &["ALTER"]) && (kw(&t, 1, &["TABLE"]) || kw(&t, 1, &["COLUMNFAMILY"])) {
        s.kind = StmtKind::Alter;
        let (ks, name, _) = qualified(&t, 2);
        s.keyspace = ks;
        s.name = name;
    }
    s
}

/// Statements in import order (stable within a kind; duplicates removed).
pub fn ordered(stmts: Vec<Stmt>) -> Vec<Stmt> {
    let mut seen = std::collections::HashSet::new();
    let mut v: Vec<Stmt> = stmts.into_iter().filter(|s| seen.insert(normalize_ws(&s.text))).collect();
    v.sort_by_key(|s| s.kind);
    v
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A `CREATE TABLE` / `CREATE MATERIALIZED VIEW` with its options reduced to what defines the
/// data (`CLUSTERING ORDER BY`, `ID`, `COMPACT STORAGE`): for a source whose options the
/// target dialect refuses (ScyllaDB's `tombstone_gc` on a Cassandra database, …).
pub fn strip_options(text: &str) -> Option<String> {
    // The WITH after the column list: the first one at parenthesis depth 0 outside quotes.
    let b = text.as_bytes();
    let (mut depth, mut i, mut with) = (0i32, 0usize, None);
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0 && b[i..].len() >= 4 && b[i..i + 4].eq_ignore_ascii_case(b"WITH") && i > 0 && b[i - 1].is_ascii_whitespace() && b.get(i + 4).is_none_or(|c| c.is_ascii_whitespace()) => {
                with = Some(i);
                break;
            }
            _ => {}
        }
        i += 1;
    }
    let at = with?;
    let head = text[..at].trim_end();
    let opts = &text[at + 4..];
    let mut keep = Vec::new();
    for part in split_and(opts) {
        let p = part.trim();
        let u = p.to_ascii_uppercase();
        if u.starts_with("CLUSTERING ORDER") || u.starts_with("ID ") || u.starts_with("ID=") || u == "COMPACT STORAGE" {
            keep.push(p.to_string());
        }
    }
    Some(if keep.is_empty() { head.to_string() } else { format!("{head} WITH {}", keep.join(" AND ")) })
}

/// Split table options on top-level `AND`.
fn split_and(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let (mut depth, mut i, mut start, mut out) = (0i32, 0usize, 0usize, Vec::new());
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
            }
            b'(' | b'{' | b'[' => depth += 1,
            b')' | b'}' | b']' => depth -= 1,
            _ if depth == 0 && b[i..].len() >= 3 && b[i..i + 3].eq_ignore_ascii_case(b"AND") && i > 0 && b[i - 1].is_ascii_whitespace() && b.get(i + 3).is_some_and(|c| c.is_ascii_whitespace()) => {
                out.push(s[start..i].to_string());
                start = i + 3;
                i += 3;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(s[start..].to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASSANDRA: &str = "CREATE TYPE IF NOT EXISTS imp_fx.addr (\n    street text,\n    zip int\n);\nCREATE TABLE IF NOT EXISTS imp_fx.colls (\n    k int PRIMARY KEY,\n    l list<int>\n) WITH ID = d3d16030-cd09-11f1-8c36-33d044eb584e\n    AND comment = 'a;b'\n    AND compaction = {'class': 'SizeTieredCompactionStrategy', 'max_threshold': '32'};\nALTER TABLE imp_fx.colls DROP l USING TIMESTAMP 1792557174924000;\n-- a comment; with a semicolon\nCREATE INDEX i ON imp_fx.colls (l);";

    #[test]
    fn splits_and_classifies() {
        let s = split(CASSANDRA);
        assert_eq!(s.len(), 4, "{s:#?}");
        let c: Vec<Stmt> = s.iter().map(|x| classify(x)).collect();
        assert_eq!(c.iter().map(|x| x.kind).collect::<Vec<_>>(), vec![StmtKind::Type, StmtKind::Table, StmtKind::Alter, StmtKind::Index]);
        assert_eq!((c[1].keyspace.as_deref(), c[1].name.as_deref()), (Some("imp_fx"), Some("colls")));
        assert_eq!((c[3].name.as_deref(), c[3].base.as_deref()), (Some("i"), Some("colls")));
        let v = classify("CREATE MATERIALIZED VIEW \"Ks\".v AS SELECT * FROM \"Ks\".\"Base\" WHERE k IS NOT NULL PRIMARY KEY (k)");
        assert_eq!((v.kind, v.keyspace.as_deref(), v.name.as_deref(), v.base.as_deref()), (StmtKind::View, Some("Ks"), Some("v"), Some("Base")));
        let k = classify("CREATE KEYSPACE ks WITH replication = {}");
        assert_eq!((k.kind, k.name.as_deref()), (StmtKind::Keyspace, Some("ks")));
        let o = ordered(c.into_iter().rev().chain([k]).collect());
        assert_eq!(o.first().unwrap().kind, StmtKind::Keyspace);
        assert_eq!(o.last().unwrap().kind, StmtKind::Index);
        assert_eq!(split("CREATE FUNCTION f() RETURNS int LANGUAGE lua AS $$ return 1; $$; X").len(), 2);
    }

    #[test]
    fn strips_options() {
        let t = "CREATE TABLE ks.t (k int, c int, PRIMARY KEY (k, c)) WITH CLUSTERING ORDER BY (c DESC)\n    AND tombstone_gc = {'mode': 'repair'}\n    AND comment = 'x AND y'";
        assert_eq!(strip_options(t).unwrap(), "CREATE TABLE ks.t (k int, c int, PRIMARY KEY (k, c)) WITH CLUSTERING ORDER BY (c DESC)");
        let t = "CREATE TABLE ks.t (k int PRIMARY KEY) WITH ID = 1234 AND compaction = {'class': 'IncrementalCompactionStrategy'}";
        assert_eq!(strip_options(t).unwrap(), "CREATE TABLE ks.t (k int PRIMARY KEY) WITH ID = 1234");
        assert_eq!(strip_options("CREATE TABLE ks.t (k int PRIMARY KEY) WITH comment = 'with'").unwrap(), "CREATE TABLE ks.t (k int PRIMARY KEY)");
        assert!(strip_options("CREATE TABLE ks.t (k int PRIMARY KEY)").is_none());
    }
}
