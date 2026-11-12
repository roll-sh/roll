//! Resolving types as written (`nosql_cql::ast::RawType`) into [`CqlType`]s: user types are looked
//! up in the statement's keyspace, nesting rules are checked with each dialect's message
//! (`CQL3Type.Raw.prepare`).

use std::sync::Arc;

use nosql_cql::ast::{NativeType, RawType as Ast};
use nosql_types::names::RawType;
use nosql_types::{CqlType, Dialect, UdtDef};

use crate::error::{Error, Result};

/// Converts the syntax tree's type into `nosql_types`' raw type (no resolution yet).
pub fn to_raw(t: &Ast, d: Dialect) -> Result<RawType> {
    Ok(match t {
        Ast::Native(n) => RawType::Native(native(*n)),
        Ast::List(e) => RawType::List(Box::new(to_raw(e, d)?), false),
        Ast::Set(e) => RawType::Set(Box::new(to_raw(e, d)?), false),
        Ast::Map(kv) => RawType::Map(
            Box::new(to_raw(&kv.0, d)?),
            Box::new(to_raw(&kv.1, d)?),
            false,
        ),
        Ast::Tuple(ts) => RawType::Tuple(ts.iter().map(|t| to_raw(t, d)).collect::<Result<_>>()?),
        Ast::Vector(v) => {
            let elem = to_raw(&v.0, d)?;
            let dim: i64 = v.1.trim().parse().unwrap_or(-1);
            if dim <= 0 {
                return Err(Error::invalid(match d {
                    Dialect::Scylla => "Vectors must have a dimension greater than 0".to_string(),
                    Dialect::Cassandra => {
                        format!("vectors may only have positive dimensions; given {}", v.1)
                    }
                }));
            }
            // ScyllaDB's limit (the Vector Search maximum).
            if d.is_scylla() && dim > crate::limits::SCYLLA_MAX_VECTOR_DIMENSION {
                return Err(Error::invalid(format!(
                    "Vectors must have a dimension less than or equal to {}",
                    crate::limits::SCYLLA_MAX_VECTOR_DIMENSION
                )));
            }
            if matches!(elem, RawType::Native(CqlType::Counter)) {
                return Err(Error::invalid("vectors may not contain counters"));
            }
            RawType::Vector(Box::new(elem), dim.min(u32::MAX as i64) as u32)
        }
        Ast::Frozen(inner) => to_raw(inner, d)?.freeze().map_err(Error::invalid)?,
        Ast::User(q) => RawType::Udt {
            keyspace: q.keyspace.as_ref().map(|k| k.name().to_string()),
            name: q.name.name().to_string(),
            frozen: false,
        },
        Ast::Custom(c) => RawType::Custom(c.to_string()),
    })
}

pub fn native(n: NativeType) -> CqlType {
    match n {
        NativeType::Ascii => CqlType::Ascii,
        NativeType::Bigint => CqlType::Bigint,
        NativeType::Blob => CqlType::Blob,
        NativeType::Boolean => CqlType::Boolean,
        NativeType::Counter => CqlType::Counter,
        NativeType::Decimal => CqlType::Decimal,
        NativeType::Double => CqlType::Double,
        NativeType::Duration => CqlType::Duration,
        NativeType::Float => CqlType::Float,
        NativeType::Inet => CqlType::Inet,
        NativeType::Int => CqlType::Int,
        NativeType::Smallint => CqlType::Smallint,
        NativeType::Text | NativeType::Varchar => CqlType::Text,
        NativeType::Timestamp => CqlType::Timestamp,
        NativeType::Tinyint => CqlType::Tinyint,
        NativeType::Uuid => CqlType::Uuid,
        NativeType::Varint => CqlType::Varint,
        NativeType::Timeuuid => CqlType::Timeuuid,
        NativeType::Date => CqlType::Date,
        NativeType::Time => CqlType::Time,
    }
}

/// Resolves a type written in a statement on `keyspace`. `types` looks up the keyspace's UDTs.
pub fn resolve(
    t: &Ast,
    keyspace: &str,
    types: &dyn Fn(&str) -> Option<Arc<UdtDef>>,
    d: Dialect,
) -> Result<CqlType> {
    let raw = to_raw(t, d)?;
    raw.prepare(keyspace, &|_ks, name| types(name))
        .map_err(|m| {
            // ScyllaDB names the user types of the type keyspace-qualified.
            let m = match d {
                Dialect::Scylla => {
                    let shown = qualified(&raw, keyspace).to_string();
                    match m.split_once(": ") {
                        Some((head, rest)) if rest == raw.to_string() => format!("{head}: {shown}"),
                        _ => m,
                    }
                }
                Dialect::Cassandra => m,
            };
            Error::invalid(dialect_message(m, d))
        })
}

/// `raw` with every unqualified user type in `keyspace`.
fn qualified(raw: &nosql_types::names::RawType, keyspace: &str) -> nosql_types::names::RawType {
    use nosql_types::names::RawType as R;
    let q = |t: &R| Box::new(qualified(t, keyspace));
    match raw {
        R::List(e, f) => R::List(q(e), *f),
        R::Set(e, f) => R::Set(q(e), *f),
        R::Map(k, v, f) => R::Map(q(k), q(v), *f),
        R::Tuple(ts) => R::Tuple(ts.iter().map(|t| qualified(t, keyspace)).collect()),
        R::Vector(e, n) => R::Vector(q(e), *n),
        R::Udt {
            keyspace: None,
            name,
            frozen,
        } => R::Udt {
            keyspace: Some(keyspace.to_string()),
            name: name.clone(),
            frozen: *frozen,
        },
        other => other.clone(),
    }
}

/// Adjusts `nosql_types`' (Cassandra-worded) type errors to the dialect.
fn dialect_message(m: String, d: Dialect) -> String {
    if d.is_scylla() {
        for prefix in [
            "Non-frozen collections are not allowed inside collections: ",
            "Non-frozen UDTs are not allowed inside collections: ",
        ] {
            if let Some(rest) = m.strip_prefix(prefix) {
                return format!(
                    "Non-frozen user types or collections are not allowed inside collections: {rest}"
                );
            }
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use nosql_cql::ast::{Ident, QualifiedName};

    fn no_types(_: &str) -> Option<Arc<UdtDef>> {
        None
    }

    #[test]
    fn natives_and_collections() {
        let t = Ast::Map(Box::new((
            Ast::Native(NativeType::Varchar),
            Ast::Frozen(Box::new(Ast::List(Box::new(Ast::Native(NativeType::Int))))),
        )));
        let r = resolve(&t, "ks", &no_types, Dialect::Scylla).unwrap();
        assert_eq!(nosql_types::cql_name(&r), "map<text, frozen<list<int>>>");
        let nested = Ast::List(Box::new(Ast::List(Box::new(Ast::Native(NativeType::Int)))));
        assert_eq!(
            resolve(&nested, "ks", &no_types, Dialect::Cassandra)
                .unwrap_err()
                .message,
            "Non-frozen collections are not allowed inside collections: list<list<int>>"
        );
        assert_eq!(
            resolve(&nested, "ks", &no_types, Dialect::Scylla)
                .unwrap_err()
                .message,
            "Non-frozen user types or collections are not allowed inside collections: list<list<int>>"
        );
        let counters = Ast::List(Box::new(Ast::Native(NativeType::Counter)));
        assert_eq!(
            resolve(&counters, "ks", &no_types, Dialect::Scylla)
                .unwrap_err()
                .message,
            "Counters are not allowed inside collections: list<counter>"
        );
        let set_dur = Ast::Set(Box::new(Ast::Native(NativeType::Duration)));
        assert_eq!(
            resolve(&set_dur, "ks", &no_types, Dialect::Cassandra)
                .unwrap_err()
                .message,
            "Durations are not allowed inside sets: set<duration>"
        );
    }

    #[test]
    fn vectors_and_user_types() {
        let v0 = Ast::Vector(Box::new((Ast::Native(NativeType::Float), "0".into())));
        assert_eq!(
            resolve(&v0, "ks", &no_types, Dialect::Scylla)
                .unwrap_err()
                .message,
            "Vectors must have a dimension greater than 0"
        );
        assert_eq!(
            resolve(&v0, "ks", &no_types, Dialect::Cassandra)
                .unwrap_err()
                .message,
            "vectors may only have positive dimensions; given 0"
        );
        let u = Ast::User(QualifiedName {
            keyspace: None,
            name: Ident::unquoted("nosuchtype"),
        });
        assert_eq!(
            resolve(&u, "e", &no_types, Dialect::Scylla)
                .unwrap_err()
                .message,
            "Unknown type e.nosuchtype"
        );
        let other = Ast::User(QualifiedName {
            keyspace: Some(Ident::unquoted("system")),
            name: Ident::unquoted("ty"),
        });
        assert_eq!(
            resolve(&other, "e", &no_types, Dialect::Scylla)
                .unwrap_err()
                .message,
            "Statement on keyspace e cannot refer to a user type in keyspace system; user types can only be used in the keyspace they are defined in"
        );
        let def = Arc::new(UdtDef::new("e", "ty", vec![("x".into(), CqlType::Int)]));
        let found = |n: &str| (n == "ty").then(|| def.clone());
        let t = resolve(
            &Ast::Frozen(Box::new(Ast::User(QualifiedName {
                keyspace: None,
                name: Ident::unquoted("ty"),
            }))),
            "e",
            &found,
            Dialect::Scylla,
        )
        .unwrap();
        assert_eq!(t, CqlType::Udt(def, true));
    }
}
