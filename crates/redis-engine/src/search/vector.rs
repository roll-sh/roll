//! Vector index sizing as VecSim estimates it (`VecSimIndex_EstimateElementSize`) and the
//! memory limit block sizes are checked against (`setMemoryInfo`).

use super::spec::{Algo, Metric, VecType, VectorParams};

/// `VecSimParams_GetStoredDataSize`: the vector, plus its norm for cosine on integers.
pub fn stored_data_size(p: &VectorParams) -> usize {
    let mut n = p.ty.size() * p.dim;
    if p.metric == Metric::Cosine && matches!(p.ty, VecType::Int8 | VecType::Uint8) {
        n += 4;
    }
    n
}

/// Bytes per element: HNSW adds graph links (`ElementGraphData` 32, two `M`-sized lists of
/// u32 ids) and metadata (tag 2, packed `ElementMetaData` 9, a one-byte lock, a lookup
/// bucket pointer 8); FLAT adds a label and a lookup pointer; SVS (unquantized on this
/// platform) a graph node of `graph_max_degree + 1` ids.
pub fn element_size(p: &VectorParams) -> usize {
    match p.algo {
        Algo::Hnsw => {
            let m = if p.m == 0 { 16 } else { p.m };
            (2 + 9 + 1 + 8) + 32 + 4 * m * 2 + stored_data_size(p)
        }
        Algo::Flat => stored_data_size(p) + 8 + 8,
        Algo::Svs => {
            let gmd = if p.graph_max_degree == 0 { 32 } else { p.graph_max_degree };
            p.ty.size() * p.dim + (gmd + 1) * 4
        }
    }
}

/// `memoryLimit`: the smaller non-zero of `maxmemory` and the machine's memory.
pub fn memory_limit(cfg: &crate::config::Config) -> usize {
    let maxmemory = cfg.num("maxmemory") as u64;
    let total = crate::commands::info::system_memory();
    let v = match (maxmemory, total) {
        (0, t) => t,
        (m, 0) => m,
        (m, t) => m.min(t),
    };
    v as usize
}

/// `floatToFP16bits` (ryg's conversion): rounds to nearest, overflow clamps to infinity; NaN
/// isn't handled.
pub fn f32_to_f16_bits(input: f32) -> u16 {
    let sign_mask: u32 = 1 << 31;
    let f16infty: u32 = 31 << 23;
    let round_mask: u32 = !0xfff;
    let magic = f32::from_bits(15 << 23);
    let mut f32b = input.to_bits();
    let sign = f32b & sign_mask;
    f32b ^= sign;
    let fscale = f32::from_bits(f32b & round_mask) * magic;
    let mut f16 = fscale.to_bits().wrapping_sub(round_mask);
    if f16 > f16infty {
        f16 = f16infty;
    }
    ((f16 >> 13) | (sign >> 16)) as u16
}

/// `floatToBF16bits`: round to nearest even, keep the high 16 bits.
pub fn f32_to_bf16_bits(input: f32) -> u16 {
    let f = input.to_bits();
    let lsb = (f >> 16) & 1;
    (f.wrapping_add(lsb + 0x7fff) >> 16) as u16
}
