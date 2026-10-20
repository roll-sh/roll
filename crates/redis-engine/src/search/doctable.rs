//! The document table (`doc_table.c`): incremental document ids, their metadata (key, score,
//! flags, sorting vector, payload, expiration) and the key → id map, with the module's memory
//! accounting (`memsize`, `sortablesSize`) to the byte.

use super::spec::DocType;
use super::triemap::TrieMap;
use bytes::Bytes;
use std::collections::HashMap;

pub const DOC_DELETED: u8 = 0x01;
pub const DOC_HAS_PAYLOAD: u8 = 0x02;
pub const DOC_HAS_SORT_VECTOR: u8 = 0x04;
pub const DOC_HAS_OFFSET_VECTOR: u8 = 0x08;
pub const DOC_HAS_EXPIRATION: u8 = 0x10;

/// `INITIAL_DOC_TABLE_SIZE`.
pub const INITIAL_SIZE: usize = 1000;
/// `sizeof(DocTable)`.
const TABLE_SIZE: usize = 72;
/// `sizeof(DMDChain)`.
const CHAIN_SIZE: usize = 8;
/// `sizeof(RSDocumentMetadata)`; without the trailing payload pointer when there's none.
const DMD_SIZE: usize = 72;
const DMD_LEAN_SIZE: usize = DMD_SIZE - 8;
/// `sizeof(RSPayload)`.
const PAYLOAD_SIZE: usize = 16;
/// `sizeof(SharedValue)` and `SHARED_VALUE_CONTENT_SIZE` (sorting vector accounting).
const SV_SLOT: usize = 8;
const SV_CONTENT: usize = 32;

/// hiredis' `sdsAllocSize` for `sdsnewlen(s, n)`: header, bytes and terminator.
pub fn sds_alloc_size(n: usize) -> usize {
    let hdr = match n {
        0 => 3,
        1..32 => 1,
        32..256 => 3,
        256..65536 => 5,
        _ if n < 1 << 32 => 9,
        _ => 17,
    };
    hdr + n + 1
}

/// A sorting vector slot (`RSValue`).
#[derive(Clone, Debug, PartialEq)]
pub enum SortVal {
    Null,
    Num(f64),
    Str(Bytes),
    /// A multi-value (JSON array) slot: counted without its contents.
    Multi(Vec<SortVal>),
}

/// `RSSortingVector_GetMemorySize`.
pub fn sort_vector_size(v: &[SortVal]) -> usize {
    v.iter()
        .map(|x| match x {
            SortVal::Null => SV_SLOT,
            SortVal::Num(_) | SortVal::Multi(_) => SV_SLOT + SV_CONTENT,
            SortVal::Str(s) => SV_SLOT + SV_CONTENT + s.len(),
        })
        .sum()
}

/// `RSDocumentMetadata`.
#[derive(Clone, Debug)]
pub struct Dmd {
    pub id: u64,
    pub key: Bytes,
    pub score: f32,
    pub max_freq: u32,
    pub flags: u8,
    pub doc_len: u32,
    pub doc_type: DocType,
    /// Document expiration (Unix ms, 0 = none).
    pub expire_at: i64,
    /// Field expirations (field index, Unix ms), sorted by field index.
    pub field_expirations: Vec<(usize, i64)>,
    pub sort: Vec<SortVal>,
    pub payload: Option<Bytes>,
    /// Byte offsets of every text token, per text field (`RSByteOffsets`) for highlighting.
    pub byte_offsets: Option<ByteOffsets>,
}

/// `RSByteOffsets`: the token positions' byte offsets and each field's token range.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ByteOffsets {
    /// (text field id, first token position, last token position).
    pub fields: Vec<(usize, u32, u32)>,
    pub offsets: Vec<u32>,
}

#[derive(Clone, Debug)]
pub struct DocTable {
    pub size: usize,
    pub max_size: u64,
    pub max_doc_id: u64,
    pub cap: usize,
    pub memsize: usize,
    pub sortables_size: usize,
    docs: HashMap<u64, Dmd>,
    /// `DocIdMap`: key → id.
    pub dim: TrieMap<u64>,
}

impl DocTable {
    /// `NewDocTable(INITIAL_DOC_TABLE_SIZE, search-max-doctablesize)`.
    pub fn new(max_size: u64) -> Self {
        DocTable {
            size: 0,
            max_size,
            max_doc_id: 0,
            cap: INITIAL_SIZE,
            memsize: INITIAL_SIZE * CHAIN_SIZE + TABLE_SIZE,
            sortables_size: 0,
            docs: HashMap::new(),
            dim: TrieMap::new(),
        }
    }

    fn bucket(&self, id: u64) -> usize {
        (if id < self.max_size { id } else { id % self.max_size.max(1) }) as usize
    }

    /// `DocTable_Set`'s growth: half the capacity more (at most 1M), up to `maxSize`.
    fn grow_for(&mut self, id: u64) {
        let bucket = self.bucket(id);
        if bucket >= self.cap && (self.cap as u64) < self.max_size {
            let old = self.cap;
            self.cap += 1 + if self.cap > 0 { (self.cap / 2).min(1024 * 1024) } else { 1 };
            self.cap = self.cap.min(self.max_size as usize).max(bucket + 1);
            self.memsize += (self.cap - old) * CHAIN_SIZE;
        }
    }

    pub fn get_id(&self, key: &[u8]) -> u64 {
        self.dim.get(key).copied().unwrap_or(0)
    }

    pub fn get(&self, id: u64) -> Option<&Dmd> {
        self.docs.get(&id).filter(|d| d.flags & DOC_DELETED == 0)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Dmd> {
        self.docs.get_mut(&id).filter(|d| d.flags & DOC_DELETED == 0)
    }

    pub fn by_key(&self, key: &[u8]) -> Option<&Dmd> {
        self.get(self.get_id(key))
    }

    pub fn by_key_mut(&mut self, key: &[u8]) -> Option<&mut Dmd> {
        let id = self.get_id(key);
        self.get_mut(id)
    }

    pub fn exists(&self, id: u64) -> bool {
        id != 0 && id <= self.max_doc_id && self.get(id).is_some()
    }

    /// The documents' keys in `DOCTABLE_FOREACH` order (by id).
    pub fn keys_by_id(&self) -> Vec<Bytes> {
        let mut v: Vec<&Dmd> = self.docs.values().collect();
        v.sort_by_key(|d| d.id);
        v.into_iter().map(|d| d.key.clone()).collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Dmd> {
        self.docs.values()
    }

    /// `DocTable_Put`: a new id for a key not in the table (else the existing document).
    pub fn put(&mut self, key: &[u8], score: f64, mut flags: u8, payload: Option<&[u8]>, doc_type: DocType) -> u64 {
        let existing = self.get_id(key);
        if existing != 0 {
            return existing;
        }
        self.max_doc_id += 1;
        let id = self.max_doc_id;
        let payload = payload.filter(|p| !p.is_empty());
        if let Some(p) = payload {
            flags |= DOC_HAS_PAYLOAD;
            self.memsize += DMD_SIZE + p.len() + PAYLOAD_SIZE;
        } else {
            self.memsize += DMD_LEAN_SIZE;
        }
        self.grow_for(id);
        self.size += 1;
        self.memsize += sds_alloc_size(key.len());
        self.dim.insert(key, id);
        self.docs.insert(
            id,
            Dmd {
                id,
                key: Bytes::copy_from_slice(key),
                score: score as f32,
                max_freq: 1,
                flags,
                doc_len: 0,
                doc_type,
                expire_at: 0,
                field_expirations: Vec::new(),
                sort: Vec::new(),
                payload: payload.map(Bytes::copy_from_slice),
                byte_offsets: None,
            },
        );
        id
    }

    /// `DocTable_SetSortingVector`.
    pub fn set_sort_vector(&mut self, id: u64, v: Vec<SortVal>) {
        let size = sort_vector_size(&v);
        if let Some(d) = self.docs.get_mut(&id) {
            d.sort = v;
            d.flags |= DOC_HAS_SORT_VECTOR;
            self.sortables_size += size;
        }
    }

    /// `DocTable_Pop`: the document leaves the table (its id is never reused).
    pub fn pop(&mut self, key: &[u8]) -> Option<Dmd> {
        let id = self.get_id(key);
        if id == 0 || id > self.max_doc_id {
            return None;
        }
        let mut d = self.docs.remove(&id)?;
        d.flags |= DOC_DELETED;
        self.memsize -= sds_alloc_size(d.key.len());
        match &d.payload {
            None => self.memsize -= DMD_LEAN_SIZE,
            Some(p) => self.memsize -= DMD_SIZE + p.len() + PAYLOAD_SIZE,
        }
        if !d.sort.is_empty() {
            self.sortables_size -= sort_vector_size(&d.sort);
        }
        self.dim.remove(key);
        self.size -= 1;
        Some(d)
    }

    /// `DocTable_Replace`: a renamed key keeps its document.
    pub fn replace(&mut self, from: &[u8], to: &[u8]) -> bool {
        let id = self.get_id(from);
        if id == 0 {
            return false;
        }
        self.dim.remove(from);
        self.dim.insert(to, id);
        if let Some(d) = self.docs.get_mut(&id) {
            self.memsize -= sds_alloc_size(d.key.len());
            d.key = Bytes::copy_from_slice(to);
            self.memsize += sds_alloc_size(to.len());
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_sizes() {
        // testDocTableInfo: 72 + 1000 * 8, then 67 per one-letter key.
        let mut t = DocTable::new(1_000_000);
        assert_eq!(t.memsize, 8072);
        t.put(b"a", 1.0, 0, None, DocType::Hash);
        t.put(b"b", 1.0, 0, None, DocType::Hash);
        assert_eq!(t.memsize, 8072 + 134);
        // FT.INFO of two docs zzd1, zzd2: 8212.
        let mut t = DocTable::new(1_000_000);
        t.put(b"zzd1", 1.0, 0, None, DocType::Hash);
        t.put(b"zzd2", 1.0, 0, None, DocType::Hash);
        assert_eq!(t.memsize, 8212);
        t.set_sort_vector(1, vec![SortVal::Str(Bytes::from_static(b"hello world running"))]);
        t.set_sort_vector(2, vec![SortVal::Str(Bytes::from_static(b"foo bar"))]);
        assert_eq!(t.sortables_size, 106);
        assert_eq!(t.dim.mem_usage(), 120);
        let d = t.pop(b"zzd1").unwrap();
        assert_eq!(d.id, 1);
        assert_eq!(t.put(b"zzd1", 1.0, 0, None, DocType::Hash), 3);
        assert_eq!(t.memsize, 8212);
    }

    #[test]
    fn growth() {
        let mut t = DocTable::new(1_000_000);
        for i in 0..1000 {
            t.put(format!("k{i}").as_bytes(), 1.0, 0, None, DocType::Hash);
        }
        assert_eq!(t.cap, 1501);
    }
}
