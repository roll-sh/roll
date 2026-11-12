//! Server-side ordering of serialised values (`AbstractType.compare` of each marshal type,
//! Apache-2.0). ScyllaDB orders identically (verified on clustering columns of every type).
//!
//! - An empty value sorts before every other value of its type.
//! - Integers are signed; `varint` and `decimal` are numeric (`1.0 == 1.00`).
//! - `float` / `double` follow `Float.compare`: `-0.0 < 0.0`, every NaN equal and above `+∞`.
//! - `uuid`: by version, then time-ordered timestamp for version 1 or unsigned high bits, then
//!   unsigned low bits. `timeuuid`: timestamp, then the low bytes compared as *signed* bytes.
//! - `date` and `time` (and text, blob, inet, duration) are unsigned byte order.
//! - Frozen collections compare element by element, then by size; tuples and UDTs component by
//!   component with nulls first and missing trailing components equal to nulls.
//!
//! Malformed values never panic; they fall back to a total byte order (validation rejects them
//! before they can be stored). Native types compare without allocating.

use std::cmp::Ordering;

use crate::CqlType;
use crate::wire::{self, Collection, Items};

/// Compares two serialised values of `ty`.
pub fn compare(ty: &CqlType, a: &[u8], b: &[u8]) -> Ordering {
    use CqlType as T;
    match ty {
        T::Ascii | T::Text | T::Blob | T::Inet | T::Date | T::Time | T::Duration | T::Custom(_) => {
            a.cmp(b)
        }
        T::Empty => Ordering::Equal,
        T::Boolean => {
            empty_first(a, b).unwrap_or_else(|| ((a[0] != 0) as u8).cmp(&((b[0] != 0) as u8)))
        }
        T::Tinyint | T::Smallint | T::Int | T::Bigint | T::Counter | T::Timestamp => {
            empty_first(a, b).unwrap_or_else(|| signed_bytes(a, b))
        }
        T::Float => empty_first(a, b).unwrap_or_else(|| match (f32_of(a), f32_of(b)) {
            (Some(x), Some(y)) => java_cmp_f64(x as f64, y as f64, x.is_nan(), y.is_nan()),
            _ => a.cmp(b),
        }),
        T::Double => {
            empty_first(a, b).unwrap_or_else(|| match (wire::i64_of(a), wire::i64_of(b)) {
                (Some(x), Some(y)) => {
                    let (x, y) = (f64::from_bits(x as u64), f64::from_bits(y as u64));
                    java_cmp_f64(x, y, x.is_nan(), y.is_nan())
                }
                _ => a.cmp(b),
            })
        }
        T::Varint => crate::num::compare_varint(a, b),
        T::Decimal => crate::num::compare_decimal(a, b),
        T::Uuid => compare_uuid(a, b),
        T::Timeuuid => compare_timeuuid(a, b),
        T::List { elem, .. } | T::Set { elem, .. } => {
            empty_first(a, b).unwrap_or_else(|| compare_list(elem, a, b))
        }
        T::Map { key, value, .. } => {
            empty_first(a, b).unwrap_or_else(|| compare_map(key, value, a, b))
        }
        T::Tuple(ts) => empty_first(a, b).unwrap_or_else(|| compare_fields(ts.iter(), a, b)),
        T::Udt(def, _) => empty_first(a, b)
            .unwrap_or_else(|| compare_fields(def.fields.iter().map(|(_, t)| t), a, b)),
        T::Vector { elem, dim } => compare_vector(elem, *dim, a, b),
    }
}

/// `compare` honouring a clustering column's `DESC` order (`ReversedType`).
pub fn compare_with_order(ty: &CqlType, reversed: bool, a: &[u8], b: &[u8]) -> Ordering {
    let o = compare(ty, a, b);
    if reversed { o.reverse() } else { o }
}

/// Empty values first; `None` when both are non-empty.
fn empty_first(a: &[u8], b: &[u8]) -> Option<Ordering> {
    match (a.is_empty(), b.is_empty()) {
        (false, false) => None,
        (x, y) => Some(y.cmp(&x)),
    }
}

/// Two's-complement integers of equal width: signed first byte, then unsigned bytes.
fn signed_bytes(a: &[u8], b: &[u8]) -> Ordering {
    if a.len() != b.len() {
        return a.len().cmp(&b.len()).then_with(|| a.cmp(b));
    }
    (a[0] as i8)
        .cmp(&(b[0] as i8))
        .then_with(|| a[1..].cmp(&b[1..]))
}

fn f32_of(b: &[u8]) -> Option<f32> {
    let a: [u8; 4] = b.try_into().ok()?;
    Some(f32::from_be_bytes(a))
}

/// `Double.compare`: NaN equals NaN and is greater than everything; `-0.0 < 0.0`.
fn java_cmp_f64(x: f64, y: f64, xn: bool, yn: bool) -> Ordering {
    match (xn, yn) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => x.total_cmp(&y),
    }
}

fn uuid_parts(b: &[u8]) -> Option<(u64, u64)> {
    let a: &[u8; 16] = b.try_into().ok()?;
    Some((
        u64::from_be_bytes(a[..8].try_into().ok()?),
        u64::from_be_bytes(a[8..].try_into().ok()?),
    ))
}

/// The timestamp of a version-1 msb, reordered so plain comparison is chronological.
fn reorder(msb: u64) -> u64 {
    (msb << 48) | ((msb << 16) & 0xFFFF_0000_0000) | (msb >> 32)
}

/// `UUIDType`: shorter (empty) first, then version, then timestamp (v1) or unsigned msb, then
/// unsigned lsb.
pub fn compare_uuid(a: &[u8], b: &[u8]) -> Ordering {
    match (uuid_parts(a), uuid_parts(b)) {
        (Some((am, al)), Some((bm, bl))) => {
            let (va, vb) = ((am >> 12) & 0xf, (bm >> 12) & 0xf);
            va.cmp(&vb)
                .then_with(|| {
                    if va == 1 {
                        reorder(am).cmp(&reorder(bm))
                    } else {
                        am.cmp(&bm)
                    }
                })
                .then_with(|| al.cmp(&bl))
        }
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => a.cmp(b),
    }
}

/// `TimeUUIDType`: timestamp, then the low eight bytes as signed bytes.
pub fn compare_timeuuid(a: &[u8], b: &[u8]) -> Ordering {
    match (uuid_parts(a), uuid_parts(b)) {
        (Some((am, al)), Some((bm, bl))) => {
            let flip = 0x8080_8080_8080_8080u64;
            reorder(am)
                .cmp(&reorder(bm))
                .then_with(|| (al ^ flip).cmp(&(bl ^ flip)))
        }
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => a.cmp(b),
    }
}

fn next_item<'a>(it: &mut Items<'a>) -> Option<Option<&'a [u8]>> {
    it.next()?.ok()
}

/// Compares two items that may be null (`None` first).
fn cmp_opt(ty: &CqlType, a: Option<&[u8]>, b: Option<&[u8]>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => compare(ty, x, y),
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
    }
}

fn compare_list(elem: &CqlType, a: &[u8], b: &[u8]) -> Ordering {
    let (Some(ca), Some(cb)) = (Collection::parse(a), Collection::parse(b)) else {
        return a.cmp(b);
    };
    let (mut ia, mut ib) = (ca.items(1), cb.items(1));
    loop {
        match (next_item(&mut ia), next_item(&mut ib)) {
            (Some(x), Some(y)) => {
                let o = cmp_opt(elem, x, y);
                if o.is_ne() {
                    return o;
                }
            }
            _ => return ca.count.cmp(&cb.count),
        }
    }
}

fn compare_map(kt: &CqlType, vt: &CqlType, a: &[u8], b: &[u8]) -> Ordering {
    let (Some(ca), Some(cb)) = (Collection::parse(a), Collection::parse(b)) else {
        return a.cmp(b);
    };
    let (mut ia, mut ib) = (ca.items(2), cb.items(2));
    let mut n = 0usize;
    loop {
        let ty = if n.is_multiple_of(2) { kt } else { vt };
        n += 1;
        match (next_item(&mut ia), next_item(&mut ib)) {
            (Some(x), Some(y)) => {
                let o = cmp_opt(ty, x, y);
                if o.is_ne() {
                    return o;
                }
            }
            _ => return ca.count.cmp(&cb.count),
        }
    }
}

/// True if every remaining component is null.
fn rest_all_null(mut f: wire::Fields<'_>) -> bool {
    f.all(|x| matches!(x, Ok(None)))
}

fn compare_fields<'t>(
    mut types: impl Iterator<Item = &'t CqlType>,
    a: &[u8],
    b: &[u8],
) -> Ordering {
    let (mut fa, mut fb) = (wire::fields(a), wire::fields(b));
    for ty in types.by_ref() {
        // Stop once either side runs out.
        if fa.is_exhausted() || fb.is_exhausted() {
            break;
        }
        let (x, y) = (fa.next(), fb.next());
        match (x, y) {
            (Some(Ok(x)), Some(Ok(y))) => {
                let o = cmp_opt(ty, x, y);
                if o.is_ne() {
                    return o;
                }
            }
            _ => return a.cmp(b),
        }
    }
    // `TupleType.compareCustom`'s tail rule: missing trailing components equal nulls. Values
    // with extra (non-null) components only arise from malformed input; Cassandra orders those
    // inconsistently, we fall back to byte order so sorting stays a total order.
    let (na, nb) = (rest_all_null(fa.clone()), rest_all_null(fb.clone()));
    match (na, nb) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => a.cmp(b),
    }
}

fn compare_vector(elem: &CqlType, dim: u32, a: &[u8], b: &[u8]) -> Ordering {
    if elem.is_byte_order_comparable() && elem.value_length_if_fixed().is_some() {
        return a.cmp(b);
    }
    match elem.value_length_if_fixed() {
        Some(len) if len > 0 => {
            let n = (dim as usize).min(a.len() / len).min(b.len() / len);
            for i in 0..n {
                let o = compare(elem, &a[i * len..(i + 1) * len], &b[i * len..(i + 1) * len]);
                if o.is_ne() {
                    return o;
                }
            }
            a.len().cmp(&b.len())
        }
        Some(_) => Ordering::Equal,
        None => {
            let (mut pa, mut pb) = (0usize, 0usize);
            for _ in 0..dim {
                let x = vint_item(a, &mut pa);
                let y = vint_item(b, &mut pb);
                match (x, y) {
                    (Some(x), Some(y)) => {
                        let o = compare(elem, x, y);
                        if o.is_ne() {
                            return o;
                        }
                    }
                    (None, None) => return Ordering::Equal,
                    (None, Some(_)) => return Ordering::Less,
                    (Some(_), None) => return Ordering::Greater,
                }
            }
            Ordering::Equal
        }
    }
}

fn vint_item<'a>(b: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let len = usize::try_from(wire::read_unsigned_vint(b, pos)?).ok()?;
    let v = b.get(*pos..pos.checked_add(len)?)?;
    *pos += len;
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Value, encode};
    use std::sync::Arc;

    fn sorted(ty: &CqlType, values: &[&[u8]]) {
        for (i, x) in values.iter().enumerate() {
            for (j, y) in values.iter().enumerate() {
                assert_eq!(compare(ty, x, y), i.cmp(&j), "{ty:?} {x:02x?} vs {y:02x?}");
            }
        }
    }

    fn uuid(s: &str) -> Vec<u8> {
        crate::timeuuid::parse_uuid(s).unwrap().to_vec()
    }

    #[test]
    fn floats_as_both_references_order_them() {
        // Clustering order on both images: empty, -inf, -1.5, -0, 0, 1.5, inf, NaN.
        let f = |x: f32| x.to_be_bytes().to_vec();
        let vals = [
            vec![],
            f(f32::NEG_INFINITY),
            f(-1.5),
            f(-0.0),
            f(0.0),
            f(1.5),
            f(f32::INFINITY),
            f(f32::NAN),
        ];
        let refs: Vec<&[u8]> = vals.iter().map(|v| &v[..]).collect();
        sorted(&CqlType::Float, &refs);
        // Every NaN payload is the same clustering value.
        assert_eq!(
            compare(&CqlType::Float, &[0x7f, 0xc0, 0, 1], &[0xff, 0xc0, 0, 0]),
            Ordering::Equal
        );
        let d = |x: f64| x.to_be_bytes().to_vec();
        let vals = [
            d(f64::NEG_INFINITY),
            d(-1e-300),
            d(-0.0),
            d(0.0),
            d(1e300),
            d(f64::INFINITY),
            d(f64::NAN),
        ];
        let refs: Vec<&[u8]> = vals.iter().map(|v| &v[..]).collect();
        sorted(&CqlType::Double, &refs);
    }

    #[test]
    fn uuids_as_both_references_order_them() {
        let vals: Vec<Vec<u8>> = [
            "00000000-0000-0000-0000-000000000000",
            "00000000-0000-1000-8000-000000000000",
            "00000001-0000-1000-8000-000000000000",
            "00000000-0001-1000-8000-000000000000",
            "00000000-0000-1001-7f00-000000000000",
            "00000000-0000-1001-8000-000000000000",
            "80000000-0000-3000-8000-000000000000",
            "00000000-0000-4000-7f00-000000000000",
            "00000000-0000-4000-8000-000000000000",
            "ffffffff-ffff-4fff-bfff-ffffffffffff",
            "00000000-0000-7000-8000-000000000000",
            "00000000-0000-f000-8000-000000000000",
        ]
        .iter()
        .map(|s| uuid(s))
        .collect();
        let mut refs: Vec<&[u8]> = vec![&[]];
        refs.extend(vals.iter().map(|v| &v[..]));
        sorted(&CqlType::Uuid, &refs);
    }

    #[test]
    fn timeuuids_as_both_references_order_them() {
        let vals: Vec<Vec<u8>> = [
            "00000000-0000-1000-8000-000000000000",
            "00000000-0000-1000-ff00-000000000000",
            "00000000-0000-1000-0080-000000000000",
            "00000000-0000-1000-0000-000000000000",
            "00000000-0000-1000-007f-000000000000",
            "00000000-0000-1000-7f00-000000000000",
            "00000001-0000-1000-8000-000000000000",
            "00000000-0001-1000-8000-000000000000",
            "00000000-0000-1001-8000-000000000000",
        ]
        .iter()
        .map(|s| uuid(s))
        .collect();
        let mut refs: Vec<&[u8]> = vec![&[]];
        refs.extend(vals.iter().map(|v| &v[..]));
        sorted(&CqlType::Timeuuid, &refs);
        let g = crate::timeuuid::Generator::with_clock_seq_and_node(0x8000_0100_0000_0000);
        let a = g.next_at(5);
        let b = g.next_at(5);
        assert_eq!(compare(&CqlType::Timeuuid, &a, &b), Ordering::Less);
        assert_eq!(
            compare(
                &CqlType::Timeuuid,
                &crate::timeuuid::min_timeuuid(5),
                &crate::timeuuid::max_timeuuid(5)
            ),
            Ordering::Less
        );
    }

    #[test]
    fn integers_and_misc() {
        let i = |x: i32| x.to_be_bytes().to_vec();
        let vals = [vec![], i(i32::MIN), i(-1), i(0), i(1), i(i32::MAX)];
        sorted(
            &CqlType::Int,
            &vals.iter().map(|v| &v[..]).collect::<Vec<_>>(),
        );
        sorted(&CqlType::Tinyint, &[&[0x80], &[0xff], &[0], &[0x7f]]);
        sorted(
            &CqlType::Smallint,
            &[&[], &[0x80, 0], &[0xff, 0xff], &[0, 0], &[0, 0xff], &[1, 0]],
        );
        let l = |x: i64| x.to_be_bytes().to_vec();
        let vals = [vec![], l(i64::MIN), l(-1), l(0), l(i64::MAX)];
        sorted(
            &CqlType::Bigint,
            &vals.iter().map(|v| &v[..]).collect::<Vec<_>>(),
        );
        sorted(
            &CqlType::Timestamp,
            &vals.iter().map(|v| &v[..]).collect::<Vec<_>>(),
        );
        sorted(&CqlType::Boolean, &[&[], &[0], &[1]]);
        assert_eq!(compare(&CqlType::Boolean, &[1], &[7]), Ordering::Equal);
        // Dates: unsigned with the epoch at 2^31 (1969 sorts before 1971).
        sorted(
            &CqlType::Date,
            &[
                &[0x7f, 0xff, 0xfe, 0x93],
                &[0x80, 0, 0, 0],
                &[0x80, 0, 1, 0x6d],
            ],
        );
        sorted(
            &CqlType::Time,
            &[
                &[0; 8],
                &[0, 0, 0, 0, 0, 0, 0, 1],
                &[0, 0, 0x4e, 0x94, 0x91, 0x4f, 0, 0],
            ],
        );
        // Inet: plain byte order, 16-byte ::1 before 4-byte 1.2.3.4.
        let v6 = "::1".parse::<std::net::Ipv6Addr>().unwrap().octets();
        let ff = "ff::".parse::<std::net::Ipv6Addr>().unwrap().octets();
        sorted(&CqlType::Inet, &[&v6, &ff, &[1, 2, 3, 4], &[255, 0, 0, 0]]);
        sorted(&CqlType::Text, &[b"", b"A", b"a", b"ab", "é".as_bytes()]);
        sorted(&CqlType::Blob, &[&[], &[0], &[0, 0], &[0xff]]);
        assert_eq!(compare(&CqlType::Empty, b"", b""), Ordering::Equal);
        assert_eq!(
            compare_with_order(&CqlType::Int, true, &i(1), &i(2)),
            Ordering::Greater
        );
        // Reversed order puts empty last, as Cassandra's DESC clustering shows.
        assert_eq!(
            compare_with_order(&CqlType::Int, true, &[], &i(-1)),
            Ordering::Greater
        );
        // Malformed lengths do not panic.
        assert_ne!(compare(&CqlType::Int, &[1], &[1, 2]), Ordering::Equal);
        assert_ne!(compare(&CqlType::Float, &[1], &[1, 2]), Ordering::Equal);
        assert_ne!(compare(&CqlType::Double, &[1], &[1, 2]), Ordering::Equal);
        assert_ne!(compare(&CqlType::Uuid, &[1], &[1, 2]), Ordering::Equal);
        assert_eq!(compare(&CqlType::Uuid, &[1; 16], &[1]), Ordering::Greater);
        assert_eq!(compare(&CqlType::Timeuuid, &[1], &[1; 16]), Ordering::Less);
    }

    #[test]
    fn frozen_collections_tuples_vectors() {
        let lt = CqlType::list(CqlType::Int, true);
        let l = |xs: &[i32]| {
            encode(
                &Value::List(xs.iter().map(|x| Value::Int(*x)).collect()),
                &lt,
            )
            .to_vec()
        };
        let vals = [l(&[]), l(&[-1, 5]), l(&[1]), l(&[1, 2]), l(&[2])];
        sorted(&lt, &vals.iter().map(|v| &v[..]).collect::<Vec<_>>());
        assert_eq!(compare(&lt, &[], &l(&[])), Ordering::Less);
        let mt = CqlType::map(CqlType::Int, CqlType::Int, true);
        let m = |xs: &[(i32, i32)]| {
            encode(
                &Value::Map(
                    xs.iter()
                        .map(|(k, v)| (Value::Int(*k), Value::Int(*v)))
                        .collect(),
                ),
                &mt,
            )
            .to_vec()
        };
        let vals = [
            m(&[]),
            m(&[(1, 1)]),
            m(&[(1, 2)]),
            m(&[(1, 2), (3, 0)]),
            m(&[(2, 0)]),
        ];
        sorted(&mt, &vals.iter().map(|v| &v[..]).collect::<Vec<_>>());

        let tt = CqlType::Tuple(vec![CqlType::Int, CqlType::Text]);
        let t = |a: Option<i32>, b: Option<&str>| {
            encode(
                &Value::Tuple(vec![
                    a.map_or(Value::Null, Value::Int),
                    b.map_or(Value::Null, |s| Value::Text(s.into())),
                ]),
                &tt,
            )
            .to_vec()
        };
        // Both images: (null, 'a') < (0, 'z') < (1, null) < (1, 'a').
        let vals = [
            t(None, Some("a")),
            t(Some(0), Some("z")),
            t(Some(1), None),
            t(Some(1), Some("a")),
        ];
        sorted(&tt, &vals.iter().map(|v| &v[..]).collect::<Vec<_>>());
        // Missing trailing components equal nulls.
        let short = encode(&Value::Tuple(vec![Value::Int(1)]), &tt).to_vec();
        assert_eq!(compare(&tt, &short, &t(Some(1), None)), Ordering::Equal);
        assert_eq!(compare(&tt, &short, &t(Some(1), Some("a"))), Ordering::Less);
        assert_eq!(
            compare(&tt, &t(Some(1), Some("a")), &short),
            Ordering::Greater
        );
        let ut = CqlType::Udt(
            Arc::new(crate::UdtDef::new(
                "k",
                "u",
                vec![("a".into(), CqlType::Int)],
            )),
            true,
        );
        assert_eq!(
            compare(
                &ut,
                &encode(&Value::Udt(vec![Value::Int(1)]), &ut),
                &encode(&Value::Udt(vec![Value::Int(2)]), &ut)
            ),
            Ordering::Less
        );

        // Vector<int, 2> on both images: [-1, 5] < [1, -1].
        let vt = CqlType::vector(CqlType::Int, 2);
        let v = |a: i32, b: i32| {
            encode(&Value::Vector(vec![Value::Int(a), Value::Int(b)]), &vt).to_vec()
        };
        sorted(&vt, &[&v(-1, 5), &v(1, -1), &v(1, 2)]);
        let vtt = CqlType::vector(CqlType::Text, 2);
        let s = |a: &str, b: &str| {
            encode(
                &Value::Vector(vec![Value::Text(a.into()), Value::Text(b.into())]),
                &vtt,
            )
            .to_vec()
        };
        sorted(&vtt, &[&s("a", "z"), &s("b", ""), &s("b", "a")]);
        // Malformed compound values do not panic.
        for ty in [&lt, &mt, &tt, &vt, &vtt] {
            let _ = compare(ty, &[0, 0, 0, 9, 1], &[0, 0, 0, 9]);
            let _ = compare(ty, &[0xff; 3], &[0x7f; 20]);
        }
    }
}
