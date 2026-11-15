//! Roaring bitmaps in their portable serialization, for OpenSearch's `terms` queries with
//! `"value_type": "bitmap"`: a base64 bitmap of the integers to match instead of a list.
//!
//! The 32-bit format (`RoaringBitmap.serialize`): a cookie — `12346` with no run containers,
//! or `12347 | (containers - 1) << 16` followed by a bitset of which containers are runs —,
//! the container count (first format only), per container its key (the high 16 bits) and
//! cardinality - 1, then (always in the first format, from 4 containers on in the second) one
//! offset per container, then the containers: arrays of up to 4096 sorted 16-bit values,
//! 8 KiB bitsets, or runs of `(start, length - 1)` pairs. The 64-bit format
//! (`Roaring64NavigableMap.serializePortable`, for `long` fields): a 64-bit count of 32-bit
//! bitmaps, each preceded by its 32-bit high key. Everything is little endian.
//!
//! Every count is checked against the bytes actually present before anything is allocated,
//! so a hostile bitmap can't make the decoder reserve more than its input size implies.

use crate::error::{EsError, EsResult};

const SERIAL_COOKIE_NO_RUNCONTAINER: u32 = 12346;
const SERIAL_COOKIE: u16 = 12347;
const NO_OFFSET_THRESHOLD: usize = 4;
const BITMAP_CONTAINER_BYTES: usize = 8192;
const ARRAY_MAX: usize = 4096;

fn corrupt(what: &str) -> EsError {
    EsError::illegal_argument(format!("Failed to deserialize the bitmap: {what}"))
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> EsResult<&[u8]> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.b.len()).ok_or_else(|| corrupt("unexpected end of input"))?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u16(&mut self) -> EsResult<u16> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }

    fn u32(&mut self) -> EsResult<u32> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn u64(&mut self) -> EsResult<u64> {
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(u64::from_le_bytes(a))
    }

    fn remaining(&self) -> usize {
        self.b.len() - self.pos
    }
}

/// Decodes one 32-bit portable bitmap from `r`, calling `out` with each value in order.
fn read32(r: &mut Reader<'_>, out: &mut dyn FnMut(u32)) -> EsResult<()> {
    let cookie = r.u32()?;
    let (size, runs) = if cookie == SERIAL_COOKIE_NO_RUNCONTAINER {
        let n = r.u32()? as usize;
        (n, None)
    } else if cookie as u16 == SERIAL_COOKIE {
        let n = (cookie >> 16) as usize + 1;
        let flags = r.take(n.div_ceil(8))?.to_vec();
        (n, Some(flags))
    } else {
        return Err(corrupt("unknown cookie"));
    };
    // Each container needs at least its 4-byte descriptive header.
    if size > 65536 || size.saturating_mul(4) > r.remaining() {
        return Err(corrupt("invalid container count"));
    }
    let mut headers = Vec::with_capacity(size);
    for _ in 0..size {
        let key = r.u16()?;
        let card = r.u16()? as usize + 1;
        headers.push((key, card));
    }
    if runs.is_none() || size >= NO_OFFSET_THRESHOLD {
        r.take(size.checked_mul(4).ok_or_else(|| corrupt("invalid container count"))?)?;
    }
    for (i, (key, card)) in headers.into_iter().enumerate() {
        let high = u32::from(key) << 16;
        let is_run = runs.as_ref().is_some_and(|f| f[i / 8] & (1 << (i % 8)) != 0);
        if is_run {
            let n = r.u16()? as usize;
            for _ in 0..n {
                let start = u32::from(r.u16()?);
                let len = u32::from(r.u16()?);
                if start + len > 0xFFFF {
                    return Err(corrupt("run past the container"));
                }
                for v in start..=start + len {
                    out(high | v);
                }
            }
        } else if card > ARRAY_MAX {
            let words = r.take(BITMAP_CONTAINER_BYTES)?;
            for (w, chunk) in words.as_chunks::<8>().0.iter().enumerate() {
                let mut bits = u64::from_le_bytes(*chunk);
                while bits != 0 {
                    let t = bits.trailing_zeros();
                    out(high | (w as u32 * 64 + t));
                    bits &= bits - 1;
                }
            }
        } else {
            let values = r.take(card * 2)?;
            for c in values.as_chunks::<2>().0 {
                out(high | u32::from(u16::from_le_bytes(*c)));
            }
        }
    }
    Ok(())
}

fn decode_base64(b64: &str) -> EsResult<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(b64.trim()).map_err(|e| EsError::illegal_argument(format!("Illegal base64 character in the bitmap: {e}")))
}

/// The integers of a base64 32-bit portable bitmap (`integer`, `short`, `byte` fields).
pub fn decode_ints(b64: &str) -> EsResult<Vec<i64>> {
    let bytes = decode_base64(b64)?;
    let mut r = Reader { b: &bytes, pos: 0 };
    let mut out = Vec::new();
    // Roaring treats values as unsigned; Java's `int` view of the high half is negative.
    read32(&mut r, &mut |v| out.push(i64::from(v as i32)))?;
    Ok(out)
}

/// The longs of a base64 64-bit portable bitmap (`long` fields).
pub fn decode_longs(b64: &str) -> EsResult<Vec<i64>> {
    let bytes = decode_base64(b64)?;
    let mut r = Reader { b: &bytes, pos: 0 };
    let maps = r.u64()?;
    // A map takes at least its key and an 8-byte header.
    if maps > (r.remaining() / 12) as u64 {
        return Err(corrupt("invalid bitmap count"));
    }
    let mut out = Vec::new();
    for _ in 0..maps {
        let high = u64::from(r.u32()?) << 32;
        read32(&mut r, &mut |v| out.push((high | u64::from(v)) as i64))?;
    }
    Ok(out)
}

/// The values a `"value_type": "bitmap"` terms query matches on `field`: its one value (given,
/// or read from the lookup document) decoded as a 64-bit bitmap for `long` fields and a 32-bit
/// one for the others.
pub fn terms(ctx: &crate::search::query::QueryCtx, field: &str, values: &[crate::json::Value]) -> EsResult<Vec<crate::json::Value>> {
    use crate::json::Value;
    let [Value::Str(b64)] = values else {
        return Err(EsError::illegal_argument("Invalid value for bitmap type: Expected a single base64 encoded serialized bitmap."));
    };
    let long = ctx.field(field).is_some_and(|f| f.kind == crate::engine::fields::Kind::Long);
    let ints = if long { decode_longs(b64)? } else { decode_ints(b64)? };
    Ok(ints.into_iter().map(Value::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The bitmaps of OpenSearch's own `380_bitmap_filtering` and `381_bitmap_filtering_long`.
    #[test]
    fn decodes_opensearch_bitmaps() {
        assert_eq!(decode_ints("OjAAAAEAAAAAAAEAEAAAAG8A3gA=").unwrap(), vec![111, 222]);
        assert_eq!(decode_ints("OjAAAAEAAAAAAAAAEAAAAG8A").unwrap(), vec![111]);
        assert_eq!(decode_ints("OjAAAAEAAAAAAAEAEAAAAN4ATQE=").unwrap(), vec![222, 333]);
        assert_eq!(
            decode_longs("AgAAAAAAAADoAAAAOjAAAAEAAACl1AAAEAAAAAEQ0QEAADowAAABAAAASqkAABAAAAACIA==").unwrap(),
            vec![1_000_000_000_001, 2_000_000_000_002]
        );
        assert_eq!(decode_longs("AQAAAAAAAADoAAAAOjAAAAEAAACl1AAAEAAAAAEQ").unwrap(), vec![1_000_000_000_001]);
    }

    #[test]
    fn run_and_bitset_containers() {
        // 12347 with one run container: values 10..=14.
        let mut b = Vec::new();
        b.extend_from_slice(&(12347u32).to_le_bytes());
        b.push(1); // run flags
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&4u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // one run
        b.extend_from_slice(&10u16.to_le_bytes());
        b.extend_from_slice(&4u16.to_le_bytes());
        use base64::Engine as _;
        let s = base64::engine::general_purpose::STANDARD.encode(&b);
        assert_eq!(decode_ints(&s).unwrap(), vec![10, 11, 12, 13, 14]);
        // A bitset container (cardinality 5000: every even value below 10000).
        let mut b = Vec::new();
        b.extend_from_slice(&SERIAL_COOKIE_NO_RUNCONTAINER.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // key 1
        b.extend_from_slice(&4999u16.to_le_bytes());
        b.extend_from_slice(&16u32.to_le_bytes());
        let mut words = vec![0u64; 1024];
        for v in (0..10000u32).step_by(2) {
            words[(v / 64) as usize] |= 1 << (v % 64);
        }
        for w in words {
            b.extend_from_slice(&w.to_le_bytes());
        }
        let s = base64::engine::general_purpose::STANDARD.encode(&b);
        let v = decode_ints(&s).unwrap();
        assert_eq!(v.len(), 5000);
        assert_eq!(v[0], 65536);
        assert_eq!(v[4999], 65536 + 9998);
    }

    #[test]
    fn hostile_input_is_refused() {
        use base64::Engine as _;
        let mut b = Vec::new();
        b.extend_from_slice(&SERIAL_COOKIE_NO_RUNCONTAINER.to_le_bytes());
        b.extend_from_slice(&u32::MAX.to_le_bytes());
        let s = base64::engine::general_purpose::STANDARD.encode(&b);
        assert!(decode_ints(&s).is_err());
        let s = base64::engine::general_purpose::STANDARD.encode(u64::MAX.to_le_bytes());
        assert!(decode_longs(&s).is_err());
        assert!(decode_ints("OjAA").is_err());
        assert!(decode_ints("not base64!").is_err());
    }
}
