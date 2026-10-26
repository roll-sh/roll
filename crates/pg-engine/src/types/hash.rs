//! Postgres's 64-bit extended hash functions (Bob Jenkins' lookup3, as in hashfn.c) and the
//! per-type `hash*extended` support functions, for hash partitioning.

use crate::types::{DataType, Value};

/// The seed of partition hashing (HASH_PARTITION_SEED).
pub const PARTITION_SEED: u64 = 0x7A5B_2236_7996_DCFD;

macro_rules! mix {
    ($a:ident, $b:ident, $c:ident) => {
        $a = $a.wrapping_sub($c);
        $a ^= $c.rotate_left(4);
        $c = $c.wrapping_add($b);
        $b = $b.wrapping_sub($a);
        $b ^= $a.rotate_left(6);
        $a = $a.wrapping_add($c);
        $c = $c.wrapping_sub($b);
        $c ^= $b.rotate_left(8);
        $b = $b.wrapping_add($a);
        $a = $a.wrapping_sub($c);
        $a ^= $c.rotate_left(16);
        $c = $c.wrapping_add($b);
        $b = $b.wrapping_sub($a);
        $b ^= $a.rotate_left(19);
        $a = $a.wrapping_add($c);
        $c = $c.wrapping_sub($b);
        $c ^= $b.rotate_left(4);
        $b = $b.wrapping_add($a);
    };
}

macro_rules! fin {
    ($a:ident, $b:ident, $c:ident) => {
        $c ^= $b;
        $c = $c.wrapping_sub($b.rotate_left(14));
        $a ^= $c;
        $a = $a.wrapping_sub($c.rotate_left(11));
        $b ^= $a;
        $b = $b.wrapping_sub($a.rotate_left(25));
        $c ^= $b;
        $c = $c.wrapping_sub($b.rotate_left(16));
        $a ^= $c;
        $a = $a.wrapping_sub($c.rotate_left(4));
        $b ^= $a;
        $b = $b.wrapping_sub($a.rotate_left(14));
        $c ^= $b;
        $c = $c.wrapping_sub($b.rotate_left(24));
    };
}

/// hash_bytes_uint32_extended
pub fn hash_uint32_extended(k: u32, seed: u64) -> u64 {
    let mut a: u32 = 0x9e37_79b9u32.wrapping_add(4).wrapping_add(3_923_095);
    let mut b = a;
    let mut c = a;
    if seed != 0 {
        a = a.wrapping_add((seed >> 32) as u32);
        b = b.wrapping_add(seed as u32);
        mix!(a, b, c);
    }
    a = a.wrapping_add(k);
    fin!(a, b, c);
    ((b as u64) << 32) | c as u64
}

/// hash_bytes_extended (little-endian byte order, as on the reference platforms).
pub fn hash_bytes_extended(k: &[u8], seed: u64) -> u64 {
    let mut a: u32 = 0x9e37_79b9u32
        .wrapping_add(k.len() as u32)
        .wrapping_add(3_923_095);
    let mut b = a;
    let mut c = a;
    if seed != 0 {
        a = a.wrapping_add((seed >> 32) as u32);
        b = b.wrapping_add(seed as u32);
        mix!(a, b, c);
    }
    let word = |s: &[u8]| u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
    let mut k = k;
    while k.len() >= 12 {
        a = a.wrapping_add(word(&k[0..4]));
        b = b.wrapping_add(word(&k[4..8]));
        c = c.wrapping_add(word(&k[8..12]));
        mix!(a, b, c);
        k = &k[12..];
    }
    let byte = |i: usize, shift: u32| (k[i] as u32) << shift;
    let len = k.len();
    if len >= 11 {
        c = c.wrapping_add(byte(10, 24));
    }
    if len >= 10 {
        c = c.wrapping_add(byte(9, 16));
    }
    if len >= 9 {
        c = c.wrapping_add(byte(8, 8));
    }
    if len >= 8 {
        b = b.wrapping_add(byte(7, 24));
    }
    if len >= 7 {
        b = b.wrapping_add(byte(6, 16));
    }
    if len >= 6 {
        b = b.wrapping_add(byte(5, 8));
    }
    if len >= 5 {
        b = b.wrapping_add(byte(4, 0));
    }
    if len >= 4 {
        a = a.wrapping_add(byte(3, 24));
    }
    if len >= 3 {
        a = a.wrapping_add(byte(2, 16));
    }
    if len >= 2 {
        a = a.wrapping_add(byte(1, 8));
    }
    if len >= 1 {
        a = a.wrapping_add(byte(0, 0));
    }
    fin!(a, b, c);
    ((b as u64) << 32) | c as u64
}

/// hash_combine64
pub fn combine64(a: u64, b: u64) -> u64 {
    a ^ b
        .wrapping_add(0x49a0_f4dd_15e5_a8e3)
        .wrapping_add(a << 54)
        .wrapping_add(a >> 7)
}

fn hash_int8(v: i64, seed: u64) -> u64 {
    let mut lo = v as u32;
    let hi = (v >> 32) as u32;
    lo ^= if v >= 0 { hi } else { !hi };
    hash_uint32_extended(lo, seed)
}

fn hash_float8(v: f64, seed: u64) -> u64 {
    // Zero and minus zero hash alike; every NaN is the same NaN.
    if v == 0.0 {
        return seed;
    }
    let v = if v.is_nan() { f64::NAN } else { v };
    hash_bytes_extended(&v.to_ne_bytes(), seed)
}

/// The type's extended hash support function (`hashint4extended`, `hashtextextended`, …) of a
/// non-null value; None for a type without one.
pub fn value_hash(v: &Value, ty: &DataType, seed: u64) -> Option<u64> {
    use Value as V;
    Some(match v {
        V::Bool(b) => hash_uint32_extended(*b as u32, seed),
        V::Int2(x) => hash_uint32_extended(*x as i32 as u32, seed),
        V::Int4(x) => hash_uint32_extended(*x as u32, seed),
        V::Int8(x) => hash_int8(*x, seed),
        V::Oid(x) => hash_uint32_extended(*x, seed),
        V::Date(x) => hash_uint32_extended(*x as u32, seed),
        V::Time(x) | V::Timestamp(x) | V::Timestamptz(x) => hash_int8(*x, seed),
        V::Float4(x) => hash_float8(*x as f64, seed),
        V::Float8(x) => hash_float8(*x, seed),
        V::Char(c) => hash_uint32_extended(*c as i8 as i32 as u32, seed),
        V::Text(s) => {
            // Trailing spaces don't count in char(n).
            let s = match ty {
                DataType::Bpchar(_) => s.trim_end_matches(' '),
                _ => s,
            };
            // (The text's bytes in the database's encoding.)
            let enc = if s.is_ascii() { super::mbconv::UTF8 } else { super::collation::database_encoding() };
            match super::mbconv::server::encode(enc, s) {
                Ok(b) => hash_bytes_extended(&b, seed),
                Err(_) => hash_bytes_extended(s.as_bytes(), seed),
            }
        }
        V::Bytea(b) => hash_bytes_extended(b, seed),
        V::Uuid(u) => hash_bytes_extended(&u.to_be_bytes(), seed),
        V::Numeric(n) => hash_numeric(n, seed),
        V::Macaddr(m) => hash_bytes_extended(m, seed),
        V::Macaddr8(m) => hash_bytes_extended(m, seed),
        V::PgLsn(l) | V::Xid8(l) => hash_int8(*l as i64, seed),
        V::Money(m) => hash_int8(*m, seed),
        // The inet struct's family, netmask length and address.
        V::Inet(addr, bits) => {
            let mut b = vec![if addr.len() == 4 { 2 } else { 3 }, *bits];
            b.extend_from_slice(addr);
            hash_bytes_extended(&b, seed)
        }
        // interval_cmp_value's span, truncated to 64 bits.
        V::Interval(iv) => {
            let span = iv.micros as i128
                + (iv.months as i128 * 30 + iv.days as i128)
                    * crate::types::value::USECS_PER_DAY as i128;
            hash_int8(span as i64, seed)
        }
        V::Timetz(t, zone) => hash_int8(*t, seed) ^ hash_uint32_extended(*zone as u32, seed),
        // ItemPointerData: the block number's halves, then the offset.
        V::Tid(block, off) => {
            let mut b = Vec::with_capacity(6);
            b.extend_from_slice(&((block >> 16) as u16).to_le_bytes());
            b.extend_from_slice(&(*block as u16).to_le_bytes());
            b.extend_from_slice(&off.to_le_bytes());
            hash_bytes_extended(&b, seed)
        }
        V::Jsonb(j) => jsonb_hash(j, seed),
        V::Array(a) if matches!(ty, DataType::OidVector) => {
            let b: Vec<u8> = a
                .items
                .iter()
                .flat_map(|v| (v.as_i64().unwrap_or(0) as u32).to_le_bytes())
                .collect();
            hash_bytes_extended(&b, seed)
        }
        // hash_array: the elements' hashes (a NULL's is 0), combined.
        V::Array(a) => {
            let mut result: u64 = 1;
            for v in &a.items {
                let h = if v.is_null() {
                    0
                } else {
                    value_hash(v, &a.elem, seed)?
                };
                result = (result << 5).wrapping_sub(result).wrapping_add(h);
            }
            result
        }
        _ => return None,
    })
}

fn hash_numeric(n: &crate::types::numeric::Numeric, seed: u64) -> u64 {
    match n {
        crate::types::numeric::Numeric::Finite { int, scale } => {
            let (digits, weight) = crate::types::binary::numeric_digits(int, *scale);
            if digits.is_empty() {
                return seed.wrapping_sub(1);
            }
            let bytes: Vec<u8> = digits.iter().flat_map(|d| d.to_le_bytes()).collect();
            hash_bytes_extended(&bytes, seed) ^ (weight as i64 as u64)
        }
        _ => seed,
    }
}

/// jsonb_hash_extended: every scalar, key and value in storage order folded in, and a flag
/// for each array or object begun (a top-level scalar is a one-element array).
fn jsonb_hash(j: &crate::types::json::Jsonb, seed: u64) -> u64 {
    use crate::types::json::Jsonb as J;
    // JB_FARRAY and JB_FOBJECT, in both halves.
    const ARRAY: u64 = 0x4000_0000_4000_0000;
    const OBJECT: u64 = 0x2000_0000_2000_0000;
    fn scalar(j: &J, seed: u64, hash: &mut u64) {
        let tmp = match j {
            J::Null => seed.wrapping_add(1),
            J::String(s) => hash_bytes_extended(s.as_bytes(), seed),
            J::Number(n) => hash_numeric(n, seed),
            J::Bool(b) if seed != 0 => hash_uint32_extended(*b as u32, seed),
            J::Bool(b) => {
                if *b {
                    2
                } else {
                    4
                }
            }
            J::Array(_) | J::Object(_) => return walk(j, seed, hash),
        };
        // ROTATE_HIGH_AND_LOW_32BITS
        *hash = ((*hash << 1) & 0xffff_fffe_ffff_fffe) | ((*hash >> 31) & 0x0000_0001_0000_0001);
        *hash ^= tmp;
    }
    fn walk(j: &J, seed: u64, hash: &mut u64) {
        crate::stack::grow(|| walk_here(j, seed, hash))
    }
    fn walk_here(j: &J, seed: u64, hash: &mut u64) {
        match j {
            J::Array(items) => {
                *hash ^= ARRAY;
                items.iter().for_each(|v| scalar(v, seed, hash));
            }
            J::Object(pairs) => {
                *hash ^= OBJECT;
                for (k, v) in pairs {
                    scalar(&J::String(k.clone()), seed, hash);
                    scalar(v, seed, hash);
                }
            }
            s => {
                *hash ^= ARRAY;
                scalar(s, seed, hash);
            }
        }
    }
    match j {
        J::Array(v) if v.is_empty() => return seed,
        J::Object(v) if v.is_empty() => return seed,
        _ => {}
    }
    let mut hash = 0;
    walk(j, seed, &mut hash);
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_postgres() {
        let s = PARTITION_SEED;
        assert_eq!(hash_uint32_extended(3, s) as i64, -6526785837914879018);
        assert_eq!(hash_uint32_extended(0, 0) as i64, 4154612158245552303);
        assert_eq!(
            hash_uint32_extended(-7i32 as u32, 42) as i64,
            -7260134608902199406
        );
        assert_eq!(hash_int8(1234567890123, s) as i64, 4234184308168508908);
        assert_eq!(hash_int8(-5, s) as i64, 5466844944386338076);
        assert_eq!(
            hash_bytes_extended(b"hello", s) as i64,
            -2635279551741001039
        );
        assert_eq!(hash_bytes_extended(b"", s) as i64, -5700645584453517373);
        assert_eq!(
            hash_bytes_extended(b"a much longer string that crosses twelve", s) as i64,
            1344983494712097834
        );
        assert_eq!(hash_float8(1.5, s) as i64, -2826383321045882978);
        assert_eq!(hash_float8(0.0, s) as i64, 8816678312871386365);
        assert_eq!(
            value_hash(&Value::Text("ab  ".into()), &DataType::Bpchar(4), s).unwrap() as i64,
            596009030900183869
        );
        let u = u128::from_str_radix("a0eebc999c0b4ef8bb6d6bb9bd380a11", 16).unwrap();
        assert_eq!(
            value_hash(&Value::Uuid(u), &DataType::Uuid, s).unwrap() as i64,
            424513190990560493
        );
        assert_eq!(
            value_hash(&Value::Char(b'x'), &DataType::Char, s).unwrap() as i64,
            8622996900272204718
        );
        let num = |t: &str| Value::Numeric(std::sync::Arc::new(t.parse().unwrap()));
        assert_eq!(
            value_hash(&num("12.50"), &DataType::Numeric(-1), s).unwrap() as i64,
            -7880450537691530978
        );
        assert_eq!(
            value_hash(&num("0"), &DataType::Numeric(-1), s).unwrap() as i64,
            8816678312871386364
        );
        assert_eq!(
            value_hash(&num("-123456789.000123"), &DataType::Numeric(-1), s).unwrap() as i64,
            6169692393251117769
        );
        assert_eq!(hash_uint32_extended(5, 0) as i64, -9138765687874534020);
    }
}
