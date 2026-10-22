//! roll code's codebase index: derived data per repo, following its default branch. For every
//! indexed file it keeps the symbols the file defines, the identifiers it references (with a
//! few line numbers each) and its trigrams, so roll can answer:
//!
//! - code search (`search`): literal or regex, narrowed by trigrams, confirmed on the content;
//! - `definitions` / `references` / `callers_of` a symbol;
//! - `related_tests` of a set of paths;
//! - `similar` code to a snippet (trigram Jaccard).
//!
//! The index lives in `<dir>/<repo>.db` (SQLite, WAL) and is updated incrementally: the caller
//! hands [`RepoIndex::apply`] the files a landing changed (new content, or `None` when
//! deleted) and the head they're at. Only the first build reads the whole tree, one file at a
//! time. Contents aren't stored: search confirms candidates through a [`Source`].
//!
//! Symbols are extracted with per-language patterns (Rust, TypeScript/JavaScript, Python, Go,
//! Java/Kotlin, C/C++, Ruby, PHP), not parsers: cheap, approximate, good enough for review
//! context and code navigation.

pub mod extract;

pub use extract::{FileFacts, SymbolDef, is_test_path, language};

use anyhow::Result;
use async_trait::async_trait;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Files bigger than this aren't indexed (they're usually generated or data).
pub const MAX_FILE_BYTES: usize = 1 << 20;
/// Distinct trigrams kept per file.
pub const MAX_TRIGRAMS_PER_FILE: usize = 40_000;
/// Files a search without a usable literal scans at most.
pub const MAX_SCAN: usize = 20_000;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    lang TEXT,
    size INTEGER NOT NULL,
    test INTEGER NOT NULL,
    tri_count INTEGER NOT NULL,
    -- The file's trigrams (u32 little-endian), to delete its postings.
    tris BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS symbols (name TEXT NOT NULL, file INTEGER NOT NULL, line INTEGER NOT NULL, kind TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS symbols_name ON symbols (name);
CREATE INDEX IF NOT EXISTS symbols_file ON symbols (file);
CREATE TABLE IF NOT EXISTS refs (name TEXT NOT NULL, file INTEGER NOT NULL, count INTEGER NOT NULL, lines TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS refs_name ON refs (name);
CREATE INDEX IF NOT EXISTS refs_file ON refs (file);
CREATE TABLE IF NOT EXISTS tri (t INTEGER NOT NULL, file INTEGER NOT NULL, PRIMARY KEY (t, file)) WITHOUT ROWID;
"#;

/// Reads files at the index's head (for search to confirm candidates).
#[async_trait]
pub trait Source: Send + Sync {
    async fn read(&self, path: &str) -> Option<Vec<u8>>;
}

/// A changed file: its new content, or `None` when it was deleted.
#[derive(Clone, Debug)]
pub struct Update {
    pub path: String,
    pub content: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub path: String,
    pub line: u64,
    pub kind: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RefHit {
    pub path: String,
    pub count: u64,
    /// The first few lines it appears on.
    pub lines: Vec<u64>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Match {
    pub path: String,
    pub line: u64,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct SearchResult {
    pub matches: Vec<Match>,
    pub files_scanned: u64,
    /// More matches (or files) than the limits allowed.
    pub truncated: bool,
    /// The index's head the search ran at.
    pub head: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Query {
    pub text: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub limit: usize,
}

#[derive(Clone, Copy, Debug, Serialize, Default)]
pub struct Stats {
    pub files: u64,
    pub symbols: u64,
    pub refs: u64,
    pub trigrams: u64,
}

/// Every repo's index under one directory.
pub struct Index {
    dir: PathBuf,
    repos: Mutex<HashMap<String, Arc<RepoIndex>>>,
}

impl Index {
    pub fn open(dir: &Path) -> Result<Index> {
        std::fs::create_dir_all(dir)?;
        Ok(Index { dir: dir.to_path_buf(), repos: Mutex::new(HashMap::new()) })
    }

    pub fn repo(&self, repo: &str) -> Result<Arc<RepoIndex>> {
        anyhow::ensure!(!repo.is_empty() && repo.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'), "bad repo id");
        let mut m = self.repos.lock();
        if let Some(r) = m.get(repo) {
            return Ok(r.clone());
        }
        let r = Arc::new(RepoIndex::open(&self.dir.join(format!("{repo}.db")))?);
        m.insert(repo.to_string(), r.clone());
        Ok(r)
    }

    /// Forget a repo's index (it was deleted).
    pub fn remove(&self, repo: &str) {
        self.repos.lock().remove(repo);
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(self.dir.join(format!("{repo}.{ext}")));
        }
    }
}

pub struct RepoIndex {
    conn: Mutex<Connection>,
}

fn trigram(a: u8, b: u8, c: u8) -> u32 {
    (a as u32) << 16 | (b as u32) << 8 | c as u32
}

/// The distinct trigrams of `text`, lowercased (ASCII), at most `max`.
pub fn trigrams(text: &[u8], max: usize) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    for w in text.windows(3) {
        if w.contains(&b'\n') {
            continue;
        }
        out.insert(trigram(w[0].to_ascii_lowercase(), w[1].to_ascii_lowercase(), w[2].to_ascii_lowercase()));
        if out.len() >= max {
            break;
        }
    }
    out
}

/// Should a file be indexed: text, not too big, not minified, not vendored.
pub fn indexable(path: &str, data: &[u8]) -> bool {
    if data.len() > MAX_FILE_BYTES || data[..data.len().min(8000)].contains(&0) {
        return false;
    }
    let p = path.to_ascii_lowercase();
    const SKIP: [&str; 8] = ["node_modules/", "vendor/", "/dist/", "target/", ".min.js", ".min.css", ".lock", "-lock.json"];
    if SKIP.iter().any(|s| p.contains(s) || p.starts_with(s.trim_start_matches('/'))) {
        return false;
    }
    let lines = data.iter().filter(|b| **b == b'\n').count().max(1);
    data.len() / lines < 1000
}

impl RepoIndex {
    fn open(path: &Path) -> Result<RepoIndex> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(RepoIndex { conn: Mutex::new(conn) })
    }

    /// The head the index is at (`None`: never built).
    pub fn head(&self) -> Result<Option<String>> {
        Ok(self.conn.lock().query_row("SELECT value FROM meta WHERE key = 'head'", [], |r| r.get(0)).optional()?)
    }

    /// Drop everything (before a rebuild).
    pub fn clear(&self) -> Result<()> {
        self.conn.lock().execute_batch("DELETE FROM files; DELETE FROM symbols; DELETE FROM refs; DELETE FROM tri; DELETE FROM meta;")?;
        Ok(())
    }

    /// Apply changed files and move the index to `head`. `head: None` keeps it where it is
    /// (a rebuild in batches sets the head with its last batch).
    pub fn apply(&self, head: Option<&str>, updates: &[Update]) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        for u in updates {
            remove_file(&tx, &u.path)?;
            let Some(data) = &u.content else { continue };
            if !indexable(&u.path, data) {
                continue;
            }
            let text = String::from_utf8_lossy(data);
            let facts = extract::extract(&u.path, &text);
            let tris = trigrams(data, MAX_TRIGRAMS_PER_FILE);
            let blob: Vec<u8> = tris.iter().flat_map(|t| t.to_le_bytes()).collect();
            tx.execute(
                "INSERT INTO files (path, lang, size, test, tri_count, tris) VALUES (?, ?, ?, ?, ?, ?)",
                params![u.path, facts.lang, data.len() as i64, is_test_path(&u.path) as i64, tris.len() as i64, blob],
            )?;
            let id = tx.last_insert_rowid();
            {
                let mut st = tx.prepare_cached("INSERT OR IGNORE INTO tri (t, file) VALUES (?, ?)")?;
                for t in &tris {
                    st.execute(params![*t as i64, id])?;
                }
                let mut st = tx.prepare_cached("INSERT INTO symbols (name, file, line, kind) VALUES (?, ?, ?, ?)")?;
                for s in &facts.symbols {
                    st.execute(params![s.name, id, s.line as i64, s.kind])?;
                }
                let mut st = tx.prepare_cached("INSERT INTO refs (name, file, count, lines) VALUES (?, ?, ?, ?)")?;
                for (name, (count, lines)) in &facts.refs {
                    let l: Vec<String> = lines.iter().map(|x| x.to_string()).collect();
                    st.execute(params![name, id, *count as i64, l.join(",")])?;
                }
            }
        }
        if let Some(h) = head {
            tx.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('head', ?)", [h])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn stats(&self) -> Result<Stats> {
        let c = self.conn.lock();
        let n = |t: &str| c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get::<_, i64>(0)).map(|x| x as u64);
        Ok(Stats { files: n("files")?, symbols: n("symbols")?, refs: n("refs")?, trigrams: n("tri")? })
    }

    pub fn paths(&self) -> Result<Vec<String>> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT path FROM files ORDER BY path")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn definitions(&self, name: &str, limit: usize) -> Result<Vec<Symbol>> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT s.name, f.path, s.line, s.kind FROM symbols s JOIN files f ON f.id = s.file WHERE s.name = ? ORDER BY f.path, s.line LIMIT ?")?;
        let rows = st.query_map(params![name, limit as i64], |r| Ok(Symbol { name: r.get(0)?, path: r.get(1)?, line: r.get::<_, i64>(2)? as u64, kind: r.get(3)? }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Files that use `name`, most uses first.
    pub fn references(&self, name: &str, limit: usize) -> Result<Vec<RefHit>> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT f.path, r.count, r.lines FROM refs r JOIN files f ON f.id = r.file WHERE r.name = ? ORDER BY r.count DESC, f.path LIMIT ?")?;
        let rows = st.query_map(params![name, limit as i64], |r| {
            let lines: String = r.get(2)?;
            Ok(RefHit { path: r.get(0)?, count: r.get::<_, i64>(1)? as u64, lines: lines.split(',').filter_map(|x| x.parse().ok()).collect() })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// How many files use `name` (fan-out, for `auto`).
    pub fn reference_count(&self, name: &str) -> Result<u64> {
        Ok(self.conn.lock().query_row("SELECT COUNT(*) FROM refs WHERE name = ?", [name], |r| r.get::<_, i64>(0))? as u64)
    }

    /// For each name, the files using it outside `exclude` (the changed files themselves).
    pub fn callers_of(&self, names: &[String], exclude: &[String], per_name: usize) -> Result<Vec<(String, Vec<RefHit>)>> {
        let mut out = Vec::new();
        for n in names {
            let hits: Vec<RefHit> = self.references(n, per_name + exclude.len())?.into_iter().filter(|h| !exclude.contains(&h.path)).take(per_name).collect();
            if !hits.is_empty() {
                out.push((n.clone(), hits));
            }
        }
        Ok(out)
    }

    /// Test files that use any of `symbols` or are named after one of `paths`
    /// (`foo.rs` → `foo_test.rs`, `test_foo.py`, `foo.test.ts`, `tests/foo.rs`, …).
    pub fn related_tests(&self, paths: &[String], symbols: &[String], limit: usize) -> Result<Vec<String>> {
        let stems: BTreeSet<String> = paths.iter().filter_map(|p| stem(p)).filter(|s| s.len() >= 3).collect();
        let c = self.conn.lock();
        let mut found: BTreeMap<String, u64> = BTreeMap::new();
        {
            let mut st = c.prepare("SELECT f.path, r.count FROM refs r JOIN files f ON f.id = r.file WHERE r.name = ? AND f.test = 1 LIMIT 50")?;
            for s in symbols {
                let rows = st.query_map([s], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?;
                for row in rows {
                    let (p, n) = row?;
                    *found.entry(p).or_default() += n;
                }
            }
        }
        let mut st = c.prepare("SELECT path FROM files WHERE test = 1")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        for p in rows {
            let p = p?;
            if let Some(s) = test_stem(&p)
                && stems.contains(&s)
            {
                *found.entry(p).or_default() += 100;
            }
        }
        let mut v: Vec<(String, u64)> = found.into_iter().filter(|(p, _)| !paths.contains(p)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(v.into_iter().take(limit).map(|(p, _)| p).collect())
    }

    /// Files most like `snippet` (Jaccard similarity of trigram sets), outside `exclude`.
    pub fn similar(&self, snippet: &str, exclude: &[String], limit: usize) -> Result<Vec<(String, f64)>> {
        let tris = trigrams(snippet.as_bytes(), 400);
        if tris.len() < 8 {
            return Ok(Vec::new());
        }
        let list: Vec<String> = tris.iter().map(|t| t.to_string()).collect();
        let c = self.conn.lock();
        let sql = format!(
            "SELECT f.path, f.tri_count, COUNT(*) AS hits FROM tri JOIN files f ON f.id = tri.file WHERE tri.t IN ({}) GROUP BY tri.file ORDER BY hits DESC LIMIT 100",
            list.join(",")
        );
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as f64, r.get::<_, i64>(2)? as f64)))?;
        let n = tris.len() as f64;
        let mut out: Vec<(String, f64)> = Vec::new();
        for row in rows {
            let (p, total, hits) = row?;
            if exclude.contains(&p) {
                continue;
            }
            out.push((p, hits / (n + total - hits).max(1.0)));
        }
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        out.truncate(limit);
        Ok(out)
    }

    /// Files that may contain the query: those with all of its literal's trigrams, or every
    /// file (up to [`MAX_SCAN`]) when it has no usable literal. `(paths, truncated)`.
    pub fn candidates(&self, q: &Query) -> Result<(Vec<String>, bool)> {
        let literals = if q.regex { regex_literals(&q.text) } else { vec![q.text.clone()] };
        let mut tris: BTreeSet<u32> = BTreeSet::new();
        for l in literals.iter().filter(|l| l.len() >= 3) {
            tris.extend(trigrams(l.as_bytes(), 10_000));
        }
        let c = self.conn.lock();
        if tris.is_empty() {
            let mut st = c.prepare("SELECT path FROM files ORDER BY path LIMIT ?")?;
            let rows: Vec<String> = st.query_map([MAX_SCAN as i64 + 1], |r| r.get(0))?.collect::<Result<_, _>>()?;
            let truncated = rows.len() > MAX_SCAN;
            return Ok((rows.into_iter().take(MAX_SCAN).collect(), truncated));
        }
        // At most 16 trigrams, spread over the literal.
        let all: Vec<u32> = tris.into_iter().collect();
        let step = all.len().div_ceil(16).max(1);
        let pick: Vec<u32> = all.iter().step_by(step).copied().collect();
        let sql = pick.iter().map(|t| format!("SELECT file FROM tri WHERE t = {t}")).collect::<Vec<_>>().join(" INTERSECT ");
        let sql = format!("SELECT f.path FROM files f WHERE f.id IN ({sql}) ORDER BY f.path");
        let mut st = c.prepare(&sql)?;
        let rows: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        Ok((rows, false))
    }

    /// Search the indexed files. `path_ok` narrows the files (a glob, typically).
    pub async fn search(&self, q: &Query, source: &dyn Source, path_ok: &(dyn Fn(&str) -> bool + Send + Sync)) -> Result<SearchResult> {
        anyhow::ensure!(!q.text.is_empty(), "empty query");
        let matcher = Matcher::new(q)?;
        let (paths, truncated) = self.candidates(q)?;
        let mut out = SearchResult { head: self.head()?, ..Default::default() };
        let paths: Vec<String> = paths.into_iter().filter(|p| path_ok(p)).collect();
        if !matcher.scan(&paths, source, q.limit, &mut out).await {
            out.truncated |= truncated;
        }
        Ok(out)
    }
}

/// Matches lines for a [`Query`].
pub struct Matcher {
    re: Option<regex::Regex>,
    needle: String,
    case_sensitive: bool,
}

impl Matcher {
    pub fn new(q: &Query) -> Result<Matcher> {
        let re = if q.regex {
            Some(regex::RegexBuilder::new(&q.text).case_insensitive(!q.case_sensitive).size_limit(1 << 20).build().map_err(|e| anyhow::anyhow!("bad regex: {e}"))?)
        } else {
            None
        };
        let needle = if q.case_sensitive { q.text.clone() } else { q.text.to_lowercase() };
        Ok(Matcher { re, needle, case_sensitive: q.case_sensitive })
    }

    pub fn is_match(&self, line: &str) -> bool {
        match &self.re {
            Some(re) => re.is_match(line),
            None if self.case_sensitive => line.contains(&self.needle),
            None => line.to_lowercase().contains(&self.needle),
        }
    }

    /// Scan `paths` (read through `source`) into `out`, up to `limit` matches in all. Returns
    /// whether it stopped at the limit (`out.truncated` is then set).
    pub async fn scan(&self, paths: &[String], source: &dyn Source, limit: usize, out: &mut SearchResult) -> bool {
        let limit = limit.clamp(1, 1000);
        for p in paths {
            let Some(data) = source.read(p).await else { continue };
            out.files_scanned += 1;
            let text = String::from_utf8_lossy(&data);
            for (i, line) in text.lines().enumerate() {
                if self.is_match(line) {
                    if out.matches.len() >= limit {
                        out.truncated = true;
                        return true;
                    }
                    out.matches.push(Match { path: p.clone(), line: i as u64 + 1, text: clip(line, 300) });
                }
            }
        }
        false
    }
}

fn remove_file(tx: &rusqlite::Transaction, path: &str) -> Result<()> {
    let row: Option<(i64, Vec<u8>)> = tx.query_row("SELECT id, tris FROM files WHERE path = ?", [path], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    let Some((id, blob)) = row else { return Ok(()) };
    {
        let mut st = tx.prepare_cached("DELETE FROM tri WHERE t = ? AND file = ?")?;
        for c in blob.chunks_exact(4) {
            let t = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            st.execute(params![t as i64, id])?;
        }
    }
    tx.execute("DELETE FROM symbols WHERE file = ?", [id])?;
    tx.execute("DELETE FROM refs WHERE file = ?", [id])?;
    tx.execute("DELETE FROM files WHERE id = ?", [id])?;
    Ok(())
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    format!("{}…", &s[..i])
}

/// `src/foo.rs` → `foo`.
fn stem(p: &str) -> Option<String> {
    let name = p.rsplit('/').next()?;
    Some(name.split('.').next()?.to_ascii_lowercase())
}

/// The name a test file is about: `foo_test.go`, `test_foo.py`, `foo.test.ts`, `foo.spec.js`,
/// `FooTest.java`, `foo_spec.rb`, `tests/foo.rs` → `foo`.
fn test_stem(p: &str) -> Option<String> {
    let name = p.rsplit('/').next()?;
    let base = name.split('.').next()?.to_string();
    let lower = base.to_ascii_lowercase();
    for (pre, suf) in [("", "_test"), ("test_", ""), ("", "_spec"), ("", "test"), ("", "tests")] {
        if !suf.is_empty()
            && let Some(s) = lower.strip_suffix(suf)
        {
            return Some(s.trim_end_matches('_').to_string());
        }
        if !pre.is_empty()
            && let Some(s) = lower.strip_prefix(pre)
        {
            return Some(s.to_string());
        }
    }
    // foo.test.ts / foo.spec.ts / tests/foo.rs
    Some(lower)
}

/// Literal runs a regex certainly contains (outside groups, classes and alternations).
pub fn regex_literals(pattern: &str) -> Vec<String> {
    if pattern.contains('|') {
        return Vec::new();
    }
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut depth, mut in_class) = (0usize, false);
    let mut i = 0;
    let flush = |cur: &mut String, out: &mut Vec<String>, next: Option<char>| {
        // A quantifier that allows zero makes the last char optional.
        if matches!(next, Some('?') | Some('*') | Some('{')) {
            cur.pop();
        }
        if cur.len() >= 3 {
            out.push(std::mem::take(cur));
        } else {
            cur.clear();
        }
    };
    while i < chars.len() {
        let ch = chars[i];
        if in_class {
            if ch == '\\' {
                i += 1;
            } else if ch == ']' {
                in_class = false;
            }
            i += 1;
            continue;
        }
        match ch {
            '[' => {
                flush(&mut cur, &mut out, None);
                in_class = true;
            }
            '(' => {
                flush(&mut cur, &mut out, None);
                depth += 1;
            }
            ')' => {
                cur.clear();
                depth = depth.saturating_sub(1);
            }
            '\\' => {
                let next = chars.get(i + 1).copied();
                match next {
                    Some(c) if c.is_ascii_punctuation() && depth == 0 => {
                        let after = chars.get(i + 2).copied();
                        cur.push(c);
                        if matches!(after, Some('?') | Some('*') | Some('{') | Some('+')) {
                            flush(&mut cur, &mut out, after);
                        }
                    }
                    _ => flush(&mut cur, &mut out, None),
                }
                i += 2;
                continue;
            }
            '.' | '^' | '$' | '*' | '+' | '?' | '{' | '}' => flush(&mut cur, &mut out, None),
            c if depth == 0 => {
                let next = chars.get(i + 1).copied();
                cur.push(c);
                if matches!(next, Some('?') | Some('*') | Some('{') | Some('+')) {
                    flush(&mut cur, &mut out, next);
                }
            }
            _ => {}
        }
        i += 1;
    }
    flush(&mut cur, &mut out, None);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_of_regexes() {
        assert_eq!(regex_literals(r"fn\s+parse_\w+"), vec!["parse_"]);
        assert_eq!(regex_literals(r"foo|bar"), Vec::<String>::new());
        assert_eq!(regex_literals(r"abcd?ef"), vec!["abc"]);
        assert_eq!(regex_literals(r"(optional)?required"), vec!["required"]);
        assert_eq!(regex_literals(r"[abc]def"), vec!["def"]);
        assert_eq!(regex_literals(r"a\.b\.c"), vec!["a.b.c"]);
        assert_eq!(regex_literals(r"x+yz"), Vec::<String>::new());
        assert_eq!(test_stem("src/foo_test.go").as_deref(), Some("foo"));
        assert_eq!(test_stem("tests/test_foo.py").as_deref(), Some("foo"));
        assert_eq!(test_stem("web/foo.test.ts").as_deref(), Some("foo"));
        assert_eq!(test_stem("FooTest.java").as_deref(), Some("foo"));
        assert_eq!(stem("a/b/Foo.rs").as_deref(), Some("foo"));
        assert!(indexable("src/a.rs", b"fn main() {}\n"));
        assert!(!indexable("src/a.bin", b"\0\0\0"));
        assert!(!indexable("node_modules/x/a.js", b"x"));
        assert!(!indexable("Cargo.lock", b"x"));
        assert!(!indexable("a.min.js", b"x"));
        assert!(!indexable("a.js", &vec![b'x'; 5000]));
    }
}
