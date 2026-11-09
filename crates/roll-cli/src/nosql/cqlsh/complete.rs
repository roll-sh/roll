//! Tab completion for the interactive shell: statement keywords, cqlsh commands, keyspaces,
//! tables, columns, consistency levels and HELP topics, from a schema snapshot taken before each
//! prompt. A small context model in the spirit of cqlsh's grammar-driven completer
//! (`cql3handling.py`), not a port of it.

use std::sync::{Arc, Mutex};

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};

use super::format::Flavor;
use super::lex::{self, Kind};

/// What completion knows about the schema.
#[derive(Clone, Debug, Default)]
pub struct Schema {
    pub keyspaces: Vec<String>,
    /// `(keyspace, table)`.
    pub tables: Vec<(String, String)>,
    /// `(keyspace, table, column)`.
    pub columns: Vec<(String, String, String)>,
    pub current: Option<String>,
    /// The lines of the statement typed so far.
    pub pending: String,
}

pub struct CqlHelper {
    pub schema: Arc<Mutex<Schema>>,
    pub flavor: Flavor,
}

pub type Editor = rustyline::Editor<CqlHelper, rustyline::history::FileHistory>;

const STATEMENTS: &[&str] = &[
    "ALTER", "APPLY", "BEGIN", "CAPTURE", "CLEAR", "CLS", "CONSISTENCY", "COPY", "CREATE", "DELETE", "DESC", "DESCRIBE", "DROP", "EXIT",
    "EXPAND", "GRANT", "HELP", "INSERT", "LIST", "LOGIN", "PAGING", "QUIT", "REVOKE", "SELECT", "SERIAL", "SHOW", "SOURCE", "TRACING",
    "TRUNCATE", "UPDATE", "USE",
];

const LEVELS: &[&str] =
    &["ANY", "ONE", "TWO", "THREE", "QUORUM", "ALL", "LOCAL_QUORUM", "EACH_QUORUM", "SERIAL", "LOCAL_SERIAL", "LOCAL_ONE", "NODE_LOCAL"];

const DESCRIBE: &[&str] = &[
    "AGGREGATE", "AGGREGATES", "CLUSTER", "COLUMNFAMILIES", "COLUMNFAMILY", "FULL", "FUNCTION", "FUNCTIONS", "INDEX", "KEYSPACE", "KEYSPACES",
    "MATERIALIZED", "ONLY", "SCHEMA", "TABLE", "TABLES", "TYPE", "TYPES",
];

const CREATE: &[&str] =
    &["AGGREGATE", "COLUMNFAMILY", "CUSTOM", "FUNCTION", "INDEX", "KEYSPACE", "MATERIALIZED", "OR", "ROLE", "TABLE", "TRIGGER", "TYPE", "USER"];

const CLAUSES: &[&str] = &[
    "AND", "ALLOW", "ASC", "BY", "CONTAINS", "DESC", "DISTINCT", "FILTERING", "FROM", "GROUP", "IF", "IN", "INTO", "JSON", "KEY", "LIMIT", "NOT",
    "NULL", "ORDER", "PER", "PARTITION", "SET", "TIMESTAMP", "TOKEN", "TTL", "USING", "VALUES", "WHERE", "WITH",
];

impl CqlHelper {
    /// Candidates for the word being typed, given everything before it.
    pub fn candidates(&self, before: &str, partial: &str) -> Vec<String> {
        let s = self.schema.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let text = format!("{}{before}", s.pending);
        let toks = match lex::split(&text) {
            Ok(sp) => sp.statements.last().cloned().unwrap_or_default(),
            Err(_) => return vec![],
        };
        let words: Vec<String> = toks.iter().map(|t| t.text(&text).to_string()).collect();
        let upper: Vec<String> = words.iter().map(|w| w.to_uppercase()).collect();
        let last = upper.last().map(String::as_str).unwrap_or("");
        let last_dot = toks.last().is_some_and(|t| t.kind == Kind::Op && t.text(&text) == ".");
        let ks_before_dot = if last_dot && words.len() >= 2 { Some(lex::unprotect(&words[words.len() - 2])) } else { None };
        let quoted = |n: &str| lex::protect_name(n);
        let tables_in = |ks: &str| -> Vec<String> { s.tables.iter().filter(|(k, _)| k == ks).map(|(_, t)| quoted(t)).collect() };
        let table_names = || -> Vec<String> {
            let mut v: Vec<String> = s.current.as_deref().map(tables_in).unwrap_or_default();
            v.extend(s.keyspaces.iter().map(|k| format!("{}.", quoted(k))));
            v
        };
        // The table the statement is about, for column names.
        let target = || -> Option<(String, String)> {
            for (i, u) in upper.iter().enumerate() {
                if matches!(u.as_str(), "FROM" | "INTO" | "UPDATE" | "TABLE") && i + 1 < words.len() {
                    if words.get(i + 2).map(String::as_str) == Some(".") && i + 3 < words.len() {
                        return Some((lex::unprotect(&words[i + 1]), lex::unprotect(&words[i + 3])));
                    }
                    return s.current.clone().map(|c| (c, lex::unprotect(&words[i + 1])));
                }
            }
            None
        };
        let columns = || -> Vec<String> {
            match target() {
                Some((k, t)) => s.columns.iter().filter(|(a, b, _)| *a == k && *b == t).map(|(_, _, c)| quoted(c)).collect(),
                None => vec![],
            }
        };
        let mut out: Vec<String> = if toks.is_empty() || toks.last().is_some_and(|t| t.kind == Kind::Endtoken) {
            STATEMENTS.iter().map(|s| s.to_string()).collect()
        } else if let Some(ks) = ks_before_dot {
            tables_in(&ks)
        } else {
            match (upper.first().map(String::as_str).unwrap_or(""), last) {
                ("USE", "USE") => s.keyspaces.iter().map(|k| quoted(k)).collect(),
                ("CONSISTENCY", _) => LEVELS.iter().map(|s| s.to_string()).collect(),
                ("SERIAL", "SERIAL") => vec!["CONSISTENCY".into()],
                ("SERIAL", "CONSISTENCY") => vec!["SERIAL".into(), "LOCAL_SERIAL".into()],
                ("TRACING" | "EXPAND", _) => vec!["ON".into(), "OFF".into()],
                ("PAGING", _) => vec!["ON".into(), "OFF".into()],
                ("SHOW", "SHOW") => vec!["HOST".into(), "REPLICAS".into(), "SESSION".into(), "VERSION".into()],
                ("HELP" | "?", _) => super::help::topics(self.flavor),
                ("DESC" | "DESCRIBE", "DESC" | "DESCRIBE") => {
                    let mut v: Vec<String> = DESCRIBE.iter().map(|s| s.to_string()).collect();
                    v.extend(table_names());
                    v
                }
                (_, "KEYSPACE") => s.keyspaces.iter().map(|k| quoted(k)).collect(),
                (_, "FROM" | "INTO" | "UPDATE" | "TABLE" | "COLUMNFAMILY" | "TRUNCATE" | "COPY" | "ON") => table_names(),
                ("CREATE" | "DROP" | "ALTER", "CREATE" | "DROP" | "ALTER") => CREATE.iter().map(|s| s.to_string()).collect(),
                ("COPY", _) => {
                    let mut v = columns();
                    v.extend(["FROM", "TO", "WITH", "AND", "STDIN", "STDOUT"].iter().map(|s| s.to_string()));
                    v
                }
                _ => {
                    let mut v = columns();
                    v.extend(CLAUSES.iter().map(|s| s.to_string()));
                    v
                }
            }
        };
        let p = partial.to_lowercase();
        out.retain(|c| lex::dequote_name(c).to_lowercase().starts_with(&p) || c.to_lowercase().starts_with(&p));
        out.sort();
        out.dedup();
        // Keywords keep the case being typed.
        if partial.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
            for c in out.iter_mut() {
                if c.chars().all(|x| x.is_ascii_uppercase() || x == '_') {
                    *c = c.to_lowercase();
                }
            }
        }
        out
    }
}

impl Completer for CqlHelper {
    type Candidate = Pair;

    fn complete(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> rustyline::Result<(usize, Vec<Pair>)> {
        let head = &line[..pos];
        let start = head.rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '"')).map(|i| i + 1).unwrap_or(0);
        let partial = &head[start..];
        let cands = self.candidates(&head[..start], partial);
        let single = cands.len() == 1;
        Ok((
            start,
            cands
                .into_iter()
                .map(|c| {
                    let replacement = if single && !c.ends_with('.') { format!("{c} ") } else { c.clone() };
                    Pair { display: c, replacement }
                })
                .collect(),
        ))
    }
}

impl Hinter for CqlHelper {
    type Hint = String;
}

impl Highlighter for CqlHelper {}

impl Validator for CqlHelper {}

impl Helper for CqlHelper {}

/// Masks what is typed (LOGIN and the password prompt).
pub struct Masked;

impl Completer for Masked {
    type Candidate = String;
}

impl Hinter for Masked {
    type Hint = String;
}

impl Highlighter for Masked {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> std::borrow::Cow<'l, str> {
        std::borrow::Cow::Owned("*".repeat(line.chars().count()))
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: rustyline::highlight::CmdKind) -> bool {
        true
    }
}

impl Validator for Masked {}

impl Helper for Masked {}

#[cfg(test)]
mod tests {
    use super::*;

    fn helper() -> CqlHelper {
        CqlHelper {
            schema: Arc::new(Mutex::new(Schema {
                keyspaces: vec!["ks".into(), "Other".into()],
                tables: vec![("ks".into(), "users".into()), ("ks".into(), "events".into()), ("Other".into(), "T".into())],
                columns: vec![("ks".into(), "users".into(), "id".into()), ("ks".into(), "users".into(), "Name".into())],
                current: Some("ks".into()),
                pending: String::new(),
            })),
            flavor: Flavor::Cassandra,
        }
    }

    #[test]
    fn completes_from_context() {
        let h = helper();
        assert_eq!(h.candidates("", "SEL"), vec!["SELECT"]);
        assert_eq!(h.candidates("", "sel"), vec!["select"]);
        assert_eq!(h.candidates("USE ", ""), vec!["\"Other\"", "ks"]);
        assert_eq!(h.candidates("SELECT * FROM ", "us"), vec!["users"]);
        assert_eq!(h.candidates("SELECT * FROM ", ""), vec!["\"Other\".", "events", "ks.", "users"]);
        assert_eq!(h.candidates("SELECT * FROM \"Other\".", ""), vec!["\"T\""]);
        assert_eq!(h.candidates("SELECT * FROM users WHERE ", "N"), vec!["\"Name\"", "NOT", "NULL"]);
        assert_eq!(h.candidates("CONSISTENCY ", "LOCAL_"), vec!["LOCAL_ONE", "LOCAL_QUORUM", "LOCAL_SERIAL"]);
        assert_eq!(h.candidates("TRACING ", ""), vec!["OFF", "ON"]);
        assert!(h.candidates("HELP ", "CREATE_T").contains(&"CREATE_TABLE".to_string()));
        assert_eq!(h.candidates("SELECT 1; ", "UPD"), vec!["UPDATE"]);
        h.schema.lock().unwrap().pending = "SELECT *\n".into();
        assert_eq!(h.candidates("FROM ", "ev"), vec!["events"]);
    }
}
