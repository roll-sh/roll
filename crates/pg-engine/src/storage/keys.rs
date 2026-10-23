//! Order-preserving key encoding: comparing encoded bytes gives the btree order of the values.
//! NULL sorts last (as in Postgres's default `NULLS LAST` for ascending order); a descending
//! column inverts every byte of its encoding, which also puts NULLs first, matching `DESC`.

use crate::types::Value;
use crate::types::numeric::Numeric;
use num_traits::Signed;

const NULL: u8 = 0xFF;
const BOOL: u8 = 0x10;
const INT: u8 = 0x20;
const FLOAT: u8 = 0x30;
const NUM_NEG_INF: u8 = 0x40;
const NUM_NEG: u8 = 0x41;
const NUM_ZERO: u8 = 0x42;
const NUM_POS: u8 = 0x43;
const NUM_POS_INF: u8 = 0x44;
const NUM_NAN: u8 = 0x45;
const BYTES: u8 = 0x50;
const INTERVAL: u8 = 0x70;
const UUID: u8 = 0x80;
const OTHER: u8 = 0x90;
const ARRAY: u8 = 0xA0;
const ARRAY_END: u8 = 0x01;
const RANGE: u8 = 0xB0;
const MULTIRANGE: u8 = 0xB1;
const GEO: u8 = 0xB2;
const TSVECTOR: u8 = 0xB3;
const TSQUERY: u8 = 0xB4;

pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_i64(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(&((v as u64) ^ (1 << 63)).to_be_bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    for &c in b {
        if c == 0 {
            out.extend_from_slice(&[0x00, 0xFF]);
        } else {
            out.push(c);
        }
    }
    out.extend_from_slice(&[0x00, 0x01]);
}

fn put_f64(out: &mut Vec<u8>, f: f64) {
    let f = if f == 0.0 { 0.0 } else { f };
    let bits = if f.is_nan() {
        u64::MAX
    } else {
        let b = f.to_bits();
        if b & (1 << 63) != 0 {
            !b
        } else {
            b | (1 << 63)
        }
    };
    out.extend_from_slice(&bits.to_be_bytes());
}

fn put_numeric(out: &mut Vec<u8>, n: &Numeric) {
    match n {
        Numeric::NaN => out.push(NUM_NAN),
        Numeric::PosInf => out.push(NUM_POS_INF),
        Numeric::NegInf => out.push(NUM_NEG_INF),
        Numeric::Finite { int, scale } => {
            if int.sign() == num_bigint::Sign::NoSign {
                out.push(NUM_ZERO);
                return;
            }
            let neg = int.is_negative();
            let digits = int.abs().to_string();
            let digits = digits.trim_end_matches('0');
            // Exponent of the leading digit.
            let exp = int.abs().to_string().len() as i64 - 1 - *scale as i64;
            let start = out.len();
            out.push(if neg { NUM_NEG } else { NUM_POS });
            out.extend_from_slice(&((exp as i32 as u32) ^ (1 << 31)).to_be_bytes());
            for d in digits.bytes() {
                out.push(d - b'0' + 1);
            }
            out.push(0);
            if neg {
                for b in &mut out[start + 1..] {
                    *b = !*b;
                }
            }
        }
    }
}

thread_local! {
    /// Encode index keys as before index key format 1 (`Catalog::index_key_format`), which
    /// put a NULL where the column's direction alone says: for testing the upgrade.
    static LEGACY_INDEX_KEYS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` encoding index keys the legacy way (format 0), on this thread.
#[doc(hidden)]
pub fn with_legacy_index_keys<R>(f: impl FnOnce() -> R) -> R {
    let _g = crate::scoped::set(&LEGACY_INDEX_KEYS, true, "keys::LEGACY_INDEX_KEYS");
    f()
}

/// The index key format this build writes (`Catalog::index_key_format`): 1 places a NULL
/// as its column's NULLS FIRST / LAST says.
pub const INDEX_KEY_FORMAT: u32 = 1;

thread_local! {
    /// Store tables with a primary key the way builds before storage format 1
    /// (`Catalog::storage_format`) did, rows keyed by the key's values (a deferrable one's
    /// too): for testing the upgrade.
    static LEGACY_PK_STORAGE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` storing new primary-keyed tables the legacy way (format 0), on this thread.
#[doc(hidden)]
pub fn with_legacy_pk_storage<R>(f: impl FnOnce() -> R) -> R {
    let _g = crate::scoped::set(&LEGACY_PK_STORAGE, true, "keys::LEGACY_PK_STORAGE");
    f()
}

/// The table storage format this build writes (`Catalog::storage_format`): 1 stores every
/// table's rows under a hidden row id (heap order), a primary key being an index of its own.
pub const STORAGE_FORMAT: u32 = 1;

/// Whether a new primary key keys its table's rows (index id 0) rather than having entries
/// of its own: only in the legacy layout.
pub fn primary_key_keys_rows() -> bool {
    LEGACY_PK_STORAGE.with(|l| l.get())
}

/// An index column's value: as `encode_value`, a NULL placed first or last in key order as
/// the column's NULLS FIRST / LAST says (the plain encoding puts it last ascending, first
/// descending).
pub fn encode_index_value(out: &mut Vec<u8>, v: &Value, desc: bool, nulls_first: bool) {
    if v.is_null() && nulls_first != desc && !LEGACY_INDEX_KEYS.with(|l| l.get()) {
        out.push(if desc { NULL } else { !NULL });
        return;
    }
    encode_value(out, v, desc)
}

/// Append the ordered encoding of `v`.
pub fn encode_value(out: &mut Vec<u8>, v: &Value, desc: bool) {
    let start = out.len();
    match v {
        Value::Null => out.push(NULL),
        Value::Bool(b) => {
            out.push(BOOL);
            out.push(*b as u8);
        }
        Value::Int2(_) | Value::Int4(_) | Value::Int8(_) | Value::Oid(_) | Value::Char(_) => {
            out.push(INT);
            put_i64(out, v.as_i64().unwrap());
        }
        Value::Date(d) => {
            out.push(INT);
            put_i64(out, *d as i64);
        }
        Value::Time(t) | Value::Timestamp(t) | Value::Timestamptz(t) | Value::Money(t) => {
            out.push(INT);
            put_i64(out, *t);
        }
        Value::Timetz(t, z) => {
            out.push(INT);
            put_i64(out, t + *z as i64 * 1_000_000);
            put_i64(out, *z as i64);
        }
        Value::PgLsn(l) | Value::Xid8(l) => {
            out.push(INT);
            put_u64(out, *l);
        }
        Value::Snapshot(s) => {
            out.push(BYTES);
            put_bytes(out, s.format().as_bytes());
        }
        Value::Float4(_) | Value::Float8(_) => {
            out.push(FLOAT);
            put_f64(out, v.as_f64().unwrap());
        }
        Value::Numeric(n) => put_numeric(out, n),
        Value::Text(s) | Value::Json(s) => {
            out.push(BYTES);
            put_bytes(out, s.as_bytes());
        }
        Value::Bytea(b) => {
            out.push(BYTES);
            put_bytes(out, b);
        }
        Value::Interval(i) => {
            out.push(INTERVAL);
            let k = i.cmp_key();
            out.extend_from_slice(&((k as u128) ^ (1 << 127)).to_be_bytes());
        }
        Value::Uuid(u) => {
            out.push(UUID);
            out.extend_from_slice(&u.to_be_bytes());
        }
        Value::Jsonb(j) => {
            out.push(OTHER);
            put_bytes(out, j.to_string().as_bytes());
        }
        Value::Array(a) => {
            out.push(ARRAY);
            for item in &a.items {
                encode_value(out, item, false);
            }
            out.push(ARRAY_END);
        }
        Value::Record(items) => {
            out.push(ARRAY);
            for item in items.iter() {
                encode_value(out, item, false);
            }
            out.push(ARRAY_END);
        }
        Value::Bits(len, b) => {
            out.push(BYTES);
            put_bytes(out, b);
            put_u32(out, *len);
        }
        // Postgres's order: family, the prefix bits (a shorter prefix sorts first on a tie),
        // then the whole address. Prefix bits are 2-bit symbols 01/10 ended by 00.
        Value::Inet(a, m) => {
            out.push(BYTES);
            out.push(a.len() as u8);
            let bit = |i: usize| a[i / 8] & (0x80 >> (i % 8)) != 0;
            let syms: Vec<u8> = (0..*m as usize)
                .map(|i| if bit(i) { 2 } else { 1 })
                .chain([0])
                .collect();
            for chunk in syms.chunks(4) {
                out.push(
                    chunk
                        .iter()
                        .enumerate()
                        .fold(0, |b, (j, s)| b | s << (6 - 2 * j)),
                );
            }
            out.extend_from_slice(a);
        }
        Value::Macaddr(m) => {
            out.push(BYTES);
            out.extend_from_slice(m);
        }
        Value::Macaddr8(m) => {
            out.push(BYTES);
            out.extend_from_slice(m);
        }
        Value::Tid(b, o) => {
            out.push(INT);
            put_u64(out, ((*b as u64) << 16) | *o as u64);
        }
        Value::Range(r) => encode_range(out, r),
        Value::Multirange(m) => {
            out.push(MULTIRANGE);
            for r in m.iter() {
                encode_range(out, r);
            }
            out.push(ARRAY_END);
        }
        Value::TsVector(v) => {
            out.push(TSVECTOR);
            crate::types::tsearch::tsvector_key(v, out);
        }
        Value::TsQuery(q) => {
            out.push(TSQUERY);
            crate::types::tsearch::tsquery_key(q, out);
        }
        // Geometric values have no btree order: any stable one will do.
        Value::Geo(g) => {
            out.push(GEO);
            out.push(g.kind() * 2 + g.is_closed_path() as u8);
            for f in g.floats() {
                put_f64(out, f);
            }
        }
    }
    if desc {
        for b in &mut out[start..] {
            *b = !*b;
        }
    }
}

/// Ranges order as `range_cmp`: empty first, then by lower bound (unbounded, then inclusive
/// before exclusive at the same value), then by upper bound (exclusive before inclusive,
/// unbounded last).
fn encode_range(out: &mut Vec<u8>, r: &crate::types::range::Range) {
    out.push(RANGE);
    if r.empty {
        out.push(0);
        return;
    }
    out.push(1);
    match &r.lower {
        None => out.push(0),
        Some(v) => {
            out.push(1);
            encode_value(out, v, false);
            out.push(if r.lower_inc { 0 } else { 1 });
        }
    }
    match &r.upper {
        Some(v) => {
            out.push(0);
            encode_value(out, v, false);
            out.push(if r.upper_inc { 1 } else { 0 });
        }
        None => out.push(1),
    }
}

/// Key prefix for an index of a table: `table_oid | index_id`.
pub fn index_prefix(table: u32, index: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(32);
    put_u32(&mut k, table);
    put_u32(&mut k, index);
    k
}

/// The smallest key greater than every key starting with `prefix`.
pub fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.last_mut() {
        if *last < 0xFF {
            *last += 1;
            return end;
        }
        end.pop();
    }
    vec![0xFF; prefix.len() + 1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    fn enc(v: &Value) -> Vec<u8> {
        let mut o = Vec::new();
        encode_value(&mut o, v, false);
        o
    }

    #[test]
    fn order_matches_values() {
        let nums = [
            "-1e10", "-12.5", "-12.25", "-1", "-0.5", "0", "0.001", "0.5", "1", "1.2", "1.25",
            "12", "1e10",
        ];
        let vals: Vec<Value> = nums
            .iter()
            .map(|s| Value::numeric(s.parse().unwrap()))
            .collect();
        for w in vals.windows(2) {
            assert_eq!(
                enc(&w[0]).cmp(&enc(&w[1])),
                Ordering::Less,
                "{:?} < {:?}",
                w[0],
                w[1]
            );
        }
        assert_eq!(
            enc(&Value::numeric("1.50".parse().unwrap())),
            enc(&Value::numeric("1.5".parse().unwrap()))
        );
        let ints = [i64::MIN, -5, 0, 7, i64::MAX];
        for w in ints.windows(2) {
            assert!(enc(&Value::Int8(w[0])) < enc(&Value::Int8(w[1])));
        }
        let floats = [
            f64::NEG_INFINITY,
            -1.5,
            -0.0,
            0.0,
            1e-300,
            2.0,
            f64::INFINITY,
            f64::NAN,
        ];
        for w in floats.windows(2) {
            assert!(enc(&Value::Float8(w[0])) <= enc(&Value::Float8(w[1])));
        }
        let strs = ["", "a", "a\0", "ab", "b"];
        for w in strs.windows(2) {
            assert!(enc(&Value::text(w[0])) < enc(&Value::text(w[1])));
        }
        assert!(enc(&Value::Int4(5)) < enc(&Value::Null));
        // Composite keys: field boundaries are unambiguous.
        let mut a = enc(&Value::text("a"));
        a.extend(enc(&Value::text("b")));
        let mut b = enc(&Value::text("ab"));
        b.extend(enc(&Value::text("")));
        assert!(a < b);
    }
}
