//! CQL type names: printing (`frozen<map<text, int>>`) and parsing.
//!
//! Printing follows `org.apache.cassandra.cql3.CQL3Type.toString` (Apache-2.0); ScyllaDB prints
//! identically in DESCRIBE and `system_schema` (both verified): tuples always show as
//! `frozen<tuple<…>>`, vectors never show `frozen`, user type names are quoted when needed, custom
//! types are the quoted class name.
//!
//! Parsing follows `CQL3Type.Raw.prepare`: `frozen<>` freezes recursively, tuples freeze their
//! components, non-frozen collections and UDTs may not nest, counters may not be elements, and
//! durations may not be set elements or map keys, all with the reference's messages.

use std::borrow::Cow;
use std::sync::Arc;

use crate::{CqlType, Dialect, UdtDef};

/// Cassandra 5.0's reserved keywords (`cql3/reserved_keywords.txt`).
const CASSANDRA_RESERVED: &[&str] = &[
    "ADD",
    "ALLOW",
    "ALTER",
    "AND",
    "APPLY",
    "ASC",
    "AUTHORIZE",
    "BATCH",
    "BEGIN",
    "BY",
    "COLUMNFAMILY",
    "CREATE",
    "DELETE",
    "DESC",
    "DESCRIBE",
    "DROP",
    "ENTRIES",
    "EXECUTE",
    "FROM",
    "FULL",
    "GRANT",
    "IF",
    "IN",
    "INDEX",
    "INFINITY",
    "INSERT",
    "INTO",
    "IS",
    "KEYSPACE",
    "LIMIT",
    "MATERIALIZED",
    "MODIFY",
    "NAN",
    "NORECURSIVE",
    "NOT",
    "NULL",
    "OF",
    "ON",
    "OR",
    "ORDER",
    "PRIMARY",
    "RENAME",
    "REVOKE",
    "SCHEMA",
    "SELECT",
    "SET",
    "TABLE",
    "TO",
    "TOKEN",
    "TRUNCATE",
    "UNLOGGED",
    "UPDATE",
    "USE",
    "USING",
    "VIEW",
    "WHERE",
    "WITH",
];

/// ScyllaDB's reserved keywords (docs, appendix A).
const SCYLLA_RESERVED: &[&str] = &[
    "ADD",
    "ALLOW",
    "ALTER",
    "AND",
    "APPLY",
    "ASC",
    "AUTHORIZE",
    "BATCH",
    "BEGIN",
    "BY",
    "COLUMNFAMILY",
    "CREATE",
    "DELETE",
    "DESC",
    "DESCRIBE",
    "DROP",
    "ENTRIES",
    "EXECUTE",
    "FROM",
    "FULL",
    "GRANT",
    "IF",
    "IN",
    "INDEX",
    "INFINITY",
    "INSERT",
    "INTO",
    "KEYSPACE",
    "LIMIT",
    "MODIFY",
    "NAN",
    "NORECURSIVE",
    "NOT",
    "NULL",
    "OF",
    "ON",
    "OR",
    "ORDER",
    "PRIMARY",
    "RENAME",
    "REPLACE",
    "REVOKE",
    "SCHEMA",
    "SELECT",
    "SET",
    "TABLE",
    "TO",
    "TOKEN",
    "TRUNCATE",
    "UNLOGGED",
    "UPDATE",
    "USE",
    "USING",
    "WHERE",
    "WITH",
];

/// Whether `word` (any case) is a reserved keyword of the dialect.
pub fn is_reserved(word: &str, dialect: Dialect) -> bool {
    let list = match dialect {
        Dialect::Cassandra => CASSANDRA_RESERVED,
        Dialect::Scylla => SCYLLA_RESERVED,
    };
    list.iter().any(|k| k.eq_ignore_ascii_case(word))
}

/// `ColumnIdentifier.maybeQuote`: identifiers matching `[a-z][a-z0-9_]*` that are not reserved
/// print bare, everything else in double quotes with `"` doubled.
pub fn quote_identifier(name: &str, dialect: Dialect) -> Cow<'_, str> {
    let b = name.as_bytes();
    let plain = !b.is_empty()
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
        && !is_reserved(name, dialect);
    if plain {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(format!("\"{}\"", name.replace('"', "\"\"")))
    }
}

/// The CQL name with Cassandra's identifier quoting.
pub fn cql_name(ty: &CqlType) -> String {
    cql_name_in(ty, Dialect::Cassandra)
}

/// The CQL name of a type as DESCRIBE and `system_schema` print it.
pub fn cql_name_in(ty: &CqlType, dialect: Dialect) -> String {
    let mut out = String::new();
    write_name(&mut out, ty, dialect);
    out
}

fn write_name(out: &mut String, ty: &CqlType, dialect: Dialect) {
    if let Some(n) = ty.native_name() {
        out.push_str(n);
        return;
    }
    match ty {
        CqlType::List { elem, frozen } | CqlType::Set { elem, frozen } => {
            if *frozen {
                out.push_str("frozen<");
            }
            out.push_str(if matches!(ty, CqlType::List { .. }) {
                "list<"
            } else {
                "set<"
            });
            write_name(out, elem, dialect);
            out.push('>');
            if *frozen {
                out.push('>');
            }
        }
        CqlType::Map { key, value, frozen } => {
            if *frozen {
                out.push_str("frozen<");
            }
            out.push_str("map<");
            write_name(out, key, dialect);
            out.push_str(", ");
            write_name(out, value, dialect);
            out.push('>');
            if *frozen {
                out.push('>');
            }
        }
        CqlType::Tuple(ts) => {
            out.push_str("frozen<tuple<");
            for (i, t) in ts.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_name(out, t, dialect);
            }
            out.push_str(">>");
        }
        CqlType::Udt(def, frozen) => {
            if *frozen {
                out.push_str("frozen<");
            }
            out.push_str(&quote_identifier(&def.name, dialect));
            if *frozen {
                out.push('>');
            }
        }
        CqlType::Vector { elem, dim } => {
            out.push_str("vector<");
            write_name(out, elem, dialect);
            out.push_str(", ");
            out.push_str(&dim.to_string());
            out.push('>');
        }
        CqlType::Custom(class) => {
            out.push('\'');
            out.push_str(class);
            out.push('\'');
        }
        _ => {}
    }
}

/// Resolves a user type name: `(keyspace, name)` as written (`None` keyspace = the statement's).
pub type UdtResolver<'a> = dyn Fn(&str, &str) -> Option<Arc<UdtDef>> + 'a;

/// A type as written, before resolution (`CQL3Type.Raw`).
#[derive(Clone, Debug, PartialEq)]
pub enum RawType {
    Native(CqlType),
    List(Box<RawType>, bool),
    Set(Box<RawType>, bool),
    Map(Box<RawType>, Box<RawType>, bool),
    Tuple(Vec<RawType>),
    Udt {
        keyspace: Option<String>,
        name: String,
        frozen: bool,
    },
    Vector(Box<RawType>, u32),
    Custom(String),
}

impl RawType {
    fn frozen(&self) -> bool {
        match self {
            RawType::List(_, f) | RawType::Set(_, f) | RawType::Map(_, _, f) => *f,
            RawType::Udt { frozen, .. } => *frozen,
            RawType::Tuple(_) | RawType::Vector(..) => true,
            _ => false,
        }
    }

    fn supports_freezing(&self) -> bool {
        !matches!(self, RawType::Native(_) | RawType::Custom(_))
    }

    fn is_collection(&self) -> bool {
        matches!(
            self,
            RawType::List(..) | RawType::Set(..) | RawType::Map(..)
        )
    }

    /// `Raw.freeze()`.
    pub fn freeze(self) -> Result<RawType, String> {
        Ok(match self {
            RawType::List(e, _) => RawType::List(Box::new(e.freeze_if_possible()), true),
            RawType::Set(e, _) => RawType::Set(Box::new(e.freeze_if_possible()), true),
            RawType::Map(k, v, _) => RawType::Map(
                Box::new(k.freeze_if_possible()),
                Box::new(v.freeze_if_possible()),
                true,
            ),
            RawType::Udt { keyspace, name, .. } => RawType::Udt {
                keyspace,
                name,
                frozen: true,
            },
            t @ (RawType::Tuple(_) | RawType::Vector(..)) => t,
            other => {
                return Err(format!(
                    "frozen<> is only allowed on collections, tuples, and user-defined types (got {other})"
                ));
            }
        })
    }

    fn freeze_if_possible(self) -> RawType {
        if self.supports_freezing() {
            self.freeze().unwrap_or_else(|_| unreachable!())
        } else {
            self
        }
    }

    fn nested_non_frozen_error(&self, inner: &RawType) -> Option<String> {
        if inner.frozen() || !inner.supports_freezing() {
            return None;
        }
        if inner.is_collection() {
            Some(format!(
                "Non-frozen collections are not allowed inside collections: {self}"
            ))
        } else if matches!(inner, RawType::Udt { .. }) {
            Some(format!(
                "Non-frozen UDTs are not allowed inside collections: {self}"
            ))
        } else {
            None
        }
    }

    /// `Raw.prepare`: resolves user types against `keyspace` and checks nesting rules.
    pub fn prepare(&self, keyspace: &str, resolve: &UdtResolver) -> Result<CqlType, String> {
        match self {
            RawType::Native(t) => Ok(t.clone()),
            RawType::Custom(class) => Ok(crate::marshal::parse_marshal(class)
                .map(|(t, _)| t)
                .unwrap_or_else(|_| CqlType::Custom(class.as_str().into()))),
            RawType::List(e, frozen) | RawType::Set(e, frozen) => {
                if !frozen && let Some(m) = self.nested_non_frozen_error(e) {
                    return Err(m);
                }
                if matches!(**e, RawType::Native(CqlType::Counter)) {
                    return Err(format!(
                        "Counters are not allowed inside collections: {self}"
                    ));
                }
                if matches!(self, RawType::Set(..))
                    && matches!(**e, RawType::Native(CqlType::Duration))
                {
                    return Err(format!("Durations are not allowed inside sets: {self}"));
                }
                let et = e.prepare(keyspace, resolve)?;
                Ok(if matches!(self, RawType::List(..)) {
                    CqlType::list(et, *frozen)
                } else {
                    CqlType::set(et, *frozen)
                })
            }
            RawType::Map(k, v, frozen) => {
                if !frozen && let Some(m) = self.nested_non_frozen_error(v) {
                    return Err(m);
                }
                if matches!(**v, RawType::Native(CqlType::Counter))
                    || matches!(**k, RawType::Native(CqlType::Counter))
                {
                    return Err(format!(
                        "Counters are not allowed inside collections: {self}"
                    ));
                }
                if matches!(**k, RawType::Native(CqlType::Duration)) {
                    return Err(format!("Durations are not allowed as map keys: {self}"));
                }
                if !frozen && let Some(m) = self.nested_non_frozen_error(k) {
                    return Err(m);
                }
                Ok(CqlType::map(
                    k.prepare(keyspace, resolve)?,
                    v.prepare(keyspace, resolve)?,
                    *frozen,
                ))
            }
            RawType::Tuple(ts) => {
                let mut out = Vec::with_capacity(ts.len());
                for t in ts {
                    if matches!(t, RawType::Native(CqlType::Counter)) {
                        return Err("Counters are not allowed inside tuples".into());
                    }
                    out.push(t.prepare(keyspace, resolve)?);
                }
                Ok(CqlType::Tuple(out))
            }
            RawType::Udt {
                keyspace: ks,
                name,
                frozen,
            } => {
                if let Some(ks) = ks
                    && ks != keyspace
                {
                    return Err(format!(
                        "Statement on keyspace {keyspace} cannot refer to a user type in keyspace {ks}; user types can only be used in the keyspace they are defined in"
                    ));
                }
                let def = resolve(keyspace, name)
                    .ok_or_else(|| format!("Unknown type {keyspace}.{name}"))?;
                Ok(CqlType::Udt(def, *frozen))
            }
            RawType::Vector(e, dim) => Ok(CqlType::vector(e.prepare(keyspace, resolve)?, *dim)),
        }
    }
}

impl std::fmt::Display for RawType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (start, end) = if self.frozen() && self.is_collection() {
            ("frozen<", ">")
        } else {
            ("", "")
        };
        match self {
            RawType::Native(t) => f.write_str(t.native_name().unwrap_or("?")),
            RawType::List(e, _) => write!(f, "{start}list<{e}>{end}"),
            RawType::Set(e, _) => write!(f, "{start}set<{e}>{end}"),
            RawType::Map(k, v, _) => write!(f, "{start}map<{k}, {v}>{end}"),
            RawType::Tuple(ts) => {
                f.write_str("tuple<")?;
                for (i, t) in ts.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{t}")?;
                }
                f.write_str(">")
            }
            RawType::Udt {
                keyspace,
                name,
                frozen,
            } => {
                let full = match keyspace {
                    Some(k) => format!("{k}.{name}"),
                    None => name.clone(),
                };
                if *frozen {
                    write!(f, "frozen<{full}>")
                } else {
                    f.write_str(&full)
                }
            }
            RawType::Vector(e, d) => write!(f, "vector<{e}, {d}>"),
            RawType::Custom(c) => write!(f, "'{c}'"),
        }
    }
}

/// Parses a CQL type as written (`frozen<list<"MyType">>`, `vector<float, 3>`, `'Class'`).
pub fn parse_raw_type(s: &str) -> Result<RawType, String> {
    let mut p = Parser {
        s: s.as_bytes(),
        i: 0,
        text: s,
    };
    let t = p.ty()?;
    p.ws();
    if p.i != p.s.len() {
        return Err(p.err());
    }
    Ok(t)
}

/// Parses and resolves a CQL type string, e.g. a `system_schema.columns.type` value.
pub fn parse_cql_type(s: &str, keyspace: &str, resolve: &UdtResolver) -> Result<CqlType, String> {
    parse_raw_type(s)?.prepare(keyspace, resolve)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    text: &'a str,
}

impl Parser<'_> {
    fn err(&self) -> String {
        format!("Invalid type '{}' at character {}", self.text, self.i)
    }

    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.eat(c) { Ok(()) } else { Err(self.err()) }
    }

    /// An identifier: bare (lowercased) or double-quoted (case kept, `""` unescaped).
    fn ident(&mut self) -> Result<(String, bool), String> {
        self.ws();
        if self.s.get(self.i) == Some(&b'"') {
            self.i += 1;
            let mut out = Vec::new();
            loop {
                match self.s.get(self.i) {
                    None => return Err(self.err()),
                    Some(b'"') if self.s.get(self.i + 1) == Some(&b'"') => {
                        out.push(b'"');
                        self.i += 2;
                    }
                    Some(b'"') => {
                        self.i += 1;
                        break;
                    }
                    Some(&c) => {
                        out.push(c);
                        self.i += 1;
                    }
                }
            }
            return String::from_utf8(out)
                .map(|s| (s, true))
                .map_err(|_| self.err());
        }
        let start = self.i;
        while self.i < self.s.len()
            && (self.s[self.i].is_ascii_alphanumeric() || self.s[self.i] == b'_')
        {
            self.i += 1;
        }
        if start == self.i || self.s[start].is_ascii_digit() {
            return Err(self.err());
        }
        Ok((self.text[start..self.i].to_ascii_lowercase(), false))
    }

    fn ty(&mut self) -> Result<RawType, String> {
        self.ws();
        if self.s.get(self.i) == Some(&b'\'') {
            self.i += 1;
            let start = self.i;
            while self.i < self.s.len() && self.s[self.i] != b'\'' {
                self.i += 1;
            }
            if self.i == self.s.len() {
                return Err(self.err());
            }
            let class = self.text[start..self.i].to_string();
            self.i += 1;
            return Ok(RawType::Custom(class));
        }
        let (word, quoted) = self.ident()?;
        if !quoted {
            match word.as_str() {
                "frozen" => {
                    self.expect(b'<')?;
                    let inner = self.ty()?;
                    self.expect(b'>')?;
                    return inner.freeze();
                }
                "list" | "set" => {
                    self.expect(b'<')?;
                    let e = self.ty()?;
                    self.expect(b'>')?;
                    return Ok(if word == "list" {
                        RawType::List(Box::new(e), false)
                    } else {
                        RawType::Set(Box::new(e), false)
                    });
                }
                "map" => {
                    self.expect(b'<')?;
                    let k = self.ty()?;
                    self.expect(b',')?;
                    let v = self.ty()?;
                    self.expect(b'>')?;
                    return Ok(RawType::Map(Box::new(k), Box::new(v), false));
                }
                "tuple" => {
                    self.expect(b'<')?;
                    let mut ts = vec![self.ty()?.freeze_if_possible()];
                    while self.eat(b',') {
                        ts.push(self.ty()?.freeze_if_possible());
                    }
                    self.expect(b'>')?;
                    return Ok(RawType::Tuple(ts));
                }
                "vector" => {
                    self.expect(b'<')?;
                    let e = self.ty()?;
                    self.expect(b',')?;
                    self.ws();
                    let start = self.i;
                    while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                        self.i += 1;
                    }
                    let dim: u32 = self.text[start..self.i].parse().map_err(|_| self.err())?;
                    if dim == 0 {
                        return Err(format!(
                            "vectors may only have positive dimensions; given {dim}"
                        ));
                    }
                    self.expect(b'>')?;
                    return Ok(RawType::Vector(Box::new(e), dim));
                }
                _ => {
                    if let Some(t) = CqlType::from_native_name(&word)
                        && !matches!(t, CqlType::Empty)
                    {
                        return Ok(RawType::Native(t));
                    }
                }
            }
        }
        // A user type, possibly keyspace-qualified.
        if self.eat(b'.') {
            let (name, _) = self.ident()?;
            return Ok(RawType::Udt {
                keyspace: Some(word),
                name,
                frozen: false,
            });
        }
        Ok(RawType::Udt {
            keyspace: None,
            name: word,
            frozen: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs() -> impl Fn(&str, &str) -> Option<Arc<UdtDef>> {
        |ks: &str, name: &str| {
            (ks == "ks" && (name == "addr" || name == "MyType" || name == "select"))
                .then(|| Arc::new(UdtDef::new("ks", name, vec![("a".into(), CqlType::Int)])))
        }
    }

    fn round(s: &str) -> String {
        let r = defs();
        cql_name(&parse_cql_type(s, "ks", &r).unwrap_or_else(|e| panic!("{s}: {e}")))
    }

    #[test]
    fn prints_like_describe() {
        // DESCRIBE TABLE / system_schema.columns on both images.
        for (written, printed) in [
            ("int", "int"),
            ("varchar", "text"),
            ("TEXT", "text"),
            ("list<int>", "list<int>"),
            ("set<frozen<list<text>>>", "set<frozen<list<text>>>"),
            (
                "map<text, frozen<map<int, blob>>>",
                "map<text, frozen<map<int, blob>>>",
            ),
            ("frozen<list<set<int>>>", "frozen<list<frozen<set<int>>>>"),
            (
                "tuple<int, text, frozen<list<int>>>",
                "frozen<tuple<int, text, frozen<list<int>>>>",
            ),
            (
                "tuple<int, list<int>>",
                "frozen<tuple<int, frozen<list<int>>>>",
            ),
            ("frozen<tuple<int>>", "frozen<tuple<int>>"),
            ("addr", "addr"),
            ("frozen<addr>", "frozen<addr>"),
            ("frozen<\"MyType\">", "frozen<\"MyType\">"),
            ("list<frozen<addr>>", "list<frozen<addr>>"),
            ("ks.addr", "addr"),
            ("vector<float, 3>", "vector<float, 3>"),
            ("vector<text,2>", "vector<text, 2>"),
            ("'org.apache.cassandra.db.marshal.EmptyType'", "empty"),
            (
                "'org.apache.cassandra.db.marshal.LexicalUUIDType'",
                "'org.apache.cassandra.db.marshal.LexicalUUIDType'",
            ),
            ("frozen< map < int , text > >", "frozen<map<int, text>>"),
            ("\"select\"", "\"select\""),
        ] {
            assert_eq!(round(written), printed, "{written}");
        }
    }

    #[test]
    fn rejects_like_cassandra() {
        let r = defs();
        for (s, m) in [
            (
                "list<list<int>>",
                "Non-frozen collections are not allowed inside collections: list<list<int>>",
            ),
            (
                "map<int, set<int>>",
                "Non-frozen collections are not allowed inside collections: map<int, set<int>>",
            ),
            (
                "map<set<int>, int>",
                "Non-frozen collections are not allowed inside collections: map<set<int>, int>",
            ),
            (
                "list<addr>",
                "Non-frozen UDTs are not allowed inside collections: list<addr>",
            ),
            (
                "list<counter>",
                "Counters are not allowed inside collections: list<counter>",
            ),
            (
                "map<counter, int>",
                "Counters are not allowed inside collections: map<counter, int>",
            ),
            (
                "set<duration>",
                "Durations are not allowed inside sets: set<duration>",
            ),
            (
                "map<duration, int>",
                "Durations are not allowed as map keys: map<duration, int>",
            ),
            ("tuple<counter>", "Counters are not allowed inside tuples"),
            (
                "frozen<int>",
                "frozen<> is only allowed on collections, tuples, and user-defined types (got int)",
            ),
            ("nope", "Unknown type ks.nope"),
            (
                "other.addr",
                "Statement on keyspace ks cannot refer to a user type in keyspace other; user types can only be used in the keyspace they are defined in",
            ),
            (
                "vector<float, 0>",
                "vectors may only have positive dimensions; given 0",
            ),
        ] {
            assert_eq!(parse_cql_type(s, "ks", &r).unwrap_err(), m, "{s}");
        }
        for bad in [
            "",
            "list<int",
            "map<int>",
            "list<>",
            "1abc",
            "'unterminated",
            "int int",
            "vector<float>",
            "\"open",
        ] {
            assert!(parse_cql_type(bad, "ks", &r).is_err(), "{bad}");
        }
        assert!(parse_cql_type("list<duration>", "ks", &r).is_ok());
        assert!(parse_cql_type("map<int, duration>", "ks", &r).is_ok());
    }

    #[test]
    fn identifier_quoting() {
        let c = Dialect::Cassandra;
        let s = Dialect::Scylla;
        assert_eq!(quote_identifier("addr", c), "addr");
        assert_eq!(quote_identifier("Zip", c), "\"Zip\"");
        assert_eq!(quote_identifier("select", c), "\"select\"");
        assert_eq!(quote_identifier("a\"b", c), "\"a\"\"b\"");
        assert_eq!(quote_identifier("1a", c), "\"1a\"");
        assert_eq!(quote_identifier("", c), "\"\"");
        assert_eq!(quote_identifier("view", c), "\"view\"");
        assert_eq!(quote_identifier("view", s), "view");
        assert_eq!(quote_identifier("replace", s), "\"replace\"");
        assert_eq!(quote_identifier("replace", c), "replace");
        assert!(is_reserved("Select", s));
        assert!(!is_reserved("text", c));
    }

    #[test]
    fn raw_display_and_freeze() {
        let r = parse_raw_type("map<text, frozen<list<int>>>").unwrap();
        assert_eq!(r.to_string(), "map<text, frozen<list<int>>>");
        assert_eq!(parse_raw_type("ks.addr").unwrap().to_string(), "ks.addr");
        assert_eq!(
            parse_raw_type("frozen<ks.addr>").unwrap().to_string(),
            "frozen<ks.addr>"
        );
        assert_eq!(parse_raw_type("'C'").unwrap().to_string(), "'C'");
        assert_eq!(
            parse_raw_type("vector<int, 2>").unwrap().to_string(),
            "vector<int, 2>"
        );
        assert_eq!(
            parse_raw_type("tuple<int, text>").unwrap().to_string(),
            "tuple<int, text>"
        );
        assert_eq!(
            cql_name_in(
                &CqlType::udt(UdtDef::new("k", "view", vec![]), false),
                Dialect::Cassandra
            ),
            "\"view\""
        );
        assert_eq!(
            cql_name_in(
                &CqlType::udt(UdtDef::new("k", "view", vec![]), false),
                Dialect::Scylla
            ),
            "view"
        );
    }
}
