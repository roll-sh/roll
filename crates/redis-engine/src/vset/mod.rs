//! Vector sets, as the `vectorset` module built into Redis 8.10 keeps them (`vset.c`): an HNSW
//! graph of (optionally projected) vectors plus an element → node map, each element with
//! optional JSON attributes for filtered similarity search.

pub mod expr;
pub mod fp;
pub mod hnsw;

use crate::bloom::{ModuleIn, ModuleOut};
use crate::client::jemalloc_size;
use hnsw::{Hnsw, NIL};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// The module type name (TYPE, SCAN TYPE) and its encoding version.
pub const TYPE_NAME: &str = "vectorset";
pub const ENCVER: u64 = 0;

/// Largest vector dimension accepted.
pub const MAX_DIM: u64 = 1 << 16;
/// HNSW creation EF when VADD gives none.
pub const DEFAULT_CREATE_EF: u32 = 200;

const SAVE_FLAG_HAS_PROJMATRIX: u64 = 1;
const SAVE_FLAG_HAS_ATTRIBS: u64 = 2;

/// `VectorSetTypeNextId`: every vector set object created gets the next id.
static NEXT_UID: AtomicU64 = AtomicU64::new(0);

// What Redis' structures weigh (aarch64, musl), for MEMORY USAGE's estimate.
const SIZEOF_VSET: usize = 112;
const SIZEOF_HNSW: usize = 1672;
const SIZEOF_NODE: usize = 312;
const SIZEOF_LAYER: usize = 24;

#[derive(Clone, Debug)]
pub struct VSet {
    pub hnsw: Hnsw,
    /// Element → node.
    pub dict: BTreeMap<Box<[u8]>, u32>,
    /// The `output × input` projection matrix of a set created with REDUCE.
    pub proj: Option<Box<[f32]>>,
    pub proj_input: u32,
    pub uid: u64,
    /// Elements that have attributes.
    pub numattribs: u64,
}

/// `RM_MallocSizeString` of an element or attribute: an embedded string object for a short
/// command argument, else an object and an exactly sized sds.
fn string_size(len: usize, raw: bool) -> usize {
    if !raw && len <= 44 {
        return jemalloc_size(16 + 3 + len + 1);
    }
    let hdr = match len {
        0..32 => 1,
        32..256 => 3,
        256..65536 => 5,
        _ => 9,
    };
    16 + jemalloc_size(hdr + len + 1)
}

impl VSet {
    /// `createVectorSetObject`.
    pub fn new(dim: u32, quant: u32, m: u32) -> Self {
        VSet {
            hnsw: Hnsw::new(dim, quant, m),
            dict: BTreeMap::new(),
            proj: None,
            proj_input: 0,
            uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
            numattribs: 0,
        }
    }

    pub fn len(&self) -> u64 {
        self.hnsw.g.node_count
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn dim(&self) -> u32 {
        self.hnsw.g.dim
    }

    /// The quantization's name (VINFO, VEMB RAW).
    pub fn quant_name(&self) -> &'static str {
        match self.hnsw.g.quant {
            hnsw::QUANT_NONE => "f32",
            hnsw::QUANT_Q8 => "int8",
            hnsw::QUANT_BIN => "bin",
            _ => "unknown",
        }
    }

    pub fn node(&self, item: &[u8]) -> Option<u32> {
        self.dict.get(item).copied()
    }

    /// `vectorSetInsert`: add `item` (true), or move an existing one to the new vector
    /// (false), replacing its attributes when some are given (empty removes them). A new
    /// element keeps even an empty attribute string.
    pub fn insert(&mut self, vec: &[f32], item: &[u8], attrib: Option<&[u8]>, ef: u32) -> bool {
        if let Some(node) = self.node(item) {
            let old = self.hnsw.delete(node);
            let new = self.hnsw.insert(vec, ef, old.item, old.attrib);
            {
                let n = self.hnsw.g.nodes[new as usize].as_mut().expect("inserted");
                n.item_raw = old.item_raw;
                n.attrib_raw = old.attrib_raw;
                if let Some(a) = attrib {
                    let had = n.attrib.is_some();
                    if a.is_empty() {
                        n.attrib = None;
                    } else {
                        n.attrib = Some(a.into());
                        n.attrib_raw = false;
                        self.numattribs += 1;
                    }
                    if had {
                        self.numattribs -= 1;
                    }
                }
            }
            self.dict.insert(item.into(), new);
            return false;
        }
        let new = self.hnsw.insert(vec, ef, item.into(), attrib.map(Into::into));
        if attrib.is_some() {
            self.numattribs += 1;
        }
        self.dict.insert(item.into(), new);
        true
    }

    /// VREM: whether the element was there.
    pub fn remove(&mut self, item: &[u8]) -> bool {
        let Some(node) = self.dict.remove(item) else { return false };
        if self.hnsw.g.n(node).attrib.is_some() {
            self.numattribs -= 1;
        }
        self.hnsw.delete(node);
        true
    }

    /// VSETATTR on an existing element: an empty string removes the attributes.
    pub fn set_attrib(&mut self, node: u32, a: &[u8]) {
        let n = self.hnsw.g.nodes[node as usize].as_mut().expect("live node");
        if a.is_empty() {
            if n.attrib.take().is_some() {
                self.numattribs -= 1;
            }
        } else {
            if n.attrib.is_none() {
                self.numattribs += 1;
            }
            n.attrib = Some(a.into());
            n.attrib_raw = false;
        }
    }

    /// `VectorSetMemUsage`: an estimate from the structure sizes and the first elements.
    pub fn mem_usage(&self) -> u64 {
        let g = &self.hnsw.g;
        let mut size = SIZEOF_VSET + SIZEOF_HNSW;
        if self.proj.is_some() {
            size += 4 * self.proj_input as usize * g.dim as usize;
        }
        if g.head == NIL {
            return size as u64;
        }
        let n = g.node_count as usize;
        size += SIZEOF_NODE * n;
        size += self.hnsw.quants_bytes() * n;
        size += SIZEOF_LAYER * n * 4 / 3;
        let level0 = g.n(g.head).layers[0].max_links as usize;
        size += 8 * level0 * n;
        size += 8 * (level0 / 2) * n / 3;
        let (mut items, mut items_size, mut attribs, mut attribs_size) = (0usize, 0usize, 0usize, 0usize);
        for (_, node) in g.iter().take(20) {
            items_size += string_size(node.item.len(), node.item_raw);
            items += 1;
            if let Some(a) = &node.attrib {
                attribs_size += string_size(a.len(), node.attrib_raw);
                attribs += 1;
            }
        }
        if let Some(per) = items_size.checked_div(items) {
            size += per * n;
        }
        if attribs == 0 {
            attribs = 1;
            attribs_size = 64;
        }
        size += attribs_size / attribs * self.numattribs as usize;
        size += self.dict.len() * 16;
        size as u64
    }

    /// `VectorSetDigest`: the sequences it adds (count and dimension, then per node its
    /// vector bytes, element and attributes).
    pub fn digest_sequences(&self, mut f: impl FnMut(&[&[u8]])) {
        let count = self.len().to_string();
        let dim = self.dim().to_string();
        f(&[count.as_bytes(), dim.as_bytes()]);
        for (_, n) in self.hnsw.g.iter() {
            let v = n.v.bytes();
            match &n.attrib {
                Some(a) => f(&[&v, &n.item, a]),
                None => f(&[&v, &n.item]),
            }
        }
    }

    /// `VectorSetRdbSave`.
    pub fn save(&self, out: &mut impl ModuleOut) {
        let g = &self.hnsw.g;
        out.uint(g.dim as u64);
        out.uint(g.node_count);
        out.uint((g.quant as u64 & 0xff) | ((g.m as u64 & 0xffff) << 8));
        let mut flags = 0;
        if self.proj.is_some() {
            flags |= SAVE_FLAG_HAS_PROJMATRIX;
        }
        if self.numattribs != 0 {
            flags |= SAVE_FLAG_HAS_ATTRIBS;
        }
        out.uint(flags);
        if let Some(m) = &self.proj {
            out.uint(self.proj_input as u64);
            let bytes: Vec<u8> = m.iter().flat_map(|x| x.to_le_bytes()).collect();
            out.buffer(&bytes);
        }
        for (i, n) in g.iter() {
            out.buffer(&n.item);
            if self.numattribs != 0 {
                out.buffer(n.attrib.as_deref().unwrap_or(b""));
            }
            out.buffer(&n.v.bytes());
            let params = self.hnsw.serialize(i);
            out.uint(params.len() as u64);
            for p in params {
                out.uint(p);
            }
        }
    }

    /// `VectorSetRdbLoad`.
    pub fn load(encver: u64, r: &mut impl ModuleIn) -> Option<VSet> {
        if encver != 0 {
            return None;
        }
        let dim = r.uint()? as u32;
        let elements = r.uint()?;
        let config = r.uint()? as u32;
        let quant = config & 0xff;
        let mut m = (config >> 8) & 0xffff;
        if dim == 0 || dim as u64 > MAX_DIM {
            return None;
        }
        if !matches!(quant, hnsw::QUANT_NONE | hnsw::QUANT_Q8 | hnsw::QUANT_BIN) {
            return None;
        }
        if m == 0 {
            m = 16;
        }
        let mut v = VSet::new(dim, quant, m);
        let flags = r.uint()?;
        if flags & SAVE_FLAG_HAS_PROJMATRIX != 0 {
            let input = r.uint()? as u32;
            if input == 0 || input as u64 > MAX_DIM || dim > input {
                return None;
            }
            let blob = r.buffer()?;
            if blob.len() != 4 * input as usize * dim as usize {
                return None;
            }
            v.proj = Some(blob.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect());
            v.proj_input = input;
        }
        let has_attribs = flags & SAVE_FLAG_HAS_ATTRIBS != 0;
        let mut pending = Vec::new();
        for _ in 0..elements {
            let item: Box<[u8]> = r.buffer()?.into();
            let attrib = if has_attribs {
                let a = r.buffer()?;
                (!a.is_empty()).then(|| a.into_boxed_slice())
            } else {
                None
            };
            let vector = r.buffer()?;
            if vector.len() != v.hnsw.quants_bytes() {
                return None;
            }
            let count = r.uint()? as u32;
            let mut params = Vec::with_capacity(count.min(1 << 16) as usize);
            for _ in 0..count {
                params.push(r.uint()?);
            }
            let has = attrib.is_some();
            let node = v.hnsw.insert_serialized(&vector, &params, item.clone(), attrib, &mut pending)?;
            {
                let n = v.hnsw.g.nodes[node as usize].as_mut().expect("inserted");
                n.item_raw = true;
                n.attrib_raw = true;
            }
            if has {
                v.numattribs += 1;
            }
            v.dict.entry(item).or_insert(node);
        }
        if !v.hnsw.deserialize(pending) {
            return None;
        }
        Some(v)
    }

    /// The vector as the graph stores it (projected when the set has a projection).
    pub fn project(&self, vec: &[f32]) -> Vec<f32> {
        match &self.proj {
            Some(m) => fp::project(vec, m, self.dim()),
            None => vec.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Buf {
        items: Vec<Item>,
        at: usize,
    }

    enum Item {
        U(u64),
        B(Vec<u8>),
    }

    impl ModuleOut for Buf {
        fn uint(&mut self, v: u64) {
            self.items.push(Item::U(v));
        }
        fn double(&mut self, _: f64) {
            unreachable!()
        }
        fn buffer(&mut self, b: &[u8]) {
            self.items.push(Item::B(b.to_vec()));
        }
    }

    impl ModuleIn for Buf {
        fn uint(&mut self) -> Option<u64> {
            self.at += 1;
            match self.items.get(self.at - 1)? {
                Item::U(v) => Some(*v),
                _ => None,
            }
        }
        fn double(&mut self) -> Option<f64> {
            None
        }
        fn buffer(&mut self) -> Option<Vec<u8>> {
            self.at += 1;
            match self.items.get(self.at - 1)? {
                Item::B(v) => Some(v.clone()),
                _ => None,
            }
        }
    }

    #[test]
    fn insert_update_remove_and_reload() {
        let mut v = VSet::new(3, hnsw::QUANT_Q8, 16);
        assert!(v.insert(&[1.0, 0.0, 0.0], b"a", None, 200));
        assert!(v.insert(&[0.0, 1.0, 0.0], b"b", Some(b"{\"x\":1}"), 200));
        assert!(v.insert(&[0.0, 0.0, 1.0], b"c", Some(b""), 200));
        assert_eq!(v.numattribs, 2);
        assert!(!v.insert(&[0.5, 0.5, 0.0], b"a", Some(b"{\"y\":2}"), 200));
        assert_eq!(v.numattribs, 3);
        assert!(!v.insert(&[0.5, 0.5, 0.0], b"a", Some(b""), 200));
        assert_eq!(v.numattribs, 2);
        assert_eq!(v.len(), 3);
        let mut b = Buf::default();
        v.save(&mut b);
        let l = VSet::load(0, &mut b).expect("loads");
        assert_eq!(l.len(), 3);
        // An empty attribute string is not kept by a reload.
        assert_eq!(l.numattribs, 1);
        assert_eq!(l.hnsw.g.last_id, v.hnsw.g.last_id);
        let mut v = l;
        assert!(v.remove(b"b"));
        assert!(!v.remove(b"b"));
        assert_eq!(v.numattribs, 0);
        assert_eq!(v.len(), 2);
    }
}
