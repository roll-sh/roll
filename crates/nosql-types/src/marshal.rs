//! Java marshal class names (`org.apache.cassandra.db.marshal.MapType(…)`), both ways.
//!
//! They appear in protocol custom types (vectors), in error messages and in legacy schema
//! columns. Printing follows each class's `toString(ignoreFreezing)` and `TypeParser.stringify*`
//! (Apache-2.0) for Cassandra:
//!
//! - frozen collections and UDTs are wrapped in `FrozenType(…)`, but their components print
//!   without it (`FrozenType(ListType(SetType(Int32Type)))`); tuples never show it;
//! - parameters are joined with `,`; vectors are `VectorType(FloatType , 3)`;
//! - UDTs are `UserType(ks,hex(name),hex(field):Type,…)`.
//!
//! ScyllaDB (from its error texts and protocol metadata) shows `FrozenType` at every level,
//! joins tuple components with `, ` and writes vectors as `VectorType(FloatType, 3)`.
//!
//! Parsing follows `TypeParser`: the package prefix is optional, collections built from a class
//! name are multi-cell with frozen components, `FrozenType` freezes, and a top-level
//! `ReversedType` is reported separately.

use std::sync::Arc;

use crate::wire::{hex, unhex};
use crate::{CqlType, Dialect, UdtDef};

pub const PACKAGE: &str = "org.apache.cassandra.db.marshal.";

/// The simple class name of a native type.
pub fn native_class(ty: &CqlType) -> Option<&'static str> {
    Some(match ty {
        CqlType::Ascii => "AsciiType",
        CqlType::Bigint => "LongType",
        CqlType::Blob => "BytesType",
        CqlType::Boolean => "BooleanType",
        CqlType::Counter => "CounterColumnType",
        CqlType::Date => "SimpleDateType",
        CqlType::Decimal => "DecimalType",
        CqlType::Double => "DoubleType",
        CqlType::Duration => "DurationType",
        CqlType::Empty => "EmptyType",
        CqlType::Float => "FloatType",
        CqlType::Inet => "InetAddressType",
        CqlType::Int => "Int32Type",
        CqlType::Smallint => "ShortType",
        CqlType::Text => "UTF8Type",
        CqlType::Time => "TimeType",
        CqlType::Timestamp => "TimestampType",
        CqlType::Timeuuid => "TimeUUIDType",
        CqlType::Tinyint => "ByteType",
        CqlType::Uuid => "UUIDType",
        CqlType::Varint => "IntegerType",
        _ => return None,
    })
}

/// The marshal class string of a type (`reversed` wraps it in `ReversedType`).
pub fn marshal_name(ty: &CqlType, reversed: bool, dialect: Dialect) -> String {
    let mut out = String::new();
    if reversed {
        out.push_str(PACKAGE);
        out.push_str("ReversedType(");
    }
    write(&mut out, ty, false, dialect);
    if reversed {
        out.push(')');
    }
    out
}

fn write(out: &mut String, ty: &CqlType, ignore_freezing: bool, dialect: Dialect) {
    if let Some(c) = native_class(ty) {
        out.push_str(PACKAGE);
        out.push_str(c);
        return;
    }
    let scylla = dialect.is_scylla();
    let frozen = !ty.is_multicell();
    let wrap = (ty.is_collection() || ty.is_udt()) && frozen && (scylla || !ignore_freezing);
    if wrap {
        out.push_str(PACKAGE);
        out.push_str("FrozenType(");
    }
    // Inside a frozen collection Cassandra stops printing FrozenType.
    let inner_ignore = !scylla && (ignore_freezing || frozen);
    match ty {
        CqlType::List { elem, .. } | CqlType::Set { elem, .. } => {
            out.push_str(PACKAGE);
            out.push_str(if matches!(ty, CqlType::List { .. }) {
                "ListType("
            } else {
                "SetType("
            });
            write(out, elem, inner_ignore, dialect);
            out.push(')');
        }
        CqlType::Map { key, value, .. } => {
            out.push_str(PACKAGE);
            out.push_str("MapType(");
            write(out, key, inner_ignore, dialect);
            out.push(',');
            write(out, value, inner_ignore, dialect);
            out.push(')');
        }
        CqlType::Tuple(ts) => {
            out.push_str(PACKAGE);
            out.push_str("TupleType(");
            for (i, t) in ts.iter().enumerate() {
                if i > 0 {
                    out.push_str(if scylla { ", " } else { "," });
                }
                write(out, t, !scylla, dialect);
            }
            out.push(')');
        }
        CqlType::Udt(def, _) => {
            out.push_str(PACKAGE);
            out.push_str("UserType(");
            out.push_str(&def.keyspace);
            out.push(',');
            out.push_str(&hex(def.name.as_bytes()));
            for (n, t) in &def.fields {
                out.push(',');
                out.push_str(&hex(n.as_bytes()));
                out.push(':');
                write(out, t, inner_ignore, dialect);
            }
            out.push(')');
        }
        CqlType::Vector { elem, dim } => {
            out.push_str(PACKAGE);
            out.push_str("VectorType(");
            write(out, elem, !scylla && ignore_freezing, dialect);
            out.push_str(if scylla { ", " } else { " , " });
            out.push_str(&dim.to_string());
            out.push(')');
        }
        CqlType::Custom(class) => out.push_str(class),
        _ => {}
    }
    if wrap {
        out.push(')');
    }
}

/// Parses a marshal class string. Returns the type and whether it was `ReversedType(…)`.
pub fn parse_marshal(s: &str) -> Result<(CqlType, bool), String> {
    let mut p = P {
        s: s.as_bytes(),
        i: 0,
        text: s,
        depth: 0,
    };
    let (t, rev) = p.parse()?;
    p.ws();
    if p.i != p.s.len() {
        return Err(p.err("unexpected character"));
    }
    Ok((t, rev))
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
    text: &'a str,
    depth: usize,
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'+' | b'.' | b'_' | b'&')
}

impl P<'_> {
    fn err(&self, msg: &str) -> String {
        format!(
            "Syntax error parsing '{}' at char {}: {msg}",
            self.text, self.i
        )
    }

    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n') {
            self.i += 1;
        }
    }

    fn ident(&mut self) -> &str {
        self.ws();
        let start = self.i;
        while self.i < self.s.len() && is_ident(self.s[self.i]) {
            self.i += 1;
        }
        &self.text[start..self.i]
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

    /// `skipBlankAndComma`: at most one comma among blanks; false at end of input.
    fn sep(&mut self) -> bool {
        let mut comma = false;
        while self.i < self.s.len() {
            match self.s[self.i] {
                b',' if comma => return true,
                b',' => comma = true,
                b' ' | b'\t' | b'\n' => {}
                _ => return true,
            }
            self.i += 1;
        }
        false
    }

    fn params(&mut self) -> Result<Vec<CqlType>, String> {
        if !self.eat(b'(') {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        while self.sep() {
            if self.s[self.i] == b')' {
                self.i += 1;
                return Ok(out);
            }
            out.push(self.parse()?.0);
        }
        Err(self.err("unexpected end of string"))
    }

    fn one(&mut self, class: &str) -> Result<CqlType, String> {
        let mut ps = self.params()?;
        if ps.len() != 1 {
            return Err(format!("{class} takes exactly 1 type parameter"));
        }
        Ok(ps.remove(0))
    }

    fn parse(&mut self) -> Result<(CqlType, bool), String> {
        self.depth += 1;
        if self.depth > 64 {
            return Err(self.err("nesting too deep"));
        }
        let name = self.ident().to_string();
        let short = name.strip_prefix(PACKAGE).unwrap_or(&name);
        let t = match short {
            "" => return Err(self.err("expecting a type name")),
            "ReversedType" => {
                let t = self.one("ReversedType")?;
                self.depth -= 1;
                return Ok((t, true));
            }
            "FrozenType" => self.one("FrozenType")?.freeze(),
            "ListType" => CqlType::list(self.one("ListType")?.freeze(), false),
            "SetType" => CqlType::set(self.one("SetType")?.freeze(), false),
            "MapType" => {
                let ps = self.params()?;
                if ps.len() != 2 {
                    return Err("MapType takes exactly 2 type parameters".into());
                }
                CqlType::map(ps[0].freeze(), ps[1].freeze(), false)
            }
            "TupleType" => CqlType::Tuple(self.params()?.iter().map(CqlType::freeze).collect()),
            "VectorType" => {
                if !self.eat(b'(') {
                    return Err(self.err("expecting '('"));
                }
                let (e, _) = self.parse()?;
                if !self.sep() {
                    return Err(self.err("unexpected end of string"));
                }
                let dim: u32 = self
                    .ident()
                    .parse()
                    .map_err(|_| self.err("invalid vector dimension"))?;
                if dim == 0 {
                    return Err("vectors may only have positive dimensions; given 0".into());
                }
                if !self.eat(b')') {
                    return Err(self.err("expecting ')'"));
                }
                CqlType::vector(e.freeze(), dim)
            }
            "UserType" => self.user_type()?,
            "DateType" => CqlType::Timestamp,
            "LexicalUUIDType" => CqlType::Custom(format!("{PACKAGE}LexicalUUIDType").into()),
            other => match crate::NATIVE_TYPES
                .iter()
                .find(|t| native_class(t) == Some(other))
            {
                Some(t) => t.clone(),
                None => {
                    // Unknown classes (and their parameters) stay opaque.
                    let start = self.i;
                    if self.eat(b'(') {
                        let mut depth = 1;
                        while self.i < self.s.len() && depth > 0 {
                            match self.s[self.i] {
                                b'(' => depth += 1,
                                b')' => depth -= 1,
                                _ => {}
                            }
                            self.i += 1;
                        }
                        if depth != 0 {
                            return Err(self.err("unexpected end of string"));
                        }
                    }
                    let full = if name.contains('.') {
                        name.clone()
                    } else {
                        format!("{PACKAGE}{name}")
                    };
                    CqlType::Custom(format!("{full}{}", &self.text[start..self.i]).trim().into())
                }
            },
        };
        self.depth -= 1;
        Ok((t, false))
    }

    fn user_type(&mut self) -> Result<CqlType, String> {
        if !self.eat(b'(') {
            return Err(self.err("expecting '('"));
        }
        self.sep();
        let keyspace = self.ident().to_string();
        self.sep();
        let name = self.ident().to_string();
        let name = unhex(&name)
            .and_then(|b| String::from_utf8(b).ok())
            .ok_or_else(|| self.err("invalid hex name"))?;
        let mut fields = Vec::new();
        while self.sep() {
            if self.s[self.i] == b')' {
                self.i += 1;
                return Ok(CqlType::udt(
                    UdtDef {
                        keyspace,
                        name,
                        fields,
                    },
                    false,
                ));
            }
            let f = self.ident().to_string();
            let f = unhex(&f)
                .and_then(|b| String::from_utf8(b).ok())
                .ok_or_else(|| self.err("invalid hex field name"))?;
            if !self.eat(b':') {
                return Err(self.err("expecting ':' token"));
            }
            fields.push((f, self.parse()?.0));
        }
        Err(self.err("unexpected end of string"))
    }
}

/// Shorthand for tests and callers holding an `Arc<UdtDef>`.
pub fn udt_marshal_name(def: &Arc<UdtDef>, frozen: bool, dialect: Dialect) -> String {
    marshal_name(&CqlType::Udt(def.clone(), frozen), false, dialect)
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: Dialect = Dialect::Cassandra;
    const S: Dialect = Dialect::Scylla;

    fn p(s: &str) -> String {
        s.replace('~', PACKAGE)
    }

    fn addr() -> Arc<UdtDef> {
        Arc::new(UdtDef::new(
            "nosql_types",
            "addr",
            vec![
                ("street".into(), CqlType::Text),
                ("Zip".into(), CqlType::Int),
                ("select".into(), CqlType::Text),
            ],
        ))
    }

    #[test]
    fn scylla_spellings() {
        // From ScyllaDB's "X cannot be cast to Y" errors and protocol metadata.
        let fl = CqlType::list(CqlType::set(CqlType::Int, true), true);
        assert_eq!(
            marshal_name(&fl, false, S),
            p("~FrozenType(~ListType(~FrozenType(~SetType(~Int32Type))))")
        );
        let lm = CqlType::list(CqlType::map(CqlType::Text, CqlType::Int, true), false);
        assert_eq!(
            marshal_name(&lm, false, S),
            p("~ListType(~FrozenType(~MapType(~UTF8Type,~Int32Type)))")
        );
        let t = CqlType::Tuple(vec![CqlType::Int, CqlType::Text, CqlType::Boolean]);
        assert_eq!(
            marshal_name(&t, false, S),
            p("~TupleType(~Int32Type, ~UTF8Type, ~BooleanType)")
        );
        let mm = CqlType::map(
            CqlType::Tuple(vec![CqlType::Int, CqlType::Int]),
            CqlType::Text,
            false,
        );
        assert_eq!(
            marshal_name(&mm, false, S),
            p("~MapType(~TupleType(~Int32Type, ~Int32Type),~UTF8Type)")
        );
        assert_eq!(
            marshal_name(&CqlType::set(CqlType::Inet, false), false, S),
            p("~SetType(~InetAddressType)")
        );
        assert_eq!(
            marshal_name(&CqlType::vector(CqlType::Int, 2), false, S),
            p("~VectorType(~Int32Type, 2)")
        );
        assert_eq!(
            marshal_name(&CqlType::vector(CqlType::Float, 3), false, S),
            p("~VectorType(~FloatType, 3)")
        );
        assert_eq!(
            marshal_name(&CqlType::Duration, false, S),
            p("~DurationType")
        );
        assert_eq!(
            udt_marshal_name(&addr(), true, S),
            p(
                "~FrozenType(~UserType(nosql_types,61646472,737472656574:~UTF8Type,5a6970:~Int32Type,73656c656374:~UTF8Type))"
            )
        );
        assert_eq!(
            marshal_name(&CqlType::list(CqlType::Udt(addr(), true), false), false, S),
            p(
                "~ListType(~FrozenType(~UserType(nosql_types,61646472,737472656574:~UTF8Type,5a6970:~Int32Type,73656c656374:~UTF8Type)))"
            )
        );
    }

    #[test]
    fn cassandra_spellings() {
        let fl = CqlType::list(CqlType::set(CqlType::Int, true), true);
        assert_eq!(
            marshal_name(&fl, false, C),
            p("~FrozenType(~ListType(~SetType(~Int32Type)))")
        );
        let lm = CqlType::list(CqlType::map(CqlType::Text, CqlType::Int, true), false);
        assert_eq!(
            marshal_name(&lm, false, C),
            p("~ListType(~FrozenType(~MapType(~UTF8Type,~Int32Type)))")
        );
        let t = CqlType::Tuple(vec![CqlType::Int, CqlType::list(CqlType::Int, true)]);
        assert_eq!(
            marshal_name(&t, false, C),
            p("~TupleType(~Int32Type,~ListType(~Int32Type))")
        );
        assert_eq!(
            marshal_name(&CqlType::vector(CqlType::Float, 3), false, C),
            p("~VectorType(~FloatType , 3)")
        );
        assert_eq!(
            marshal_name(&CqlType::Int, true, C),
            p("~ReversedType(~Int32Type)")
        );
        assert_eq!(
            marshal_name(&CqlType::list(CqlType::Int, true), true, C),
            p("~ReversedType(~FrozenType(~ListType(~Int32Type)))")
        );
        assert_eq!(
            udt_marshal_name(&addr(), false, C),
            p(
                "~UserType(nosql_types,61646472,737472656574:~UTF8Type,5a6970:~Int32Type,73656c656374:~UTF8Type)"
            )
        );
        for t in crate::NATIVE_TYPES {
            assert_eq!(
                marshal_name(t, false, C),
                format!("{PACKAGE}{}", native_class(t).unwrap())
            );
        }
        assert_eq!(
            marshal_name(&CqlType::Custom("a.B".into()), false, C),
            "a.B"
        );
    }

    #[test]
    fn parse_round_trips() {
        let types = vec![
            CqlType::Int,
            CqlType::list(CqlType::Int, false),
            CqlType::list(CqlType::Int, true),
            CqlType::list(CqlType::set(CqlType::Int, true), true),
            CqlType::map(CqlType::Text, CqlType::list(CqlType::Blob, true), false),
            CqlType::Tuple(vec![CqlType::Int, CqlType::list(CqlType::Int, true)]),
            CqlType::Udt(addr(), true),
            CqlType::Udt(addr(), false),
            CqlType::list(CqlType::Udt(addr(), true), false),
            CqlType::vector(CqlType::Float, 3),
            CqlType::vector(CqlType::Text, 2),
            CqlType::Duration,
        ];
        for d in [C, S] {
            for t in &types {
                let s = marshal_name(t, false, d);
                assert_eq!(parse_marshal(&s), Ok((t.clone(), false)), "{s}");
                let r = marshal_name(t, true, d);
                assert_eq!(parse_marshal(&r), Ok((t.clone(), true)), "{r}");
            }
        }
        assert_eq!(parse_marshal("Int32Type"), Ok((CqlType::Int, false)));
        assert_eq!(parse_marshal(" UTF8Type "), Ok((CqlType::Text, false)));
        assert_eq!(parse_marshal("DateType"), Ok((CqlType::Timestamp, false)));
        assert_eq!(
            parse_marshal("ListType(Int32Type)"),
            Ok((CqlType::list(CqlType::Int, false), false))
        );
        // Components of a class-name collection are frozen.
        assert_eq!(
            parse_marshal("ListType(SetType(Int32Type))"),
            Ok((
                CqlType::list(CqlType::set(CqlType::Int, true), false),
                false
            ))
        );
        assert_eq!(
            parse_marshal("com.example.Thing(a,b)"),
            Ok((CqlType::Custom("com.example.Thing(a,b)".into()), false))
        );
        assert_eq!(
            parse_marshal("LexicalUUIDType"),
            Ok((
                CqlType::Custom(format!("{PACKAGE}LexicalUUIDType").into()),
                false
            ))
        );
        for bad in [
            "",
            "ListType(",
            "MapType(Int32Type)",
            "ListType(Int32Type,Int32Type)",
            "VectorType(FloatType, x)",
            "VectorType(FloatType, 0)",
            "UserType(ks,zz)",
            "Int32Type)",
            "UserType(ks,61,62 Int32Type)",
        ] {
            assert!(parse_marshal(bad).is_err(), "{bad}");
        }
        let deep = "ListType(".repeat(100) + &")".repeat(100);
        assert!(parse_marshal(&deep).is_err());
    }
}
