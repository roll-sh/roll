//! Binary I/O of the native extension types (their typsend / typreceive): hstore_send /
//! hstore_recv, ltree_send, lquery_send and ltxtq_send (a version byte, then the text),
//! cube_send / cube_recv, citext as text. The others have no send or receive function:
//! asking for their binary format fails as in PostgreSQL.

use super::types::{self, CITEXT, CUBE, HALFVEC, HSTORE, LQUERY, LTREE, LTXTQUERY, SPARSEVEC, VECTOR};
use crate::error::{Error, Result, code};
use crate::types::binary::{get_text, put_text};
use crate::types::text::FormatCtx;
use bytes::{Buf, BufMut, BytesMut};

/// Whether native type `kind` has send and receive functions.
pub fn has_binary(kind: u16) -> bool {
    matches!(kind, CITEXT | HSTORE | LTREE | LQUERY | LTXTQUERY | CUBE | VECTOR | HALFVEC | SPARSEVEC)
}

fn no_function(kind: u16, dir: &str) -> Error {
    let name = types::get(kind).map_or("unknown", |t| t.name);
    Error::new(
        code::UNDEFINED_FUNCTION,
        format!("no binary {dir} function available for type {name}"),
    )
}

/// The type's send function over value bytes `b`.
pub fn send(kind: u16, b: &[u8], ctx: &FormatCtx, out: &mut BytesMut) -> Result<()> {
    let text = |b: &[u8]| types::get(kind).map(|t| (t.output)(b, ctx)).unwrap_or_default();
    match kind {
        CITEXT => put_text(out, &text(b), ctx),
        HSTORE => {
            let hs = super::hstore::Hs::new(b);
            out.put_i32(hs.count() as i32);
            for (k, v) in hs.pairs() {
                // (The lengths are the server's bytes; pq_sendtext converts the text.)
                out.put_i32(k.len() as i32);
                put_text(out, &String::from_utf8_lossy(k), ctx)?;
                match v {
                    None => out.put_i32(-1),
                    Some(v) => {
                        out.put_i32(v.len() as i32);
                        put_text(out, &String::from_utf8_lossy(v), ctx)?;
                    }
                }
            }
            Ok(())
        }
        LTREE | LQUERY | LTXTQUERY => {
            out.put_u8(1);
            put_text(out, &text(b), ctx)
        }
        VECTOR | HALFVEC | SPARSEVEC => super::vector::send(kind, b, out),
        CUBE => {
            // The header, then every stored coordinate (cube_send, big-endian).
            let mut r = b;
            if r.remaining() < 4 {
                return Err(short());
            }
            out.put_u32(r.get_u32_le());
            while r.remaining() >= 8 {
                out.put_f64(r.get_f64_le());
            }
            Ok(())
        }
        _ => Err(no_function(kind, "output")),
    }
}

/// The type's receive function: the value's bytes.
pub fn recv(kind: u16, b: &[u8], ctx: &FormatCtx) -> Result<Vec<u8>> {
    let input = |s: &str| types::get(kind).map_or(Ok(Vec::new()), |t| (t.input)(s));
    match kind {
        CITEXT => input(&get_text(b, ctx)?),
        HSTORE => hstore_recv(b, ctx),
        LTREE | LQUERY | LTXTQUERY => {
            let Some((&version, rest)) = b.split_first() else {
                return Err(short());
            };
            if version != 1 {
                let name = types::get(kind).map_or("", |t| t.name);
                return Err(crate::error::internal(format!(
                    "unsupported {name} version number {version}"
                )));
            }
            input(&get_text(rest, ctx)?)
        }
        CUBE => cube_recv(b),
        // (pgvector's receive functions check the typmod: -1 here, a bound parameter's.)
        VECTOR | HALFVEC | SPARSEVEC => super::vector::recv(kind, b, -1),
        _ => Err(no_function(kind, "input")),
    }
}

fn short() -> Error {
    Error::new(code::PROTOCOL_VIOLATION, "insufficient data left in message")
}

fn get_i32(r: &mut &[u8]) -> Result<i32> {
    if r.remaining() < 4 {
        return Err(short());
    }
    Ok(r.get_i32())
}

/// hstore_recv: the pairs, then (hstoreUniquePairs) sorted with duplicate keys dropped.
fn hstore_recv(b: &[u8], ctx: &FormatCtx) -> Result<Vec<u8>> {
    use super::hstore::Pair;
    let mut r = b;
    let count = get_i32(&mut r)?;
    const MAX_PAIRS: i32 = 0x3fff_ffff / 32;
    if !(0..=MAX_PAIRS).contains(&count) {
        return Err(Error::new(
            code::PROGRAM_LIMIT_EXCEEDED,
            format!("number of pairs ({count}) exceeds the maximum allowed ({MAX_PAIRS})"),
        ));
    }
    let mut pairs = Vec::with_capacity(count as usize);
    let text = |r: &mut &[u8], len: i32| -> Result<Vec<u8>> {
        let len = len as usize;
        if r.remaining() < len {
            return Err(short());
        }
        let s = get_text(&r[..len], ctx)?.into_bytes();
        r.advance(len);
        Ok(s)
    };
    for _ in 0..count {
        let klen = get_i32(&mut r)?;
        if klen < 0 {
            return Err(Error::new(code::NULL_VALUE_NOT_ALLOWED, "null value not allowed for hstore key"));
        }
        let key = text(&mut r, klen)?;
        super::hstore::check_key_len(key.len())?;
        let vlen = get_i32(&mut r)?;
        let val = if vlen < 0 {
            None
        } else {
            let v = text(&mut r, vlen)?;
            super::hstore::check_val_len(v.len())?;
            Some(v)
        };
        pairs.push(Pair { key, val, needfree: true });
    }
    Ok(super::hstore::from_pairs(pairs))
}

/// cube_recv: the header (dimensions, the point bit), then the coordinates.
fn cube_recv(b: &[u8]) -> Result<Vec<u8>> {
    const POINT_BIT: u32 = 0x8000_0000;
    const DIM_MASK: u32 = 0x7fff_ffff;
    const CUBE_MAX_DIM: u32 = 100;
    let mut r = b;
    let header = get_i32(&mut r)? as u32;
    let mut n = header & DIM_MASK;
    if n > CUBE_MAX_DIM {
        return Err(Error::new(code::PROGRAM_LIMIT_EXCEEDED, "cube dimension is too large")
            .detail(format!("A cube cannot have more than {CUBE_MAX_DIM} dimensions.")));
    }
    if header & POINT_BIT == 0 {
        n += n;
    }
    let mut out = Vec::with_capacity(4 + 8 * n as usize);
    out.extend_from_slice(&header.to_le_bytes());
    for _ in 0..n {
        if r.remaining() < 8 {
            return Err(short());
        }
        out.extend_from_slice(&r.get_f64().to_le_bytes());
    }
    Ok(out)
}
