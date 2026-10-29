//! Sorts that come out exactly as Postgres's quicksort's (`pgsort`) would, done faster:
//! when every sort key's values encode as bytes that order as `compare_values` orders them
//! (no USING operator; integers, floats, numerics, dates and times, booleans, text compared
//! as its bytes), the rows are sorted by those bytes with Rust's sort. That gives the
//! quicksort's order whenever no two rows with equal keys differ (how equal, identical rows
//! are arranged can't be seen), which is checked after sorting; when some do, Postgres's
//! sort runs from the rows as they were, comparing their bytes (the same comparisons'
//! outcomes as comparing the values: the same order).

use super::Row;
use crate::sql::plan::SortKey;
use crate::types::Value;
use std::cmp::Ordering;

/// What a key column's values are, for one encoding to cover them all.
#[derive(Clone, Copy, PartialEq)]
enum Class {
    Int,
    Float,
    Bool,
    Date,
    Micros,
    Text,
    Numeric,
}

fn class(v: &Value) -> Option<Class> {
    Some(match v {
        Value::Int2(_) | Value::Int4(_) | Value::Int8(_) | Value::Oid(_) | Value::Char(_) => Class::Int,
        Value::Float4(_) | Value::Float8(_) => Class::Float,
        Value::Bool(_) => Class::Bool,
        Value::Date(_) => Class::Date,
        Value::Time(_) | Value::Timestamp(_) | Value::Timestamptz(_) => Class::Micros,
        Value::Text(s) if !s.as_bytes().contains(&0) => Class::Text,
        Value::Numeric(_) => Class::Numeric,
        _ => return None,
    })
}

/// Append `v`'s order-preserving bytes (ascending; inverted for DESC; NULLs placed as
/// `compare_values` places them whatever the direction).
fn encode(v: &Value, desc: bool, nulls_first: bool, out: &mut Vec<u8>) {
    if v.is_null() {
        out.push(if nulls_first { 0 } else { 2 });
        return;
    }
    out.push(1);
    let start = out.len();
    match v {
        Value::Float4(_) | Value::Float8(_) => {
            let f = v.as_f64().unwrap_or(0.0);
            // (float_cmp: NaN above everything, -0 equal to 0.)
            let bits = if f.is_nan() {
                u64::MAX
            } else {
                let f = if f == 0.0 { 0.0 } else { f };
                let b = f.to_bits();
                if b >> 63 == 1 { !b } else { b | (1 << 63) }
            };
            out.extend_from_slice(&bits.to_be_bytes());
        }
        Value::Bool(b) => out.push(*b as u8),
        Value::Date(d) => out.extend_from_slice(&((*d as u32) ^ (1 << 31)).to_be_bytes()),
        Value::Time(t) | Value::Timestamp(t) | Value::Timestamptz(t) => {
            out.extend_from_slice(&((*t as u64) ^ (1 << 63)).to_be_bytes())
        }
        Value::Text(s) => {
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        }
        Value::Numeric(n) => numeric_key(n, out),
        v => {
            let i = v.as_i64().unwrap_or(0);
            out.extend_from_slice(&((i as u64) ^ (1 << 63)).to_be_bytes());
        }
    }
    if desc {
        for b in &mut out[start..] {
            *b = !*b;
        }
    }
}

/// A numeric's order-preserving bytes, equal for equal values (1.0 and 1.00): -Infinity,
/// negatives, zero, positives, Infinity, NaN; a finite value's decimal exponent then its
/// significant digits (trailing zeros dropped), inverted for negatives.
fn numeric_key(n: &crate::types::numeric::Numeric, out: &mut Vec<u8>) {
    use crate::types::numeric::Numeric as N;
    let (int, scale) = match n {
        N::NegInf => return out.push(0),
        N::PosInf => return out.push(4),
        N::NaN => return out.push(5),
        N::Finite { int, scale } => (int, *scale),
    };
    if int.is_zero() {
        return out.push(2);
    }
    let digits = int.abs().to_string();
    let sig = digits.trim_end_matches('0');
    let exp = digits.len() as i64 - scale as i64;
    let neg = int.is_negative();
    out.push(if neg { 1 } else { 3 });
    let start = out.len();
    out.extend_from_slice(&((exp.clamp(i32::MIN as i64, i32::MAX as i64) as i32 as u32) ^ (1 << 31)).to_be_bytes());
    out.extend_from_slice(sig.as_bytes());
    out.push(0);
    if neg {
        for b in &mut out[start..] {
            *b = !*b;
        }
    }
}

/// Two values the same in every way (not just comparing equal: 1.0 and 1.00 differ).
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int2(x), Value::Int2(y)) => x == y,
        (Value::Int4(x), Value::Int4(y)) => x == y,
        (Value::Int8(x), Value::Int8(y)) => x == y,
        (Value::Oid(x), Value::Oid(y)) => x == y,
        (Value::Char(x), Value::Char(y)) => x == y,
        (Value::Float4(x), Value::Float4(y)) => x.to_bits() == y.to_bits(),
        (Value::Float8(x), Value::Float8(y)) => x.to_bits() == y.to_bits(),
        (Value::Text(x), Value::Text(y)) => x == y,
        (Value::Date(x), Value::Date(y)) => x == y,
        (Value::Time(x), Value::Time(y)) => x == y,
        (Value::Timestamp(x), Value::Timestamp(y)) => x == y,
        (Value::Timestamptz(x), Value::Timestamptz(y)) => x == y,
        (Value::Numeric(x), Value::Numeric(y)) => format!("{x:?}") == format!("{y:?}"),
        _ => false,
    }
}

fn same_row(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_value(x, y))
}

/// The order (a permutation of `0..n`) Postgres's sort of `n` items gives, when it can be
/// told without it: `key(i, k)` is item i's value of sort key k, `row(i)` the whole item.
/// With `bound`, only the first `bound` places are wanted (they come first). None: sort
/// as Postgres does.
pub fn order<'a>(
    n: usize,
    keys: &[SortKey],
    key: impl Fn(usize, usize) -> &'a Value,
    row: impl Fn(usize) -> &'a [Value],
    bound: Option<usize>,
) -> Option<Vec<usize>> {
    if n < 2 || n > u32::MAX as usize || keys.is_empty() || keys.iter().any(|k| k.using.is_some()) {
        return None;
    }
    // Every key's non-NULL values of one class that encodes.
    for k in 0..keys.len() {
        let mut c: Option<Class> = None;
        for i in 0..n {
            let v = key(i, k);
            if v.is_null() {
                continue;
            }
            let vc = class(v)?;
            if c.is_some_and(|c| c != vc) {
                return None;
            }
            c = Some(vc);
        }
    }
    let mut buf: Vec<u8> = Vec::with_capacity(n * 10 * keys.len());
    let mut ends: Vec<usize> = Vec::with_capacity(n);
    for i in 0..n {
        for (k, sk) in keys.iter().enumerate() {
            encode(key(i, k), sk.desc, sk.nulls_first, &mut buf);
        }
        ends.push(buf.len());
    }
    let bytes = |i: u32| {
        let i = i as usize;
        let s = if i == 0 { 0 } else { ends[i - 1] };
        &buf[s..ends[i]]
    };
    let mut idx: Vec<u32> = (0..n as u32).collect();
    let by = |a: &u32, b: &u32| bytes(*a).cmp(bytes(*b));
    let want = match bound {
        Some(b) if b < n => {
            if b == 0 {
                return Some(Vec::new());
            }
            idx.select_nth_unstable_by(b, by);
            // (A tie across the cut: which of the tied rows are kept can be seen.)
            let cut = idx[b];
            idx.truncate(b);
            idx.sort_unstable_by(by);
            if bytes(idx[b - 1]) == bytes(cut) { None } else { Some(b) }
        }
        _ => {
            idx.sort_unstable_by(by);
            Some(n)
        }
    };
    let ties = |idx: &[u32], want: usize| {
        idx[..want].windows(2).any(|w| {
            bytes(w[0]).cmp(bytes(w[1])) == Ordering::Equal && !same_row(row(w[0] as usize), row(w[1] as usize))
        })
    };
    if let Some(want) = want
        && !ties(&idx, want)
    {
        return Some(idx.into_iter().map(|i| i as usize).collect());
    }
    // Rows with equal keys that differ: Postgres's sort, from the rows' order, comparing
    // their bytes.
    let mut f = |a: &u32, b: &u32| Ok(bytes(*a).cmp(bytes(*b)));
    let mut cmp = super::pgsort::Cmp::new(&mut f);
    let sorted = super::pgsort::tuplesort((0..n as u32).collect(), bound, &mut cmp);
    Some(sorted.into_iter().map(|i| i as usize).collect())
}

/// `rows` put in `order` (a permutation, or its first places).
pub fn permute(rows: Vec<Row>, order: &[usize]) -> Vec<Row> {
    let mut slots: Vec<Option<Row>> = rows.into_iter().map(Some).collect();
    order.iter().filter_map(|&i| slots[i].take()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `compare_values` without a USING operator.
    fn compare(a: &Value, b: &Value, desc: bool, nulls_first: bool) -> Ordering {
        match (a.is_null(), b.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if nulls_first { Ordering::Less } else { Ordering::Greater }
            }
            (false, true) => {
                if nulls_first { Ordering::Greater } else { Ordering::Less }
            }
            _ => {
                let o = a.cmp_total(b);
                if desc { o.reverse() } else { o }
            }
        }
    }

    #[test]
    fn encodings_order_as_compare_values() {
        let sets: Vec<Vec<Value>> = vec![
            [i64::MIN, -5, -1, 0, 1, 7, i64::MAX].iter().map(|&i| Value::Int8(i)).collect(),
            [i32::MIN, -3, 0, 2, i32::MAX].iter().map(|&i| Value::Int4(i)).collect(),
            [f64::NEG_INFINITY, -2.5, -0.0, 0.0, 1e-300, 3.0, f64::INFINITY, f64::NAN]
                .iter()
                .map(|&f| Value::Float8(f))
                .collect(),
            [i32::MIN, -1, 0, 1, i32::MAX].iter().map(|&d| Value::Date(d)).collect(),
            [i64::MIN, -1, 0, 1, i64::MAX].iter().map(|&t| Value::Timestamp(t)).collect(),
            ["", "a", "ab", "abc", "b", "\u{e9}", "\u{ff}x"].iter().map(|s| Value::text(*s)).collect(),
            [
                "-Infinity", "-1e30", "-12.5", "-12.50", "-12.4", "-1.2", "-1.23", "-0.001", "0", "0.000", "1e-20",
                "0.001", "0.0010", "1.2", "1.23", "1.230", "9.99", "10", "10.0", "12", "120", "1e30", "Infinity", "NaN",
            ]
            .iter()
            .map(|s| Value::Numeric(std::sync::Arc::new(s.parse::<crate::types::numeric::Numeric>().unwrap())))
            .collect(),
            vec![Value::Bool(false), Value::Bool(true)],
        ];
        for mut set in sets {
            set.push(Value::Null);
            for desc in [false, true] {
                for nf in [false, true] {
                    for a in &set {
                        for b in &set {
                            let (mut x, mut y) = (Vec::new(), Vec::new());
                            encode(a, desc, nf, &mut x);
                            encode(b, desc, nf, &mut y);
                            assert_eq!(x.cmp(&y), compare(a, b, desc, nf), "{a:?} {b:?} desc={desc} nf={nf}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn ties_between_different_rows_are_left_to_postgres_order() {
        let key = |desc| SortKey {
            expr: crate::sql::expr::Expr::Col(0, crate::types::DataType::Int4),
            desc,
            nulls_first: false,
            using: None,
        };
        let rows: Vec<Row> = vec![
            vec![Value::Int4(2), Value::text("x")],
            vec![Value::Int4(1), Value::text("y")],
            vec![Value::Int4(2), Value::text("x")],
        ];
        // Equal keys, the same rows: sorted here.
        let o = order(3, &[key(false)], |i, _| &rows[i][0], |i| &rows[i], None).unwrap();
        assert_eq!(o[0], 1);
        // Equal keys, different rows: Postgres's order (its sort over the bytes, which is
        // its sort over the values).
        let mut rows2 = rows.clone();
        rows2[2][1] = Value::text("z");
        let pg = |keys: &[SortKey], rows: &[Row], bound: Option<usize>| -> Vec<usize> {
            let mut f = |a: &usize, b: &usize| {
                let k = &keys[0];
                let o = rows[*a][0].cmp_total(&rows[*b][0]);
                Ok(if k.desc { o.reverse() } else { o })
            };
            let mut c = super::super::pgsort::Cmp::new(&mut f);
            super::super::pgsort::tuplesort((0..rows.len()).collect(), bound, &mut c)
        };
        for (desc, bound) in [(false, None), (true, Some(1)), (true, Some(2)), (false, Some(2))] {
            let k = [key(desc)];
            assert_eq!(
                order(3, &k, |i, _| &rows2[i][0], |i| &rows2[i], bound).unwrap(),
                pg(&k, &rows2, bound),
                "desc={desc} bound={bound:?}"
            );
        }
        let o = order(3, &[key(false)], |i, _| &rows2[i][0], |i| &rows2[i], Some(1)).unwrap();
        assert_eq!(o, vec![1]);
    }
}
