//! Flat vectors (`Lucene99FlatVectorsReader`, `.vemf` and `.vec`): each vector field's raw
//! values, one after another by ordinal, and which document each ordinal belongs to
//! (`OrdToDocDISIReaderConfiguration`: every document, or an `IndexedDISI` in `.vec`).
//! Elasticsearch's quantized and HNSW formats keep the raw vectors this way under their own
//! per-field file names, and Elasticsearch 9 leaves them out of `_source`
//! (`index.mapping.exclude_source_vectors`), so a restore reads them back from here.

use crate::codec;
use crate::fields::{FieldInfos, VectorEncoding};
use crate::input::Input;
use crate::{Id, Result, corrupt};

pub const META_CODEC: &str = "Lucene99FlatVectorsFormatMeta";
pub const DATA_CODEC: &str = "Lucene99FlatVectorsFormatData";

#[derive(Clone, Debug, PartialEq)]
pub struct VectorEntry {
    pub field: u32,
    pub encoding: VectorEncoding,
    pub data_offset: u64,
    pub data_length: u64,
    pub dims: u32,
    /// Vectors (ordinals).
    pub size: u32,
    /// `-2` none, `-1` every document (ordinal = document), else where the `IndexedDISI` of the
    /// documents with a vector starts in `.vec`.
    pub docs_offset: i64,
    pub docs_length: i64,
    pub dense_rank_power: i8,
}

impl VectorEntry {
    /// Bytes a vector takes.
    pub fn vector_bytes(&self) -> usize {
        self.dims as usize * if self.encoding == VectorEncoding::Byte { 1 } else { 4 }
    }
}

/// `.vemf`'s entries (footer checked first). `suffix`: the per-field format's
/// (`ES94HnswScalarQuantizedVectorsFormat_0`).
pub fn read_meta(file: &[u8], id: &Id, suffix: &str, fields: &FieldInfos) -> Result<Vec<VectorEntry>> {
    codec::check_footer(file)?;
    let mut inp = Input::new(codec::body(file)?);
    codec::check_index_header(&mut inp, META_CODEC, 0, 0, id, suffix)?;
    let mut out = Vec::new();
    loop {
        let number = inp.i32()?;
        if number == -1 {
            break;
        }
        let Some(info) = fields.by_number(number as u32).filter(|_| number >= 0) else { return corrupt(format!("Invalid field number: {number}")) };
        let encoding = match inp.i32()? {
            0 => VectorEncoding::Byte,
            1 => VectorEncoding::Float32,
            e => return corrupt(format!("Invalid vector encoding id: {e}")),
        };
        let _similarity = inp.i32()?;
        let data_offset = inp.vlong()? as u64;
        let data_length = inp.vlong()? as u64;
        let dims = inp.vuint()?;
        let size = inp.i32()?;
        if size < 0 || dims == 0 || dims > 65536 || dims != info.vector_dims {
            return corrupt(format!("invalid vector field [{}]: [{size}] vectors of [{dims}] dimensions", info.name));
        }
        let docs_offset = inp.i64()?;
        let docs_length = inp.i64()?;
        let _jump = inp.i16()?;
        let dense_rank_power = inp.u8()? as i8;
        if docs_offset > -1 {
            inp.i64()?;
            let shift = inp.vint()?;
            if !(2..=30).contains(&shift) {
                return corrupt(format!("invalid block shift [{shift}]"));
            }
            let blocks = ((size as u64) >> shift) + u64::from(size as u64 & ((1u64 << shift) - 1) != 0);
            if blocks.saturating_mul(21) > inp.remaining() as u64 {
                return corrupt("vector addresses metadata past the end");
            }
            inp.skip(blocks as usize * 21)?;
            inp.i64()?;
        }
        let e = VectorEntry { field: info.number, encoding, data_offset, data_length, dims, size: size as u32, docs_offset, docs_length, dense_rank_power };
        if (e.size as u64).checked_mul(e.vector_bytes() as u64) != Some(data_length) {
            return corrupt(format!("vector data of [{data_length}] bytes for [{size}] vectors of [{dims}] dimensions"));
        }
        out.push(e);
    }
    if inp.remaining() != 0 {
        return corrupt("trailing bytes after the vectors metadata");
    }
    Ok(out)
}

/// A vector's values (bytes as their signed value), which `_source` prints as `f32`s are.
pub fn decode(entry: &VectorEntry, bytes: &[u8]) -> Vec<f32> {
    match entry.encoding {
        VectorEncoding::Byte => bytes.iter().map(|b| *b as i8 as f32).collect(),
        VectorEncoding::Float32 => bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect(),
    }
}
