//! Live documents (`.liv`, `Lucene90LiveDocsFormat`): one bit per document, set while it lives,
//! for the hard deletes of a commit. Elasticsearch deletes and updates with soft deletes
//! instead ([`crate::docvalues`]'s `__soft_deletes` field); hard deletes are what a merge or a
//! failed write left.

use crate::codec;
use crate::input::Input;
use crate::{Id, Result, corrupt};

/// A segment's live documents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bits {
    words: Vec<u64>,
    len: u32,
}

impl Bits {
    /// Every document of `len` live.
    pub fn all(len: u32) -> Bits {
        let mut words = vec![u64::MAX; (len as usize).div_ceil(64)];
        if len % 64 != 0
            && let Some(last) = words.last_mut()
        {
            *last = (1u64 << (len % 64)) - 1;
        }
        Bits { words, len }
    }

    pub fn len(&self) -> u32 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, doc: u32) -> bool {
        doc < self.len && self.words[(doc / 64) as usize] >> (doc % 64) & 1 == 1
    }

    pub fn clear(&mut self, doc: u32) {
        if doc < self.len {
            self.words[(doc / 64) as usize] &= !(1u64 << (doc % 64));
        }
    }

    pub fn count(&self) -> u32 {
        self.words.iter().map(|w| w.count_ones()).sum()
    }

    /// Bytes held.
    pub fn bytes(len: u32) -> usize {
        (len as usize).div_ceil(64) * 8
    }
}

/// Reads `.liv` at generation `generation` of a segment of `max_doc` documents, `del_count` of
/// them deleted; the footer is checked first.
pub fn read_live_docs(file: &[u8], id: &Id, generation: i64, max_doc: u32, del_count: u32) -> Result<Bits> {
    codec::check_footer(file)?;
    let mut inp = Input::new(codec::body(file)?);
    codec::check_index_header(&mut inp, "Lucene90LiveDocs", 0, 0, id, &crate::base36(generation))?;
    let words = (max_doc as usize).div_ceil(64);
    if inp.remaining() != words * 8 {
        return corrupt(format!("live docs of [{}] bytes for [{max_doc}] documents", inp.remaining()));
    }
    let mut out = Vec::with_capacity(words);
    for _ in 0..words {
        out.push(inp.i64()? as u64);
    }
    // Bits past the last document are never set.
    if max_doc % 64 != 0
        && let Some(last) = out.last_mut()
    {
        *last &= (1u64 << (max_doc % 64)) - 1;
    }
    let bits = Bits { words: out, len: max_doc };
    let deleted = max_doc - bits.count();
    if deleted != del_count {
        return corrupt(format!("bits.deleted={deleted} info.delcount={del_count}"));
    }
    Ok(bits)
}
