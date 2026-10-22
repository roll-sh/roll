//! Shared test helpers: the 1 GiB allocation guard and a random corpus with a brute-force model.
#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::sync::Arc;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use search_codec::{DocValuesType, IndexOptions, PointsSpec};
use search_index::doc::{DocValue, IndexableDoc, IndexableField, PreToken};
use search_index::query::MatchAllQuery;
use search_index::schema::{FieldSchema, ID_FIELD};
use search_index::search::Searcher;
use search_index::{DeleteOp, IndexOp, QueryRef, Schema, Shard, ShardConfig};

/// Fails any single allocation over 1 GiB, as Linux does, so a buffer sized from a hostile
/// count aborts the tests on every platform.
struct Capped;

const MAX_ALLOC: usize = 1 << 30;

unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.alloc(layout) } }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.alloc_zeroed(layout) } }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.realloc(ptr, layout, new_size) } }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Capped = Capped;

pub const BODY: u32 = 10;
pub const TAG: u32 = 11;
pub const NUM: u32 = 12;
pub const TITLE: u32 = 13;

pub fn schema() -> Schema {
    Schema::new()
        .with_field(FieldSchema::new(BODY, "body").indexed(IndexOptions::DocsAndFreqsAndPositionsAndOffsets, true))
        .unwrap()
        .with_field(
            FieldSchema::new(TAG, "tag").indexed(IndexOptions::Docs, false).with_doc_values(DocValuesType::SortedSet),
        )
        .unwrap()
        .with_field(
            FieldSchema::new(NUM, "num")
                .with_points(PointsSpec::new(1, 8))
                .with_doc_values(DocValuesType::SortedNumeric)
                .with_skip_index(),
        )
        .unwrap()
        .with_field(FieldSchema::new(TITLE, "title").indexed(IndexOptions::DocsAndFreqsAndPositions, true))
        .unwrap()
}

/// One model document.
#[derive(Clone, Debug)]
pub struct MDoc {
    pub id: String,
    pub body: Vec<String>,
    pub title: Vec<String>,
    pub tags: Vec<String>,
    pub nums: Vec<i64>,
}

pub const VOCAB: usize = 60;

pub fn word(i: usize) -> String {
    format!("w{i}")
}

/// Zipf-like pick of a word id.
pub fn zipf(rng: &mut StdRng, n: usize) -> usize {
    let u: f64 = rng.r#gen();
    let r = (n as f64).powf(u) - 1.0;
    (r as usize).min(n - 1)
}

pub fn random_doc(rng: &mut StdRng, id: usize) -> MDoc {
    let len = rng.gen_range(0..30);
    let body = (0..len).map(|_| word(zipf(rng, VOCAB))).collect();
    let tl = rng.gen_range(0..5);
    let title = (0..tl).map(|_| word(zipf(rng, 20))).collect();
    let nt = rng.gen_range(0..3);
    let tags = (0..nt).map(|_| format!("t{}", rng.gen_range(0..8))).collect();
    let nn = rng.gen_range(0..3);
    let nums = (0..nn).map(|_| rng.gen_range(-50..50)).collect();
    MDoc { id: format!("d{id}"), body, title, tags, nums }
}

fn tokens(words: &[String]) -> Vec<PreToken> {
    let mut off = 0u32;
    words
        .iter()
        .map(|w| {
            let t = PreToken::new(w.clone().into_bytes(), 1, off, off + w.len() as u32);
            off += w.len() as u32 + 1;
            t
        })
        .collect()
}

pub fn to_indexable(d: &MDoc) -> IndexableDoc {
    let mut doc = IndexableDoc::new(d.id.clone().into_bytes());
    if !d.body.is_empty() {
        doc.fields.push(IndexableField::Tokens { field: BODY, tokens: tokens(&d.body) });
    }
    if !d.title.is_empty() {
        doc.fields.push(IndexableField::Tokens { field: TITLE, tokens: tokens(&d.title) });
    }
    for t in &d.tags {
        doc.fields.push(IndexableField::Term { field: TAG, term: t.clone().into_bytes().into() });
        doc.fields.push(IndexableField::DocValue { field: TAG, value: DocValue::SortedSet(t.clone().into_bytes().into()) });
    }
    for &n in &d.nums {
        doc.fields.push(IndexableField::Point { field: NUM, packed: search_codec::points::encode::i64_bytes(n).to_vec() });
        doc.fields.push(IndexableField::DocValue { field: NUM, value: DocValue::SortedNumeric(n) });
    }
    doc.stored.push(search_index::doc::StoredField {
        field: 20,
        value: search_codec::stored::OwnedStoredValue::Str(d.body.join(" ")),
    });
    doc
}

/// A shard built from random operations plus the model of its live docs.
pub struct Corpus {
    pub shard: Shard,
    pub live: HashMap<String, MDoc>,
}

/// Indexes `n` random docs with random refreshes, updates, deletes and merges.
pub fn build(seed: u64, n: usize, cfg: ShardConfig) -> Corpus {
    let mut rng = StdRng::seed_from_u64(seed);
    let shard = Shard::open(cfg, schema(), Vec::new()).unwrap();
    let mut live: HashMap<String, MDoc> = HashMap::new();
    let mut next = 0usize;
    for _ in 0..n {
        let r: f64 = rng.r#gen();
        if r < 0.75 || live.is_empty() {
            let d = random_doc(&mut rng, next);
            next += 1;
            shard.index(IndexOp::new(to_indexable(&d))).unwrap();
            live.insert(d.id.clone(), d);
        } else if r < 0.88 {
            // Update an existing doc.
            let k = rng.gen_range(0..next);
            let mut d = random_doc(&mut rng, k);
            d.id = format!("d{k}");
            shard.index(IndexOp::new(to_indexable(&d))).unwrap();
            live.insert(d.id.clone(), d);
        } else {
            let k = format!("d{}", rng.gen_range(0..next));
            shard.delete(DeleteOp::new(k.clone().into_bytes())).unwrap();
            live.remove(&k);
        }
        if rng.gen_bool(0.01) {
            shard.refresh().unwrap();
        }
        if rng.gen_bool(0.002) {
            shard.force_merge(Some(rng.gen_range(1..4)), false).unwrap();
        }
    }
    shard.refresh().unwrap();
    Corpus { shard, live }
}

/// Global doc id -> `_id` of every live doc.
pub fn ids(s: &Searcher) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    let q: QueryRef = Arc::new(MatchAllQuery);
    let mut c = search_index::collector::TopScoreCollector::new(s.num_docs() as usize + 1, u64::MAX);
    s.search(&q, &mut c).unwrap();
    for h in c.top_docs().hits {
        let f = s.stored(h.doc).unwrap();
        let id = f.iter().find(|(n, _)| *n == ID_FIELD).unwrap();
        if let search_codec::stored::OwnedStoredValue::Bytes(b) = &id.1 {
            out.insert(h.doc, String::from_utf8(b.clone()).unwrap());
        }
    }
    out
}

pub fn rng(seed: u64) -> StdRng {
    StdRng::seed_from_u64(seed)
}
