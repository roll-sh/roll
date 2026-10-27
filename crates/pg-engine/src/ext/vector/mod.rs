//! pgvector 0.8.7 (`vector`, github.com/pgvector/pgvector at v0.8.7): the vector, halfvec and
//! sparsevec types, the distance functions over them and over `bit`, and the ivfflat and hnsw
//! index access methods. Its own install script (`files/vector--0.8.7.sql`) runs unchanged;
//! its C functions are the native code here, by symbol.
//!
//! Values are `Value::Ext` of kinds `VECTOR` (the floats, little-endian f32), `HALFVEC` (the
//! halves, little-endian IEEE binary16 bits) and `SPARSEVEC` (dim, nnz, the indices and the
//! values, little-endian i32 / f32). Distances are computed as the reference build computes
//! them (`dist`): the same float operations in the same order, so results are bit for bit
//! pgvector's.

mod bitvec;
mod common;
pub(crate) mod dist;
mod halfvec;
mod hnsw;
mod ivfflat;
mod sparsevec;
#[cfg(test)]
mod tests;
#[allow(clippy::module_inception)]
mod vector;

use super::native::Native;
use super::types::{HALFVEC, SPARSEVEC, VECTOR};
use crate::Result;
use bytes::BytesMut;

pub(crate) use halfvec::TYPE as HALFVEC_TYPE;
pub(crate) use sparsevec::TYPE as SPARSEVEC_TYPE;
pub(crate) use vector::TYPE as VECTOR_TYPE;

/// The C function `sym` of library `vector`.
pub(super) fn symbol(sym: &str) -> Option<Native> {
    // The access method handlers and their private support functions take `internal`: the
    // index runs them itself.
    if matches!(sym, "ivfflathandler" | "hnswhandler") || (sym.ends_with("_support") && (sym.starts_with("ivfflat_") || sym.starts_with("hnsw_"))) {
        return Some(Native::scalar(|_, _| Err(crate::error::internal("index access method support functions are run by the index itself"))));
    }
    vector::symbol(sym)
        .or_else(|| halfvec::symbol(sym))
        .or_else(|| sparsevec::symbol(sym))
        .or_else(|| bitvec::symbol(sym))
}

/// The index access method routine handler `sym` returns.
pub(crate) fn am_routine(sym: &str) -> Option<&'static crate::ext::index_am::AmRoutine> {
    match sym {
        "ivfflathandler" => Some(&ivfflat::ROUTINE),
        "hnswhandler" => Some(&hnsw::ROUTINE),
        _ => None,
    }
}

/// At the start of an index build. pgvector's builds draw from the backend's generator
/// (pg_global_prng_state, seeded at random when the session starts), so two builds of the
/// same data differ; built with IVFFLAT_BENCH / HNSW_MEMORY it seeds it with 42 first. With
/// the environment variable ROLL_VECTOR_BUILD_SEED set (to the seed) the engine does the same,
/// so its builds can be compared with such a reference (the equality tests).
pub(crate) fn seed_build(ctx: &crate::exec::ExecCtx) {
    static SEED: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    let seed = SEED.get_or_init(|| std::env::var("ROLL_VECTOR_BUILD_SEED").ok().and_then(|s| s.trim().parse().ok()));
    if let Some(s) = seed {
        ctx.sess.global_seed(*s);
    }
}

/// The type's typmod_in over the modifiers given (`vector(3)`): the typmod.
pub(crate) fn typmod_in(kind: u16, mods: &[i64]) -> Option<Result<i32>> {
    Some(match kind {
        VECTOR => vector::typmod_in(mods),
        HALFVEC => halfvec::typmod_in(mods),
        SPARSEVEC => sparsevec::typmod_in(mods),
        _ => return None,
    })
}

/// The type's length coercion (`vector(vector, integer, boolean)`): an error when value `b`
/// doesn't fit typmod `typmod` (>= 0).
pub(crate) fn check_typmod(kind: u16, b: &[u8], typmod: i32) -> Result<()> {
    match kind {
        VECTOR => vector::check_typmod(b, typmod),
        HALFVEC => halfvec::check_typmod(b, typmod),
        SPARSEVEC => sparsevec::check_typmod(b, typmod),
        _ => Ok(()),
    }
}

/// The type's send function (`vector_send`, ...).
pub(crate) fn send(kind: u16, b: &[u8], out: &mut BytesMut) -> Result<()> {
    match kind {
        VECTOR => vector::send(b, out),
        HALFVEC => halfvec::send(b, out),
        SPARSEVEC => sparsevec::send(b, out),
        _ => Ok(()),
    }
}

/// The type's receive function (`vector_recv`, ...): the value's bytes, with typmod
/// `typmod` checked (-1: none).
pub(crate) fn recv(kind: u16, buf: &[u8], typmod: i32) -> Result<Vec<u8>> {
    match kind {
        VECTOR => vector::recv(buf, typmod),
        HALFVEC => halfvec::recv(buf, typmod),
        _ => sparsevec::recv(buf, typmod),
    }
}
