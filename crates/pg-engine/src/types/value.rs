//! Runtime values. A value doesn't carry its full type; the planner knows the static type of
//! every expression. Comparisons here are the btree orderings Postgres uses.

use super::array::ArrayValue;
use super::json::Jsonb;
use super::numeric::Numeric;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

pub const USECS_PER_DAY: i64 = 86_400_000_000;
pub const USECS_PER_HOUR: i64 = 3_600_000_000;
pub const USECS_PER_MINUTE: i64 = 60_000_000;
pub const USECS_PER_SEC: i64 = 1_000_000;

impl Interval {
    pub fn new(months: i32, days: i32, micros: i64) -> Self {
        Self {
            months,
            days,
            micros,
        }
    }

    /// The span used for comparison: months count as 30 days, days as 24 hours.
    pub fn cmp_key(&self) -> i128 {
        (self.months as i128 * 30 + self.days as i128) * USECS_PER_DAY as i128 + self.micros as i128
    }

    pub fn is_infinite(&self) -> bool {
        self.is_pos_inf() || self.is_neg_inf()
    }

    pub fn is_pos_inf(&self) -> bool {
        self.months == i32::MAX && self.days == i32::MAX && self.micros == i64::MAX
    }

    pub fn is_neg_inf(&self) -> bool {
        self.months == i32::MIN && self.days == i32::MIN && self.micros == i64::MIN
    }

    pub const POS_INF: Interval = Interval {
        months: i32::MAX,
        days: i32::MAX,
        micros: i64::MAX,
    };
    pub const NEG_INF: Interval = Interval {
        months: i32::MIN,
        days: i32::MIN,
        micros: i64::MIN,
    };
}

impl PartialOrd for Interval {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Interval {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cmp_key().cmp(&other.cmp_key())
    }
}

#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Int2(i16),
    Int4(i32),
    Int8(i64),
    Float4(f32),
    Float8(f64),
    Numeric(Arc<Numeric>),
    /// text, varchar, bpchar, name, unknown literals
    Text(Arc<str>),
    Char(u8),
    Bytea(Arc<[u8]>),
    /// Days since 2000-01-01; i32::MIN/MAX are -infinity/infinity.
    Date(i32),
    /// Microseconds since midnight.
    Time(i64),
    /// Microseconds since midnight, and the zone offset in seconds west of UTC (Postgres's sign).
    Timetz(i64, i32),
    /// Microseconds since 2000-01-01 00:00:00; i64::MIN/MAX are -infinity/infinity.
    Timestamp(i64),
    /// Microseconds since 2000-01-01 00:00:00 UTC.
    Timestamptz(i64),
    Interval(Interval),
    Uuid(u128),
    /// json keeps the input text verbatim.
    Json(Arc<str>),
    Jsonb(Arc<Jsonb>),
    /// oid and the reg* types.
    Oid(u32),
    Array(Arc<ArrayValue>),
    /// Anonymous records (`ROW(1, 'a')`).
    Record(Arc<Vec<Value>>),
    /// Bit strings: length in bits and the packed bytes.
    Bits(u32, Arc<[u8]>),
    /// inet/cidr: address bytes (4 or 16) and the netmask length.
    Inet(Arc<[u8]>, u8),
    Macaddr([u8; 6]),
    Macaddr8([u8; 8]),
    /// money, in cents.
    Money(i64),
    PgLsn(u64),
    Xid8(u64),
    /// pg_snapshot and txid_snapshot.
    Snapshot(Arc<super::snapshot::Snapshot>),
    Tid(u32, u16),
    Range(Arc<super::range::Range>),
    /// Normalised: sorted, non-empty, non-overlapping, non-adjacent ranges.
    Multirange(Arc<Vec<super::range::Range>>),
    /// point, line, lseg, box, path, polygon, circle.
    Geo(Arc<super::geo::Geo>),
    TsVector(Arc<super::tsearch::TsVector>),
    TsQuery(Arc<super::tsearch::TsQuery>),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn text(s: impl Into<Arc<str>>) -> Value {
        Value::Text(s.into())
    }

    pub fn numeric(n: Numeric) -> Value {
        Value::Numeric(Arc::new(n))
    }

    pub fn bytea(b: impl Into<Arc<[u8]>>) -> Value {
        Value::Bytea(b.into())
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) | Value::Json(s) => Some(s),
            _ => None,
        }
    }

    /// Integer value of any integer-like variant.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int2(v) => Some(*v as i64),
            Value::Int4(v) => Some(*v as i64),
            Value::Int8(v) => Some(*v),
            Value::Oid(v) => Some(*v as i64),
            Value::Char(c) => Some(*c as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float4(v) => Some(*v as f64),
            Value::Float8(v) => Some(*v),
            Value::Numeric(n) => Some(n.to_f64()),
            v => v.as_i64().map(|i| i as f64),
        }
    }

    pub fn as_array(&self) -> Option<&ArrayValue> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    /// Btree comparison between two non-null values of the same type family. NULLs sort last.
    pub fn cmp_total(&self, other: &Value) -> Ordering {
        use Value::*;
        match (self, other) {
            (Null, Null) => Ordering::Equal,
            (Null, _) => Ordering::Greater,
            (_, Null) => Ordering::Less,
            (Bool(a), Bool(b)) => a.cmp(b),
            (Float4(a), Float4(b)) => float_cmp(*a as f64, *b as f64),
            (Float8(a), Float8(b)) => float_cmp(*a, *b),
            (Float4(_) | Float8(_), _) | (_, Float4(_) | Float8(_)) => {
                match (self.as_f64(), other.as_f64()) {
                    (Some(a), Some(b)) => float_cmp(a, b),
                    _ => self.variant_rank().cmp(&other.variant_rank()),
                }
            }
            (Numeric(a), Numeric(b)) => a.cmp(b),
            (Numeric(a), b) if b.as_i64().is_some() => a
                .as_ref()
                .cmp(&super::Numeric::from_i64(b.as_i64().unwrap())),
            (a, Numeric(b)) if a.as_i64().is_some() => {
                super::Numeric::from_i64(a.as_i64().unwrap()).cmp(b)
            }
            (Text(a), Text(b)) => a.as_bytes().cmp(b.as_bytes()),
            (Char(a), Char(b)) => a.cmp(b),
            (Bytea(a), Bytea(b)) => a.cmp(b),
            (Date(a), Date(b)) => a.cmp(b),
            (Time(a), Time(b)) => a.cmp(b),
            (Timetz(a, za), Timetz(b, zb)) => {
                // Compare by UTC instant, then by zone.
                (a + *za as i64 * 1_000_000)
                    .cmp(&(b + *zb as i64 * 1_000_000))
                    .then(za.cmp(zb))
            }
            (Timestamp(a), Timestamp(b)) | (Timestamptz(a), Timestamptz(b)) => a.cmp(b),
            (Interval(a), Interval(b)) => a.cmp(b),
            (Uuid(a), Uuid(b)) => a.cmp(b),
            (Json(a), Json(b)) => a.cmp(b),
            (Jsonb(a), Jsonb(b)) => a.cmp(b),
            (Array(a), Array(b)) => a.cmp(b),
            (Record(a), Record(b)) => {
                for (x, y) in a.iter().zip(b.iter()) {
                    let c = x.cmp_total(y);
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                a.len().cmp(&b.len())
            }
            (Bits(la, a), Bits(lb, b)) => a.cmp(b).then(la.cmp(lb)),
            (Inet(a, ma), Inet(b, mb)) => inet_cmp(a, *ma, b, *mb),
            (Macaddr(a), Macaddr(b)) => a.cmp(b),
            (Macaddr8(a), Macaddr8(b)) => a.cmp(b),
            (TsVector(a), TsVector(b)) => super::tsearch::tsvector_cmp(a, b),
            (TsQuery(a), TsQuery(b)) => super::tsearch::tsquery_cmp(a, b),
            (Money(a), Money(b)) => a.cmp(b),
            (PgLsn(a), PgLsn(b)) | (Xid8(a), Xid8(b)) => a.cmp(b),
            (Snapshot(a), Snapshot(b)) => a.cmp(b),
            (Tid(a, x), Tid(b, y)) => a.cmp(b).then(x.cmp(y)),
            (Range(a), Range(b)) => super::range::range_cmp(a, b),
            (Multirange(a), Multirange(b)) => super::range::multi_cmp(a, b),
            (Geo(a), Geo(b)) => a
                .kind()
                .cmp(&b.kind())
                .then(a.is_closed_path().cmp(&b.is_closed_path()))
                .then_with(|| {
                    let (x, y) = (a.floats(), b.floats());
                    x.iter()
                        .zip(&y)
                        .map(|(p, q)| p.total_cmp(q))
                        .find(|o| o.is_ne())
                        .unwrap_or(x.len().cmp(&y.len()))
                }),
            (a, b) => match (a.as_i64(), b.as_i64()) {
                (Some(x), Some(y)) => x.cmp(&y),
                _ => a.variant_rank().cmp(&b.variant_rank()),
            },
        }
    }

    fn variant_rank(&self) -> u8 {
        use Value::*;
        match self {
            Null => 255,
            Bool(_) => 1,
            Int2(_) | Int4(_) | Int8(_) | Oid(_) | Char(_) => 2,
            Float4(_) | Float8(_) => 3,
            Numeric(_) => 4,
            Text(_) => 5,
            Bytea(_) => 6,
            Date(_) => 7,
            Time(_) => 8,
            Timetz(..) => 9,
            Timestamp(_) => 10,
            Timestamptz(_) => 11,
            Interval(_) => 12,
            Uuid(_) => 13,
            Json(_) => 14,
            Jsonb(_) => 15,
            Array(_) => 16,
            Record(_) => 17,
            Bits(..) => 18,
            Inet(..) => 19,
            Macaddr(_) => 20,
            Money(_) => 21,
            PgLsn(_) => 22,
            Tid(..) => 23,
            Range(_) => 24,
            Multirange(_) => 25,
            Geo(_) => 26,
            Macaddr8(_) => 27,
            TsVector(_) => 28,
            TsQuery(_) => 29,
            Xid8(_) => 30,
            Snapshot(_) => 31,
        }
    }

    /// SQL equality for grouping/distinct (NULLs are equal to each other).
    pub fn group_eq(&self, other: &Value) -> bool {
        self.cmp_total(other) == Ordering::Equal
    }
}

/// Postgres float ordering: NaN equals NaN and is greater than everything; -0 = 0.
pub fn float_cmp(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.cmp_total(other) == Ordering::Equal
    }
}

impl Eq for Value {}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cmp_total(other)
    }
}

impl Hash for Value {
    /// Consistent with `cmp_total` equality: integers of any width hash alike, numerics are
    /// normalised, -0.0 hashes like 0.0.
    fn hash<H: Hasher>(&self, state: &mut H) {
        use Value::*;
        match self {
            Null => 0u8.hash(state),
            Bool(b) => b.hash(state),
            Int2(_) | Int4(_) | Int8(_) | Oid(_) | Char(_) => {
                2u8.hash(state);
                self.as_i64().unwrap().hash(state);
            }
            Float4(_) | Float8(_) => {
                3u8.hash(state);
                let f = self.as_f64().unwrap();
                let f = if f == 0.0 { 0.0 } else { f };
                if f.is_nan() {
                    u64::MAX.hash(state)
                } else {
                    f.to_bits().hash(state)
                }
            }
            Numeric(n) => {
                4u8.hash(state);
                n.hash(state)
            }
            Text(s) | Json(s) => s.hash(state),
            Bytea(b) => b.hash(state),
            Date(d) => d.hash(state),
            Time(t) | Timestamp(t) | Timestamptz(t) => t.hash(state),
            Timetz(t, z) => (t + *z as i64 * 1_000_000).hash(state),
            Interval(i) => i.cmp_key().hash(state),
            Uuid(u) => u.hash(state),
            Jsonb(j) => hash_jsonb(j, state),
            Array(a) => {
                for v in &a.items {
                    v.hash(state);
                }
            }
            Record(r) => {
                for v in r.iter() {
                    v.hash(state);
                }
            }
            Bits(l, b) => {
                l.hash(state);
                b.hash(state)
            }
            Inet(a, m) => {
                a.hash(state);
                m.hash(state)
            }
            Macaddr(m) => m.hash(state),
            Macaddr8(m) => m.hash(state),
            TsVector(v) => v.hash(state),
            TsQuery(q) => q.hash(state),
            Money(m) => m.hash(state),
            PgLsn(l) | Xid8(l) => l.hash(state),
            Snapshot(s) => s.hash(state),
            Tid(a, b) => {
                a.hash(state);
                b.hash(state)
            }
            Range(r) => hash_range(r, state),
            Geo(g) => {
                g.kind().hash(state);
                g.is_closed_path().hash(state);
                for f in g.floats() {
                    f.to_bits().hash(state);
                }
            }
            Multirange(m) => {
                for r in m.iter() {
                    hash_range(r, state);
                }
            }
        }
    }
}

fn hash_range<H: Hasher>(r: &super::range::Range, state: &mut H) {
    (r.empty, r.lower_inc, r.upper_inc).hash(state);
    r.lower.hash(state);
    r.upper.hash(state);
}

pub type Row = Vec<Value>;

/// Compares the first `n` bits of two addresses (`bitncmp`).
pub fn bitncmp(a: &[u8], b: &[u8], n: usize) -> Ordering {
    let whole = n / 8;
    a[..whole].cmp(&b[..whole]).then_with(|| {
        let rest = n % 8;
        if rest == 0 {
            return Ordering::Equal;
        }
        let m = 0xffu8 << (8 - rest);
        (a[whole] & m).cmp(&(b[whole] & m))
    })
}

/// `network_cmp`: family, then the common network prefix, then the mask, then the whole address.
pub fn inet_cmp(a: &[u8], ma: u8, b: &[u8], mb: u8) -> Ordering {
    a.len()
        .cmp(&b.len())
        .then_with(|| bitncmp(a, b, ma.min(mb) as usize))
        .then(ma.cmp(&mb))
        .then_with(|| a.cmp(b))
}

/// A jsonb value's hash, consistent with its equality (numbers by value: 1.0 = 1).
fn hash_jsonb<H: Hasher>(j: &Jsonb, state: &mut H) {
    match j {
        Jsonb::Null => 0u8.hash(state),
        Jsonb::Bool(b) => {
            1u8.hash(state);
            b.hash(state);
        }
        Jsonb::Number(n) => {
            2u8.hash(state);
            n.hash(state);
        }
        Jsonb::String(s) => {
            3u8.hash(state);
            s.hash(state);
        }
        Jsonb::Array(xs) => {
            4u8.hash(state);
            xs.len().hash(state);
            for x in xs {
                hash_jsonb(x, state);
            }
        }
        Jsonb::Object(m) => {
            5u8.hash(state);
            m.len().hash(state);
            for (k, v) in m {
                k.hash(state);
                hash_jsonb(v, state);
            }
        }
    }
}
