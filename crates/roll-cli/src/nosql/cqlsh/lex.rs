//! cqlsh's lexer and statement splitter.
//!
//! Ported from Apache Cassandra's `pylib/cqlshlib` (Apache-2.0): the terminal rules at the top of
//! `cql3handling.py` (tried in order, first match wins, as `re.Scanner` does),
//! `CqlParsingRuleSet.cql_massage_tokens` (cqlsh commands that end at a newline) and
//! `cql_split_statements` (batches are joined into one statement).

/// Token types, named as in `cql3handling.py`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Endline,
    QuotedString,
    PgString,
    QuotedName,
    UnclosedPgString,
    UnclosedString,
    UnclosedName,
    UnclosedComment,
    Float,
    Uuid,
    Blob,
    WholeNumber,
    Identifier,
    Colon,
    Star,
    Endtoken,
    Op,
    Cmp,
    Brackets,
    Qmark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: Kind,
    /// Byte offsets into the source.
    pub start: usize,
    pub end: usize,
    /// One of CQL's reserved keywords (`reserved_identifier`).
    pub reserved: bool,
}

impl Token {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start..self.end]
    }
}

/// `pylexotron.LexingError`: 1-based line and character (in code points).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LexError {
    pub line: usize,
    pub char: usize,
}

/// `cqlhandling.cql_keywords_reserved`.
pub const RESERVED: &[&str] = &[
    "add", "allow", "alter", "and", "apply", "asc", "authorize", "batch", "begin", "by", "columnfamily", "create", "delete",
    "desc", "describe", "drop", "entries", "execute", "from", "full", "grant", "if", "in", "index", "infinity", "insert",
    "into", "is", "keyspace", "limit", "materialized", "modify", "nan", "norecursive", "not", "null", "of", "on", "or",
    "order", "primary", "rename", "revoke", "schema", "select", "set", "table", "to", "token", "truncate", "unlogged",
    "update", "use", "using", "view", "where", "with",
];

/// `cqlshhandling.my_commands_ending_with_newline`.
pub const ENDS_WITH_NEWLINE: &[&str] = &[
    "help", "?", "consistency", "serial", "describe", "desc", "show", "source", "capture", "login", "debug", "tracing",
    "expand", "paging", "exit", "quit", "clear", "cls", "history",
];

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// A quoted literal (`'…'` or `"…"`, the quote doubled inside), with the regex's backtracking:
/// the longest prefix that closes, or `None` when nothing closes.
fn quoted(s: &[u8], i: usize, q: u8) -> Option<usize> {
    let mut j = i + 1;
    let mut last = None;
    while j < s.len() {
        if s[j] == q {
            if s.get(j + 1) == Some(&q) {
                last = Some(j + 1);
                j += 2;
                continue;
            }
            return Some(j + 1);
        }
        j += 1;
    }
    last
}

fn find(s: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    s.get(from..)?.windows(pat.len()).position(|w| w == pat).map(|p| p + from)
}

fn digits(s: &[u8], i: usize) -> usize {
    s[i..].iter().take_while(|b| b.is_ascii_digit()).count()
}

fn hex_run(s: &[u8], i: usize, n: usize) -> bool {
    s.len() >= i + n && s[i..i + n].iter().all(|b| b.is_ascii_hexdigit())
}

/// Lexes `src`. JUNK (whitespace and comments) produces no tokens.
pub fn lex(src: &str) -> Result<Vec<Token>, LexError> {
    let s = src.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        let tok = |kind, end| Token { kind, start: i, end, reserved: false };
        // endline
        if c == b'\n' {
            out.push(tok(Kind::Endline, i + 1));
            i += 1;
            continue;
        }
        // JUNK
        if matches!(c, b' ' | b'\t' | b'\r' | 0x0c | 0x0b) {
            i += s[i..].iter().take_while(|b| matches!(b, b' ' | b'\t' | b'\r' | 0x0c | 0x0b)).count();
            continue;
        }
        if s[i..].starts_with(b"--") || s[i..].starts_with(b"//") {
            let rest = s[i..].iter().position(|b| *b == b'\n' || *b == b'\r');
            i = match rest {
                Some(p) => i + p + 1,
                None => s.len(),
            };
            continue;
        }
        if s[i..].starts_with(b"/*")
            && let Some(p) = find(s, i + 2, b"*/")
        {
            i = p + 2;
            continue;
        }
        let t = match c {
            b'\'' => match quoted(s, i, b'\'') {
                Some(e) => tok(Kind::QuotedString, e),
                None => tok(Kind::UnclosedString, s.len()),
            },
            b'"' => match quoted(s, i, b'"') {
                Some(e) => tok(Kind::QuotedName, e),
                None => tok(Kind::UnclosedName, s.len()),
            },
            b'$' if s[i..].starts_with(b"$$") => match find(s, i + 2, b"$$") {
                Some(p) => tok(Kind::PgString, p + 2),
                None => tok(Kind::UnclosedPgString, s.len()),
            },
            b'/' if s[i..].starts_with(b"/*") => tok(Kind::UnclosedComment, s.len()),
            _ => {
                // float: -?[0-9]+\.[0-9]+
                let neg = usize::from(c == b'-');
                let d = if i + neg < s.len() { digits(s, i + neg) } else { 0 };
                let float = d > 0
                    && s.get(i + neg + d) == Some(&b'.')
                    && i + neg + d + 1 < s.len()
                    && digits(s, i + neg + d + 1) > 0;
                let uuid = hex_run(s, i, 8)
                    && s.get(i + 8) == Some(&b'-')
                    && hex_run(s, i + 9, 4)
                    && s.get(i + 13) == Some(&b'-')
                    && hex_run(s, i + 14, 4)
                    && s.get(i + 18) == Some(&b'-')
                    && hex_run(s, i + 19, 4)
                    && s.get(i + 23) == Some(&b'-')
                    && hex_run(s, i + 24, 12);
                if float {
                    tok(Kind::Float, i + neg + d + 1 + digits(s, i + neg + d + 1))
                } else if uuid {
                    tok(Kind::Uuid, i + 36)
                } else if c == b'0' && matches!(s.get(i + 1), Some(b'x' | b'X')) && hex_run(s, i + 2, 1) {
                    tok(Kind::Blob, i + 2 + s[i + 2..].iter().take_while(|b| b.is_ascii_hexdigit()).count())
                } else if c.is_ascii_digit() {
                    tok(Kind::WholeNumber, i + digits(s, i))
                } else if c.is_ascii_alphabetic() {
                    let e = i + s[i..].iter().take_while(|b| is_word(**b)).count();
                    let word = src[i..e].to_ascii_lowercase();
                    // `\bkw\b`: the start is a word boundary unless a word character precedes it.
                    let boundary = i == 0 || !is_word(s[i - 1]);
                    Token { kind: Kind::Identifier, start: i, end: e, reserved: boundary && RESERVED.contains(&word.as_str()) }
                } else {
                    match c {
                        b':' => tok(Kind::Colon, i + 1),
                        b'*' => tok(Kind::Star, i + 1),
                        b';' => tok(Kind::Endtoken, i + 1),
                        b'-' | b'+' | b'=' | b'%' | b'/' | b',' | b'(' | b')' | b'.' => tok(Kind::Op, i + 1),
                        b'<' | b'>' | b'!' => tok(Kind::Cmp, if s.get(i + 1) == Some(&b'=') { i + 2 } else { i + 1 }),
                        b'[' | b']' | b'{' | b'}' => tok(Kind::Brackets, i + 1),
                        b'?' => tok(Kind::Qmark, i + 1),
                        _ => {
                            let before = &src[..i];
                            let line = before.matches('\n').count() + 1;
                            let char = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
                            return Err(LexError { line, char });
                        }
                    }
                }
            }
        };
        i = t.end;
        out.push(t);
    }
    Ok(out)
}

/// `cql_massage_tokens`: newlines end the cqlsh commands that end at a newline and are dropped
/// everywhere else.
pub fn massage(src: &str, toks: Vec<Token>) -> Vec<Token> {
    let mut out = vec![];
    let mut cur: Vec<Token> = vec![];
    let mut term_on_nl = false;
    for mut t in toks {
        if t.kind == Kind::Endline {
            if term_on_nl {
                t.kind = Kind::Endtoken;
            } else {
                continue;
            }
        }
        cur.push(t);
        if t.kind == Kind::Endtoken {
            term_on_nl = false;
            out.append(&mut cur);
        } else if cur.len() == 1 {
            term_on_nl = ENDS_WITH_NEWLINE.contains(&t.text(src).to_lowercase().as_str());
        }
    }
    out.append(&mut cur);
    out
}

/// The result of [`split`].
pub struct Split {
    pub statements: Vec<Vec<Token>>,
    /// Inside a batch or an unclosed `$$` string: more input is needed.
    pub escaped: bool,
}

/// `cql_split_statements`.
pub fn split(src: &str) -> Result<Split, LexError> {
    let toks = massage(src, lex(src)?);
    let in_pg = toks.iter().filter(|t| t.kind == Kind::UnclosedPgString).count() == 1;
    let mut stmts: Vec<Vec<Token>> = vec![vec![]];
    for t in &toks {
        stmts.last_mut().unwrap().push(*t);
        if t.kind == Kind::Endtoken {
            stmts.push(vec![]);
        }
    }
    let mut out: Vec<Vec<Token>> = vec![];
    let mut in_batch = false;
    for st in stmts {
        let n = st.len();
        let (applies, begins) = if n > 2 {
            (st[n - 3].text(src).eq_ignore_ascii_case("apply"), st[0].text(src).eq_ignore_ascii_case("begin"))
        } else {
            (false, false)
        };
        if in_batch {
            out.last_mut().unwrap().extend(st);
        } else {
            out.push(st);
        }
        if applies {
            in_batch = false;
        } else if begins {
            in_batch = true;
        }
    }
    Ok(Split { statements: out, escaped: in_batch || in_pg })
}

/// `cql_extract_orig`: the source from the first token to the end of the last.
pub fn extract<'a>(src: &'a str, toks: &[Token]) -> &'a str {
    match (toks.first(), toks.last()) {
        (Some(a), Some(b)) => &src[a.start..b.end],
        _ => "",
    }
}

/// `dequote_name`: `"Foo""x"` → `Foo"x`; other names as written.
pub fn dequote_name(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].replace("\"\"", "\"")
    } else {
        s.to_string()
    }
}

/// `dequote_value`: `'it''s'` → `it's`, `$$x$$` → `x`.
pub fn dequote_value(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        s[1..s.len() - 1].replace("''", "'")
    } else if s.len() >= 4 && s.starts_with("$$") && s.ends_with("$$") {
        s[2..s.len() - 2].to_string()
    } else {
        s.to_string()
    }
}

/// A name as the server stores it: quoted names keep their case, others are lowercased.
pub fn unprotect(s: &str) -> String {
    if s.starts_with('"') { dequote_name(s) } else { s.to_lowercase() }
}

/// `protect_name`: quotes a name unless it is a valid unreserved lowercase identifier.
pub fn protect_name(name: &str) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !RESERVED.contains(&name);
    if plain { name.to_string() } else { format!("\"{}\"", name.replace('"', "\"\"")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<(Kind, &str)> {
        lex(s).unwrap().iter().map(|t| (t.kind, t.text(s))).collect()
    }

    #[test]
    fn lexes_like_cqlsh() {
        use Kind::*;
        assert_eq!(
            kinds("SELECT a, 'it''s', \"Q\"\"x\" FROM t WHERE k = -1.5;"),
            vec![
                (Identifier, "SELECT"),
                (Identifier, "a"),
                (Op, ","),
                (QuotedString, "'it''s'"),
                (Op, ","),
                (QuotedName, "\"Q\"\"x\""),
                (Identifier, "FROM"),
                (Identifier, "t"),
                (Identifier, "WHERE"),
                (Identifier, "k"),
                (Op, "="),
                (Float, "-1.5"),
                (Endtoken, ";"),
            ]
        );
        assert_eq!(kinds("-- c\nx // d\n/* e\nf */ y"), vec![(Identifier, "x"), (Identifier, "y")]);
        assert_eq!(kinds("$$a;b$$ $$open"), vec![(PgString, "$$a;b$$"), (UnclosedPgString, "$$open")]);
        assert_eq!(kinds("'abc"), vec![(UnclosedString, "'abc")]);
        // The regex backtracks: the first quote of a trailing '' closes the string.
        assert_eq!(kinds("'a'' b"), vec![(QuotedString, "'a'"), (UnclosedString, "' b")]);
        assert_eq!(kinds("/* open"), vec![(UnclosedComment, "/* open")]);
        assert_eq!(
            kinds("0xCAFE 12 1e5 550e8400-e29b-41d4-a716-446655440000 <= ? [ :"),
            vec![
                (Blob, "0xCAFE"),
                (WholeNumber, "12"),
                (WholeNumber, "1"),
                (Identifier, "e5"),
                (Uuid, "550e8400-e29b-41d4-a716-446655440000"),
                (Cmp, "<="),
                (Qmark, "?"),
                (Brackets, "["),
                (Colon, ":"),
            ]
        );
        assert_eq!(lex("SELECT &").unwrap_err(), LexError { line: 1, char: 8 });
        assert_eq!(lex("x\n é &").unwrap_err(), LexError { line: 2, char: 2 });
        let t = lex("select selectx").unwrap();
        assert!(t[0].reserved && !t[1].reserved);
    }

    #[test]
    fn splits_statements() {
        let src = "SELECT 1; SELECT 2;";
        let s = split(src).unwrap();
        assert_eq!(s.statements.len(), 3);
        assert_eq!(extract(src, &s.statements[1]), "SELECT 2;");
        assert!(s.statements[2].is_empty());
        // Batches stay together, even across lines.
        let src = "BEGIN BATCH INSERT INTO t (k) VALUES (1); INSERT INTO t (k) VALUES (2); APPLY BATCH;";
        let s = split(src).unwrap();
        assert_eq!(extract(src, &s.statements[0]), src);
        assert!(!s.escaped);
        assert!(split("BEGIN BATCH INSERT INTO t (k) VALUES (1);").unwrap().escaped);
        assert!(split("SELECT $$x").unwrap().escaped);
        // cqlsh commands end at a newline.
        let src = "EXPAND ON\nSELECT\n1;";
        let s = split(src).unwrap();
        assert_eq!(extract(src, &s.statements[0]), "EXPAND ON\n");
        assert_eq!(extract(src, &s.statements[1]), "SELECT\n1;");
    }

    #[test]
    fn names() {
        assert_eq!(dequote_name("\"A\"\"b\""), "A\"b");
        assert_eq!(dequote_value("'it''s'"), "it's");
        assert_eq!(dequote_value("$$x$$"), "x");
        assert_eq!(unprotect("Ks"), "ks");
        assert_eq!(unprotect("\"Ks\""), "Ks");
        assert_eq!(protect_name("t"), "t");
        assert_eq!(protect_name("t$paxos"), "\"t$paxos\"");
        assert_eq!(protect_name("IndexInfo"), "\"IndexInfo\"");
        assert_eq!(protect_name("select"), "\"select\"");
    }
}
