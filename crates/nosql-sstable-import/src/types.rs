//! Column types as SSTables name them: Cassandra's `AbstractType` class names
//! (`org.apache.cassandra.db.marshal.ListType(org.apache.cassandra.db.marshal.Int32Type)`), as
//! written by `AbstractTypeSerializer` into the serialization header and by ScyllaDB into
//! `Scylla.db`. Parsing follows `org.apache.cassandra.db.marshal.TypeParser`.
//!
//! Only what reading needs is modelled: the fixed value length (`valueLengthIfFixed`), whether a
//! column is multi-cell (a non-frozen collection or UDT), counters, and a CQL rendering for
//! schema checks and recreating user types.

use anyhow::{Result, bail};

/// A parsed column type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SType {
    /// A native type, by CQL name (`int`, `text`, …; `timestamp` for the legacy `DateType`).
    Native(&'static str),
    Counter,
    List { elem: Box<SType>, frozen: bool },
    Set { elem: Box<SType>, frozen: bool },
    Map { key: Box<SType>, value: Box<SType>, frozen: bool },
    Tuple(Vec<SType>),
    Udt { keyspace: String, name: String, fields: Vec<(String, SType)>, frozen: bool },
    Vector { elem: Box<SType>, dim: u32 },
    /// A clustering column declared `DESC`.
    Reversed(Box<SType>),
    /// A composite partition key's components.
    Composite(Vec<SType>),
    /// An unknown marshal class (values are opaque, variable length).
    Custom(String),
}

const NATIVE: &[(&str, &str, Option<usize>)] = &[
    ("AsciiType", "ascii", None),
    ("LongType", "bigint", Some(8)),
    ("BytesType", "blob", None),
    ("BooleanType", "boolean", Some(1)),
    ("SimpleDateType", "date", None),
    ("DecimalType", "decimal", None),
    ("DoubleType", "double", Some(8)),
    ("DurationType", "duration", None),
    ("FloatType", "float", Some(4)),
    ("InetAddressType", "inet", None),
    ("Int32Type", "int", Some(4)),
    ("ShortType", "smallint", None),
    ("UTF8Type", "text", None),
    ("TimeType", "time", None),
    ("TimestampType", "timestamp", Some(8)),
    ("DateType", "timestamp", Some(8)),
    ("TimeUUIDType", "timeuuid", Some(16)),
    ("ByteType", "tinyint", None),
    ("UUIDType", "uuid", Some(16)),
    ("LexicalUUIDType", "uuid", Some(16)),
    ("IntegerType", "varint", None),
    ("EmptyType", "empty", Some(0)),
];

impl SType {
    /// Parse a marshal type name.
    pub fn parse(s: &str) -> Result<SType> {
        let mut p = Parser { s: s.as_bytes(), i: 0, depth: 0 };
        let t = p.ty()?;
        p.ws();
        if p.i != p.s.len() {
            bail!("bad type {s:?}: trailing input at {}", p.i);
        }
        Ok(t)
    }

    /// `AbstractType.valueLengthIfFixed`.
    pub fn fixed_len(&self) -> Option<usize> {
        match self {
            SType::Native(n) => NATIVE.iter().find(|(_, cql, _)| cql == n).and_then(|x| x.2),
            SType::Reversed(t) => t.fixed_len(),
            SType::Vector { elem, dim } => elem.fixed_len().and_then(|l| l.checked_mul(*dim as usize)),
            _ => None,
        }
    }

    /// A non-frozen collection or UDT: a complex column.
    pub fn is_multicell(&self) -> bool {
        match self {
            SType::List { frozen, .. } | SType::Set { frozen, .. } | SType::Map { frozen, .. } | SType::Udt { frozen, .. } => !frozen,
            _ => false,
        }
    }

    pub fn is_counter(&self) -> bool {
        matches!(self, SType::Counter)
    }

    /// The type without a `ReversedType` wrapper.
    pub fn base(&self) -> &SType {
        match self {
            SType::Reversed(t) => t.base(),
            t => t,
        }
    }

    /// Every user type this type uses, innermost first (definition order for `CREATE TYPE`).
    pub fn udts<'a>(&'a self, out: &mut Vec<&'a SType>) {
        match self {
            SType::List { elem, .. } | SType::Set { elem, .. } | SType::Vector { elem, .. } | SType::Reversed(elem) => elem.udts(out),
            SType::Map { key, value, .. } => {
                key.udts(out);
                value.udts(out);
            }
            SType::Tuple(ts) | SType::Composite(ts) => ts.iter().for_each(|t| t.udts(out)),
            SType::Udt { fields, .. } => {
                for (_, t) in fields {
                    t.udts(out);
                }
                if !out.iter().any(|u| same_udt(u, self)) {
                    out.push(self);
                }
            }
            _ => {}
        }
    }

    /// The type in CQL (`frozen<list<int>>`, `map<text, int>`, a UDT by its quoted name).
    /// Collections and UDTs nested in another type are frozen, as CQL requires.
    pub fn cql(&self) -> String {
        let nested = |t: &SType| freeze(t.clone()).cql();
        let wrap = |frozen: bool, s: String| if frozen { format!("frozen<{s}>") } else { s };
        match self {
            SType::Native(n) => n.to_string(),
            SType::Counter => "counter".into(),
            SType::List { elem, frozen } => wrap(*frozen, format!("list<{}>", nested(elem))),
            SType::Set { elem, frozen } => wrap(*frozen, format!("set<{}>", nested(elem))),
            SType::Map { key, value, frozen } => wrap(*frozen, format!("map<{}, {}>", nested(key), nested(value))),
            SType::Tuple(ts) => format!("tuple<{}>", ts.iter().map(nested).collect::<Vec<_>>().join(", ")),
            SType::Udt { name, frozen, .. } => wrap(*frozen, quote(name)),
            SType::Vector { elem, dim } => format!("vector<{}, {dim}>", nested(elem)),
            SType::Reversed(t) => t.cql(),
            SType::Composite(ts) => ts.iter().map(|t| t.cql()).collect::<Vec<_>>().join(", "),
            SType::Custom(c) => format!("'{}'", c.replace('\'', "''")),
        }
    }

    /// The type's structure without freezing (`map<text,list<int>>`, UDTs by name): for
    /// comparing with a schema whose spelling of nested freezing differs.
    pub fn shape(&self) -> String {
        match self {
            SType::Native(n) => n.to_string(),
            SType::Counter => "counter".into(),
            SType::List { elem, .. } => format!("list<{}>", elem.shape()),
            SType::Set { elem, .. } => format!("set<{}>", elem.shape()),
            SType::Map { key, value, .. } => format!("map<{},{}>", key.shape(), value.shape()),
            SType::Tuple(ts) => format!("tuple<{}>", ts.iter().map(|t| t.shape()).collect::<Vec<_>>().join(",")),
            SType::Udt { name, .. } => name.to_ascii_lowercase(),
            SType::Vector { elem, dim } => format!("vector<{},{dim}>", elem.shape()),
            SType::Reversed(t) => t.shape(),
            SType::Composite(ts) => ts.iter().map(|t| t.shape()).collect::<Vec<_>>().join(","),
            SType::Custom(c) => c.clone(),
        }
    }

    /// `CREATE TYPE IF NOT EXISTS` for a UDT.
    pub fn create_type(&self) -> Option<String> {
        let SType::Udt { keyspace, name, fields, .. } = self else { return None };
        let fs: Vec<String> = fields.iter().map(|(f, t)| format!("{} {}", quote(f), t.field_cql())).collect();
        Some(format!("CREATE TYPE IF NOT EXISTS {}.{} ({})", quote(keyspace), quote(name), fs.join(", ")))
    }

    /// A UDT field's type (always frozen when it's a collection or UDT).
    fn field_cql(&self) -> String {
        freeze(self.clone()).cql()
    }
}

fn same_udt(a: &SType, b: &SType) -> bool {
    matches!((a, b), (SType::Udt { keyspace: k1, name: n1, .. }, SType::Udt { keyspace: k2, name: n2, .. }) if k1 == k2 && n1 == n2)
}

/// Quote a CQL identifier when it isn't a plain lowercase one.
pub fn quote(id: &str) -> String {
    let plain = id.chars().next().is_some_and(|c| c.is_ascii_lowercase()) && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain && !is_reserved(id) { id.to_string() } else { format!("\"{}\"", id.replace('"', "\"\"")) }
}

fn is_reserved(id: &str) -> bool {
    const R: &[&str] = &[
        "add", "allow", "alter", "and", "apply", "asc", "authorize", "batch", "begin", "by", "columnfamily", "create", "delete", "desc", "describe", "drop", "entries", "execute", "from", "full", "grant",
        "if", "in", "index", "infinity", "insert", "into", "is", "keyspace", "limit", "materialized", "mbean", "mbeans", "modify", "nan", "norecursive", "not", "null", "of", "on", "or", "order",
        "primary", "rename", "replace", "revoke", "schema", "select", "set", "table", "to", "token", "truncate", "unlogged", "unset", "update", "use", "using", "view", "where", "with",
    ];
    R.contains(&id)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    depth: usize,
}

const MAX_DEPTH: usize = 64;

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> Result<()> {
        if !self.eat(c) {
            bail!("bad type {:?}: expected '{}' at {}", String::from_utf8_lossy(self.s), c as char, self.i);
        }
        Ok(())
    }

    /// An identifier: class names, keyspace names, hex names, numbers.
    fn word(&mut self) -> Result<&str> {
        self.ws();
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b'+' | b'$' | b'&') {
                self.i += 1;
            } else {
                break;
            }
        }
        if start == self.i {
            bail!("bad type {:?}: expected a name at {start}", String::from_utf8_lossy(self.s));
        }
        Ok(std::str::from_utf8(&self.s[start..self.i])?)
    }

    fn args(&mut self) -> Result<Vec<SType>> {
        let mut v = Vec::new();
        if self.eat(b')') {
            return Ok(v);
        }
        loop {
            v.push(self.ty()?);
            if self.eat(b')') {
                return Ok(v);
            }
            self.expect(b',')?;
        }
    }

    fn ty(&mut self) -> Result<SType> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            bail!("bad type: nested too deeply");
        }
        let full = self.word()?.to_string();
        let name = full.rsplit('.').next().unwrap_or(&full).to_string();
        let has_args = self.eat(b'(');
        let one = |mut a: Vec<SType>, what: &str| -> Result<SType> {
            if a.len() != 1 {
                bail!("bad type: {what} takes one argument, got {}", a.len());
            }
            Ok(a.remove(0))
        };
        let t = match (name.as_str(), has_args) {
            ("CounterColumnType", false) => SType::Counter,
            (n, false) => match NATIVE.iter().find(|(class, _, _)| *class == n) {
                Some((_, cql, _)) => SType::Native(cql),
                None => SType::Custom(full.clone()),
            },
            ("ReversedType", true) => SType::Reversed(Box::new(one(self.args()?, "ReversedType")?)),
            ("FrozenType", true) => freeze(one(self.args()?, "FrozenType")?),
            ("ListType", true) => SType::List { elem: Box::new(one(self.args()?, "ListType")?), frozen: false },
            ("SetType", true) => SType::Set { elem: Box::new(one(self.args()?, "SetType")?), frozen: false },
            ("MapType", true) => {
                let mut a = self.args()?;
                if a.len() != 2 {
                    bail!("bad type: MapType takes two arguments");
                }
                let value = a.pop().expect("2");
                let key = a.pop().expect("2");
                SType::Map { key: Box::new(key), value: Box::new(value), frozen: false }
            }
            ("TupleType", true) => SType::Tuple(self.args()?),
            ("CompositeType", true) => SType::Composite(self.args()?),
            ("VectorType", true) => {
                let elem = self.ty()?;
                self.expect(b',')?;
                let dim: u32 = self.word()?.parse().map_err(|_| anyhow::anyhow!("bad type: vector dimension"))?;
                self.expect(b')')?;
                SType::Vector { elem: Box::new(elem), dim }
            }
            ("UserType", true) => {
                let keyspace = self.word()?.to_string();
                self.expect(b',')?;
                let name = unhex(self.word()?)?;
                let mut fields = Vec::new();
                while self.eat(b',') {
                    let f = unhex(self.word()?)?;
                    self.expect(b':')?;
                    fields.push((f, self.ty()?));
                }
                self.expect(b')')?;
                SType::Udt { keyspace, name, fields, frozen: false }
            }
            (_, true) => {
                // An unknown parameterised class: skip its arguments, keep the text.
                let start = self.i;
                let mut depth = 1;
                while depth > 0 {
                    match self.peek() {
                        Some(b'(') => depth += 1,
                        Some(b')') => depth -= 1,
                        None => bail!("bad type: unbalanced parentheses"),
                        _ => {}
                    }
                    self.i += 1;
                }
                SType::Custom(format!("{full}({}", String::from_utf8_lossy(&self.s[start..self.i])))
            }
        };
        self.depth -= 1;
        Ok(t)
    }
}

fn freeze(t: SType) -> SType {
    match t {
        SType::List { elem, .. } => SType::List { elem, frozen: true },
        SType::Set { elem, .. } => SType::Set { elem, frozen: true },
        SType::Map { key, value, .. } => SType::Map { key, value, frozen: true },
        SType::Udt { keyspace, name, fields, .. } => SType::Udt { keyspace, name, fields, frozen: true },
        t => t,
    }
}

fn unhex(s: &str) -> Result<String> {
    if !s.len().is_multiple_of(2) {
        bail!("bad type: odd hex name {s:?}");
    }
    let bytes: Result<Vec<u8>> = (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| anyhow::anyhow!("bad type: hex name {s:?}"))).collect();
    Ok(String::from_utf8_lossy(&bytes?).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: &str = "org.apache.cassandra.db.marshal.";

    #[test]
    fn natives_and_fixed_lengths() {
        for (class, cql, len) in [("Int32Type", "int", Some(4)), ("UTF8Type", "text", None), ("TimestampType", "timestamp", Some(8)), ("SimpleDateType", "date", None), ("ShortType", "smallint", None), ("TimeUUIDType", "timeuuid", Some(16))] {
            let t = SType::parse(&format!("{M}{class}")).unwrap();
            assert_eq!(t, SType::Native(cql));
            assert_eq!(t.fixed_len(), len, "{class}");
        }
        assert!(SType::parse(&format!("{M}CounterColumnType")).unwrap().is_counter());
        let r = SType::parse(&format!("{M}ReversedType({M}LongType)")).unwrap();
        assert_eq!(r.fixed_len(), Some(8));
        assert_eq!(r.cql(), "bigint");
        let v = SType::parse(&format!("{M}VectorType({M}FloatType, 3)")).unwrap();
        assert_eq!((v.fixed_len(), v.cql()), (Some(12), "vector<float, 3>".into()));
        assert_eq!(SType::parse(&format!("{M}VectorType({M}UTF8Type,2)")).unwrap().fixed_len(), None);
    }

    #[test]
    fn collections_udts_and_freezing() {
        let l = SType::parse(&format!("{M}ListType({M}Int32Type)")).unwrap();
        assert!(l.is_multicell());
        assert_eq!(l.cql(), "list<int>");
        let f = SType::parse(&format!("{M}FrozenType({M}MapType({M}Int32Type,{M}UTF8Type))")).unwrap();
        assert!(!f.is_multicell());
        assert_eq!(f.cql(), "frozen<map<int, text>>");
        let udt = format!("{M}UserType(imp_fx,61646472,737472656574:{M}UTF8Type,7a6970:{M}Int32Type,74616773:{M}FrozenType({M}SetType({M}UTF8Type)))");
        let u = SType::parse(&udt).unwrap();
        assert!(u.is_multicell());
        assert_eq!(u.cql(), "addr");
        assert_eq!(u.create_type().unwrap(), "CREATE TYPE IF NOT EXISTS imp_fx.addr (street text, zip int, tags frozen<set<text>>)");
        let lu = SType::parse(&format!("{M}ListType({M}FrozenType({udt}))")).unwrap();
        assert_eq!(lu.cql(), "list<frozen<addr>>");
        assert_eq!(lu.shape(), "list<addr>");
        let mut us = vec![];
        lu.udts(&mut us);
        assert_eq!(us.len(), 1);
        let t = SType::parse(&format!("{M}TupleType({M}Int32Type,{M}UTF8Type,{M}FrozenType({M}ListType({M}Int32Type)))")).unwrap();
        assert_eq!(t.cql(), "tuple<int, text, frozen<list<int>>>");
        let c = SType::parse(&format!("{M}CompositeType({M}Int32Type,{M}UTF8Type)")).unwrap();
        assert!(matches!(c, SType::Composite(ref v) if v.len() == 2));
        assert!(matches!(SType::parse("com.example.Custom(a,b)").unwrap(), SType::Custom(_)));
    }

    #[test]
    fn bad_types_are_errors() {
        for bad in ["", "ListType(", "MapType(a)", "UserType(ks,6)", "ListType(Int32Type) x", &"ListType(".repeat(100)] {
            assert!(SType::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(quote("Quoted"), "\"Quoted\"");
        assert_eq!(quote("select"), "\"select\"");
        assert_eq!(quote("plain_1"), "plain_1");
    }
}
