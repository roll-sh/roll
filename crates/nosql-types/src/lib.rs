//! CQL data types, as Apache Cassandra 5.0 and ScyllaDB 2026.3 define them.
//!
//! Values travel and rest in their native-protocol serialisation (`Bytes`); this crate knows
//! what those bytes mean:
//!
//! - [`CqlType`] is a resolved type; [`names`] prints and parses CQL type names
//!   (`frozen<map<text, int>>`), [`marshal`] the Java marshal class names drivers and system
//!   tables use (`org.apache.cassandra.db.marshal.MapType(…)`).
//! - [`compare`] orders serialised values with each type's server comparator, [`validate`]
//!   rejects malformed ones with the reference's message.
//! - [`Value`] is the decoded form ([`decode`] / [`encode`]), used for literals, functions,
//!   casts and formatting.
//! - [`format`] renders values as CQL literals, as `CAST(x AS text)` does, and as cqlsh shows them;
//!   [`json`] implements `SELECT JSON` / `INSERT JSON` / `toJson` / `fromJson`; [`literal`]
//!   turns CQL constants into values.
//! - [`token`] is Murmur3Partitioner, [`timeuuid`] generates and inspects version-1 UUIDs,
//!   [`cast`] is the `CAST` table.
//!
//! Wherever the two products differ, functions take a [`Dialect`]. Behaviour was verified on the
//! reference images; the unit tests record what they print.

pub mod cast;
mod compare;
mod dialect;
pub mod duration;
mod error;
pub mod float;
pub mod format;
pub mod inet;
pub mod json;
pub mod literal;
pub mod marshal;
pub mod names;
pub mod num;
pub mod temporal;
pub mod timeuuid;
pub mod token;
mod types;
mod validate;
mod value;
mod wire;

pub use compare::{compare, compare_timeuuid, compare_uuid, compare_with_order};
pub use dialect::Dialect;
pub use duration::Duration;
pub use error::{MarshalError, Result};
pub use format::{to_cql_literal, to_cqlsh, to_text};
pub use json::{Json, from_json, from_json_text, parse_json, to_json};
pub use literal::{LiteralKind, from_string, literal_to_bytes};
pub use marshal::{marshal_name, parse_marshal};
pub use names::{cql_name, cql_name_in, parse_cql_type, quote_identifier};
pub use num::Decimal;
pub use token::{composite_key, partition_key, split_composite, token};
pub use types::{CqlType, NATIVE_TYPES, UdtDef};
pub use validate::validate;
pub use value::{
    MapEntry, Value, collection_elements, decode, encode, map_entries, pack_collection,
    pack_fields, pack_map, tuple_fields, vector_elements,
};

#[cfg(test)]
mod robustness {
    //! Malformed input never panics, and valid values survive every path.

    use std::sync::Arc;

    use super::*;

    /// A small deterministic generator (no dependency, no large fixtures).
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }

        fn bytes(&mut self, max: usize) -> Vec<u8> {
            let n = (self.next() as usize) % (max + 1);
            (0..n).map(|_| self.next() as u8).collect()
        }
    }

    fn types() -> Vec<CqlType> {
        let udt = Arc::new(UdtDef::new(
            "ks",
            "u",
            vec![("a".into(), CqlType::Int), ("B".into(), CqlType::Text)],
        ));
        let mut v: Vec<CqlType> = NATIVE_TYPES.to_vec();
        v.extend([
            CqlType::list(CqlType::Int, false),
            CqlType::set(CqlType::Text, true),
            CqlType::map(CqlType::Int, CqlType::Duration, false),
            CqlType::map(CqlType::list(CqlType::Int, true), CqlType::Blob, true),
            CqlType::Tuple(vec![CqlType::Int, CqlType::Text, CqlType::Decimal]),
            CqlType::Udt(udt.clone(), true),
            CqlType::Udt(udt, false),
            CqlType::vector(CqlType::Float, 3),
            CqlType::vector(CqlType::Varint, 2),
            CqlType::Custom("x.Y".into()),
        ]);
        v
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut r = Lcg(42);
        let tys = types();
        for i in 0..6000 {
            let ty = &tys[i % tys.len()];
            let a = r.bytes(40);
            let b = r.bytes(40);
            for d in [Dialect::Cassandra, Dialect::Scylla] {
                let _ = validate(ty, &a, d);
                let _ = to_text(ty, &a, d);
                let _ = to_cql_literal(ty, &a, d);
                let _ = to_json(ty, &a, d);
                let _ = cast::cast(&a, ty, &CqlType::Text, d);
                let _ = cast::cast(&a, ty, &CqlType::Bigint, d);
            }
            let _ = decode(ty, &a);
            let _ = to_cqlsh(ty, &a);
            let o = compare(ty, &a, &b);
            assert_eq!(o.reverse(), compare(ty, &b, &a), "{ty:?} {a:02x?} {b:02x?}");
            let _ = token(&a);
            let _ = split_composite(&a);
        }
    }

    #[test]
    fn random_text_never_panics() {
        let mut r = Lcg(7);
        let tys = types();
        let alphabet = b"0123456789-+:.eE TZ[]{}\",'xPYMDWHSmsunao\\";
        for i in 0..4000 {
            let n = (r.next() % 24) as usize;
            let s: String = (0..n)
                .map(|_| alphabet[(r.next() as usize) % alphabet.len()] as char)
                .collect();
            let ty = &tys[i % tys.len()];
            for d in [Dialect::Cassandra, Dialect::Scylla] {
                let _ = from_string(ty, &s, d);
                let _ = from_json_text(ty, &s, d);
                let _ = parse_json(&s, d);
                let _ = Duration::parse(&s, d);
                let _ = temporal::parse_timestamp(&s, d);
                let _ = temporal::parse_date(&s, d);
                let _ = temporal::parse_time(&s, d);
                let _ = inet::parse_inet(&s, d);
            }
            let _ = parse_marshal(&s);
            let _ = names::parse_raw_type(&s);
            let _ = num::Decimal::parse(&s);
            let _ = float::parse_java_double(&s);
        }
    }

    #[test]
    fn valid_values_round_trip_everywhere() {
        let udt = Arc::new(UdtDef::new(
            "ks",
            "u",
            vec![("a".into(), CqlType::Int), ("B".into(), CqlType::Text)],
        ));
        let cases: Vec<(CqlType, Value)> = vec![
            (CqlType::Int, Value::Int(-7)),
            (CqlType::Text, Value::Text("q'\"".into())),
            (CqlType::Timestamp, Value::Timestamp(1_792_497_600_123)),
            (CqlType::Uuid, Value::Uuid([0x55; 16])),
            (
                CqlType::list(CqlType::Int, false),
                Value::List(vec![Value::Int(1), Value::Int(2)]),
            ),
            (
                CqlType::map(CqlType::Text, CqlType::Double, true),
                Value::Map(vec![(Value::Text("k".into()), Value::Double(0.5))]),
            ),
            (
                CqlType::Tuple(vec![CqlType::Int, CqlType::Text]),
                Value::Tuple(vec![Value::Int(1), Value::Text("x".into())]),
            ),
            (
                CqlType::Udt(udt, true),
                Value::Udt(vec![Value::Int(1), Value::Text("y".into())]),
            ),
            (
                CqlType::vector(CqlType::Text, 2),
                Value::Vector(vec![Value::Text("a".into()), Value::Text("bc".into())]),
            ),
            (CqlType::Duration, Value::Duration(Duration::new(1, 2, 3))),
        ];
        for (ty, v) in cases {
            let b = encode(&v, &ty);
            for d in [Dialect::Cassandra, Dialect::Scylla] {
                validate(&ty, &b, d).unwrap_or_else(|e| panic!("{ty:?}: {e}"));
                let j = to_json(&ty, &b, d).unwrap();
                let back = from_json_text(&ty, &j, d).unwrap_or_else(|e| panic!("{ty:?} {j}: {e}"));
                assert_eq!(
                    compare(&ty, &back, &b),
                    std::cmp::Ordering::Equal,
                    "{ty:?} {j}"
                );
            }
            assert_eq!(decode(&ty, &b).unwrap(), v);
        }
    }
}
