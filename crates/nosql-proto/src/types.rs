//! Column types as the protocol describes them (`[option]`s in result metadata).
//!
//! This is the wire view, independent of `nosql-types`: the engine converts its resolved types
//! into [`ColType`]. Types newer than the connection's version go out as `Custom` with
//! Cassandra's marshal class name, as `DataType.fromType` does (`date` on v3 is
//! `org.apache.cassandra.db.marshal.SimpleDateType`, `duration` before v5 is
//! `…DurationType`). Vectors are always custom:
//! `org.apache.cassandra.db.marshal.VectorType(org.apache.cassandra.db.marshal.FloatType,3)`.

use bytes::{BufMut, BytesMut};

use crate::error::DecodeError;
use crate::version::{Ctx, Dialect, Version};
use crate::wire::{PutExt, Reader};

/// Deepest type nesting accepted from a peer (each level is another `[option]`).
pub const MAX_TYPE_DEPTH: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ColType {
    /// 0x0000: a marshal class name.
    Custom(String),
    Ascii,
    Bigint,
    Blob,
    Boolean,
    Counter,
    Decimal,
    Double,
    Float,
    Int,
    Timestamp,
    Uuid,
    /// 0x000D (`text` / `varchar`; 0x000A, the v1 `text` id, decodes to this too).
    Varchar,
    Varint,
    Timeuuid,
    Inet,
    /// v4+
    Date,
    /// v4+
    Time,
    /// v4+
    Smallint,
    /// v4+
    Tinyint,
    /// v5+
    Duration,
    List(Box<ColType>),
    Map(Box<ColType>, Box<ColType>),
    Set(Box<ColType>),
    Udt { keyspace: String, name: String, fields: Vec<(String, ColType)> },
    Tuple(Vec<ColType>),
}

const MARSHAL: &str = "org.apache.cassandra.db.marshal.";

impl ColType {
    pub fn list(t: ColType) -> Self {
        ColType::List(Box::new(t))
    }

    pub fn set(t: ColType) -> Self {
        ColType::Set(Box::new(t))
    }

    pub fn map(k: ColType, v: ColType) -> Self {
        ColType::Map(Box::new(k), Box::new(v))
    }

    /// A `vector<elem, dim>` column, as the custom type Cassandra sends.
    pub fn vector(elem_class: &str, dim: u32) -> Self {
        ColType::Custom(format!("{MARSHAL}VectorType({elem_class},{dim})"))
    }

    /// The option id.
    pub fn id(&self) -> u16 {
        match self {
            ColType::Custom(_) => 0x0000,
            ColType::Ascii => 0x0001,
            ColType::Bigint => 0x0002,
            ColType::Blob => 0x0003,
            ColType::Boolean => 0x0004,
            ColType::Counter => 0x0005,
            ColType::Decimal => 0x0006,
            ColType::Double => 0x0007,
            ColType::Float => 0x0008,
            ColType::Int => 0x0009,
            ColType::Timestamp => 0x000B,
            ColType::Uuid => 0x000C,
            ColType::Varchar => 0x000D,
            ColType::Varint => 0x000E,
            ColType::Timeuuid => 0x000F,
            ColType::Inet => 0x0010,
            ColType::Date => 0x0011,
            ColType::Time => 0x0012,
            ColType::Smallint => 0x0013,
            ColType::Tinyint => 0x0014,
            ColType::Duration => 0x0015,
            ColType::List(_) => 0x0020,
            ColType::Map(..) => 0x0021,
            ColType::Set(_) => 0x0022,
            ColType::Udt { .. } => 0x0030,
            ColType::Tuple(_) => 0x0031,
        }
    }

    /// The first version with a native id for this type; older versions get `Custom`.
    fn since(&self) -> Version {
        match self {
            ColType::Date | ColType::Time | ColType::Smallint | ColType::Tinyint => Version::V4,
            ColType::Duration => Version::V5,
            _ => Version::V3,
        }
    }

    /// The marshal class of a native type (`org.apache.cassandra.db.marshal.Int32Type`).
    pub fn class_name(&self) -> Option<String> {
        let c = match self {
            ColType::Custom(c) => return Some(c.clone()),
            ColType::Ascii => "AsciiType",
            ColType::Bigint => "LongType",
            ColType::Blob => "BytesType",
            ColType::Boolean => "BooleanType",
            ColType::Counter => "CounterColumnType",
            ColType::Decimal => "DecimalType",
            ColType::Double => "DoubleType",
            ColType::Float => "FloatType",
            ColType::Int => "Int32Type",
            ColType::Timestamp => "TimestampType",
            ColType::Uuid => "UUIDType",
            ColType::Varchar => "UTF8Type",
            ColType::Varint => "IntegerType",
            ColType::Timeuuid => "TimeUUIDType",
            ColType::Inet => "InetAddressType",
            ColType::Date => "SimpleDateType",
            ColType::Time => "TimeType",
            ColType::Smallint => "ShortType",
            ColType::Tinyint => "ByteType",
            ColType::Duration => "DurationType",
            _ => return None,
        };
        Some(format!("{MARSHAL}{c}"))
    }

    /// Writes the `[option]`. Cassandra sends types newer than the connection's version as
    /// `Custom`; ScyllaDB sends native ids on every version (observed on v3: `date`, `time`,
    /// `smallint`, `tinyint`, `duration`).
    pub fn encode(&self, ctx: impl Into<Ctx>, out: &mut BytesMut) {
        let ctx = ctx.into();
        if ctx.dialect == Dialect::Cassandra && ctx.version < self.since() {
            out.put_u16(0);
            out.put_string(&self.class_name().unwrap_or_default());
            return;
        }
        out.put_u16(self.id());
        match self {
            ColType::Custom(c) => out.put_string(c),
            ColType::List(t) | ColType::Set(t) => t.encode(ctx, out),
            ColType::Map(k, v) => {
                k.encode(ctx, out);
                v.encode(ctx, out);
            }
            ColType::Udt { keyspace, name, fields } => {
                out.put_string(keyspace);
                out.put_string(name);
                out.put_u16(fields.len() as u16);
                for (n, t) in fields {
                    out.put_string(n);
                    t.encode(ctx, out);
                }
            }
            ColType::Tuple(ts) => {
                out.put_u16(ts.len() as u16);
                for t in ts {
                    t.encode(ctx, out);
                }
            }
            _ => {}
        }
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Self::decode_at(r, 0)
    }

    fn decode_at(r: &mut Reader, depth: usize) -> Result<Self, DecodeError> {
        if depth >= MAX_TYPE_DEPTH {
            return Err(DecodeError::invalid("Type nesting is too deep"));
        }
        let id = r.u16()?;
        let next = |r: &mut Reader| Self::decode_at(r, depth + 1).map(Box::new);
        Ok(match id {
            0x0000 => ColType::Custom(r.string()?),
            0x0001 => ColType::Ascii,
            0x0002 => ColType::Bigint,
            0x0003 => ColType::Blob,
            0x0004 => ColType::Boolean,
            0x0005 => ColType::Counter,
            0x0006 => ColType::Decimal,
            0x0007 => ColType::Double,
            0x0008 => ColType::Float,
            0x0009 => ColType::Int,
            0x000A | 0x000D => ColType::Varchar,
            0x000B => ColType::Timestamp,
            0x000C => ColType::Uuid,
            0x000E => ColType::Varint,
            0x000F => ColType::Timeuuid,
            0x0010 => ColType::Inet,
            0x0011 => ColType::Date,
            0x0012 => ColType::Time,
            0x0013 => ColType::Smallint,
            0x0014 => ColType::Tinyint,
            0x0015 => ColType::Duration,
            0x0020 => ColType::List(next(r)?),
            0x0021 => {
                let k = next(r)?;
                ColType::Map(k, next(r)?)
            }
            0x0022 => ColType::Set(next(r)?),
            0x0030 => {
                let keyspace = r.string()?;
                let name = r.string()?;
                let n = r.u16()? as usize;
                // A field is at least a 2-byte name and a 2-byte id.
                let mut fields = Vec::with_capacity(n.min(r.remaining() / 4));
                for _ in 0..n {
                    let f = r.string()?;
                    fields.push((f, *next(r)?));
                }
                ColType::Udt { keyspace, name, fields }
            }
            0x0031 => {
                let n = r.u16()? as usize;
                let mut ts = Vec::with_capacity(n.min(r.remaining() / 2));
                for _ in 0..n {
                    ts.push(*next(r)?);
                }
                ColType::Tuple(ts)
            }
            other => return Err(DecodeError::invalid(format!("Unknown type id {other}"))),
        })
    }
}

/// One column of result metadata or one bind marker of prepared metadata.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ColSpec {
    pub keyspace: String,
    pub table: String,
    pub name: String,
    pub ty: ColType,
}

impl ColSpec {
    pub fn new(keyspace: impl Into<String>, table: impl Into<String>, name: impl Into<String>, ty: ColType) -> Self {
        ColSpec { keyspace: keyspace.into(), table: table.into(), name: name.into(), ty }
    }
}

/// Whether every spec names the same table (Cassandra then sets `Global_tables_spec`).
pub fn same_table(cols: &[ColSpec]) -> bool {
    match cols.split_first() {
        Some((first, rest)) => rest.iter().all(|c| c.keyspace == first.keyspace && c.table == first.table),
        None => false,
    }
}

/// Writes column specs (with or without per-column table names).
pub(crate) fn put_specs(out: &mut BytesMut, ctx: Ctx, global: bool, cols: &[ColSpec]) {
    if global && let Some(first) = cols.first() {
        out.put_string(&first.keyspace);
        out.put_string(&first.table);
    }
    for c in cols {
        if !global {
            out.put_string(&c.keyspace);
            out.put_string(&c.table);
        }
        out.put_string(&c.name);
        c.ty.encode(ctx, out);
    }
}

pub(crate) fn read_specs(r: &mut Reader, global: bool, count: usize) -> Result<Vec<ColSpec>, DecodeError> {
    let (gks, gtable) = if global && count > 0 { (r.string()?, r.string()?) } else { Default::default() };
    // A spec is at least a 2-byte name and a 2-byte type id.
    let mut cols = Vec::with_capacity(count.min(r.remaining() / 4));
    for _ in 0..count {
        let (keyspace, table) = if global { (gks.clone(), gtable.clone()) } else { (r.string()?, r.string()?) };
        let name = r.string()?;
        cols.push(ColSpec { keyspace, table, name, ty: ColType::decode(r)? });
    }
    Ok(cols)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn round(t: &ColType, v: Version) -> ColType {
        let mut b = BytesMut::new();
        t.encode(v, &mut b);
        let mut r = Reader::new(b.freeze());
        let back = ColType::decode(&mut r).unwrap();
        assert!(r.is_empty());
        back
    }

    fn samples() -> Vec<ColType> {
        use ColType::*;
        let mut v = vec![
            Ascii, Bigint, Blob, Boolean, Counter, Decimal, Double, Float, Int, Timestamp, Uuid, Varchar, Varint, Timeuuid,
            Inet, Date, Time, Smallint, Tinyint, Duration,
        ];
        v.push(ColType::list(Int));
        v.push(ColType::set(Varchar));
        v.push(ColType::map(Varchar, ColType::list(Date)));
        v.push(Udt { keyspace: "ks".into(), name: "addr".into(), fields: vec![("street".into(), Varchar), ("n".into(), Smallint)] });
        v.push(Tuple(vec![Int, Duration]));
        v.push(ColType::vector("org.apache.cassandra.db.marshal.FloatType", 3));
        v
    }

    #[test]
    fn round_trips_on_v5() {
        for t in samples() {
            assert_eq!(round(&t, Version::V5), t);
        }
    }

    #[test]
    fn newer_types_become_custom() {
        assert_eq!(round(&ColType::Date, Version::V3), ColType::Custom("org.apache.cassandra.db.marshal.SimpleDateType".into()));
        assert_eq!(round(&ColType::Tinyint, Version::V3), ColType::Custom("org.apache.cassandra.db.marshal.ByteType".into()));
        assert_eq!(round(&ColType::Duration, Version::V4), ColType::Custom("org.apache.cassandra.db.marshal.DurationType".into()));
        assert_eq!(round(&ColType::Date, Version::V4), ColType::Date);
        // ScyllaDB never downgrades.
        let scylla = Ctx::new(Version::V3).dialect(Dialect::Scylla);
        for t in [ColType::Date, ColType::Time, ColType::Smallint, ColType::Tinyint, ColType::Duration] {
            let mut b = BytesMut::new();
            t.encode(scylla, &mut b);
            assert_eq!(ColType::decode(&mut Reader::new(b.freeze())).unwrap(), t);
        }
        assert_eq!(
            round(&ColType::list(ColType::Time), Version::V3),
            ColType::list(ColType::Custom("org.apache.cassandra.db.marshal.TimeType".into()))
        );
    }

    #[test]
    fn class_names() {
        for t in samples() {
            let simple = !matches!(t, ColType::List(_) | ColType::Set(_) | ColType::Map(..) | ColType::Udt { .. } | ColType::Tuple(_));
            assert_eq!(t.class_name().is_some(), simple, "{t:?}");
            if let Some(c) = t.class_name() {
                assert!(c.starts_with("org.apache.cassandra.db.marshal."), "{c}");
            }
        }
        assert_eq!(ColType::Int.class_name().unwrap(), "org.apache.cassandra.db.marshal.Int32Type");
        // The v1 text id decodes as varchar.
        assert_eq!(ColType::decode(&mut Reader::new(Bytes::from_static(b"\x00\x0a"))).unwrap(), ColType::Varchar);
    }

    #[test]
    fn vector_class() {
        assert_eq!(
            ColType::vector("org.apache.cassandra.db.marshal.FloatType", 3),
            ColType::Custom("org.apache.cassandra.db.marshal.VectorType(org.apache.cassandra.db.marshal.FloatType,3)".into())
        );
    }

    #[test]
    fn malformed_types() {
        let dec = |b: &'static [u8]| ColType::decode(&mut Reader::new(Bytes::from_static(b)));
        assert!(matches!(dec(b"\x00\x99"), Err(DecodeError::Invalid(_))));
        assert!(matches!(dec(b"\x00\x20"), Err(DecodeError::Truncated { .. })));
        assert!(matches!(dec(b"\x00\x31\xff\xff"), Err(DecodeError::Truncated { .. })));
        assert!(matches!(dec(b"\x00\x30\x00\x00\x00\x00\xff\xff"), Err(DecodeError::Truncated { .. })));
        // list<list<…>> nested far beyond the cap: rejected without recursing that deep.
        let deep: Vec<u8> = std::iter::repeat_n([0x00, 0x20], 100_000).flatten().collect();
        assert!(matches!(ColType::decode(&mut Reader::new(Bytes::from(deep))), Err(DecodeError::Invalid(_))));
    }

    #[test]
    fn specs() {
        let cols = vec![ColSpec::new("ks", "t", "a", ColType::Int), ColSpec::new("ks", "t", "b", ColType::Varchar)];
        assert!(same_table(&cols));
        for global in [true, false] {
            let mut b = BytesMut::new();
            put_specs(&mut b, Ctx::new(Version::V4), global, &cols);
            let mut r = Reader::new(b.freeze());
            assert_eq!(read_specs(&mut r, global, 2).unwrap(), cols);
            assert!(r.is_empty());
        }
        assert!(!same_table(&[]));
        assert!(matches!(
            read_specs(&mut Reader::new(Bytes::new()), false, 1_000_000),
            Err(DecodeError::Truncated { .. })
        ));
    }
}
