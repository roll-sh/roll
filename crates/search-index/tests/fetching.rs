//! Segments opened while their files are still downloading (`SegmentHandle::fetching`).
//!
//! The test gate serves a sparse copy of each segment file: every byte reads as zero until a
//! `wait_ready` call writes the real bytes of the asked ranges into the file under the live
//! memory map. A reader that touched a range without asking would read zeros, so comparing
//! every query family, sort, explanation, stored fetch and realtime get against the same shard
//! opened normally proves each read path waits on the gate; the ranges asked for show the open
//! stays lazy (only what readers parse, never the bulk of postings or documents).

mod common;

use std::collections::HashMap;
use std::fs::File;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use common::*;
use parking_lot::Mutex;
use search_codec::container::{ContainerReader, SectionKind};
use search_codec::{IndexOptions, Slice};
use search_index::collector::{FieldSort, Missing, SortKey, SortMode, SortType, TopFieldCollector, TopScoreCollector};
use search_index::doc::IndexableField;
use search_index::query::feature::feature_token;
use search_index::query::{
    BooleanQuery, CombinedFieldQuery, CoveringQuery, MinMatch, MultiPhrasePrefixQuery, DisMaxQuery, FeatureQuery, FieldExistsQuery, IntervalQuery, IntervalsSource,
    MatchAllQuery, MultiPhraseQuery, MultiTermQuery, Occur, PhraseQuery, PointRangeQuery, RewriteMethod, SliceBy,
    SliceQuery, SpanNode, SpanQuery, SynonymQuery, TermQuery,
};
use search_index::merge::policy::MergePolicyConfig;
use search_index::schema::FieldSchema;
use search_index::{
    DeleteOp, IndexOp, QueryRef, Schema, SegmentGate, SegmentHandle, Shard, ShardConfig,
};

const FEATURE: u32 = 30;

fn schema_f() -> Schema {
    schema().with_field(FieldSchema::new(FEATURE, "_feature").indexed(IndexOptions::DocsAndFreqs, false)).unwrap()
}

/// Serves a sparse copy of `real`; ranges become readable when someone waits for them.
struct TestGate {
    real: Vec<u8>,
    file: File,
    ready: Mutex<Vec<Range<u64>>>,
    asked: Mutex<Vec<Range<u64>>>,
    prioritized: Mutex<Vec<Range<u64>>>,
    waits: AtomicUsize,
    fail: AtomicBool,
}

impl TestGate {
    fn new(real: Vec<u8>, path: &Path) -> Arc<TestGate> {
        let file = File::options().create(true).truncate(true).read(true).write(true).open(path).unwrap();
        file.set_len(real.len() as u64).unwrap();
        Arc::new(TestGate {
            real,
            file,
            ready: Mutex::new(Vec::new()),
            asked: Mutex::new(Vec::new()),
            prioritized: Mutex::new(Vec::new()),
            waits: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        })
    }

    fn covered(ready: &[Range<u64>], r: &Range<u64>) -> bool {
        // `ready` is sorted and merged.
        r.is_empty() || ready.iter().any(|x| x.start <= r.start && r.end <= x.end)
    }

    fn mark(ready: &mut Vec<Range<u64>>, r: Range<u64>) {
        ready.push(r);
        ready.sort_by_key(|x| x.start);
        let mut out: Vec<Range<u64>> = Vec::with_capacity(ready.len());
        for x in ready.drain(..) {
            match out.last_mut() {
                Some(l) if x.start <= l.end => l.end = l.end.max(x.end),
                _ => out.push(x),
            }
        }
        *ready = out;
    }

    /// Bytes of section `(kind, field)` readable yet.
    fn section_ready(&self, kind: SectionKind, field: u32) -> bool {
        let c = ContainerReader::open(Slice::from(self.real.clone())).unwrap();
        let Some(e) = c.entry(kind, field) else { return true };
        Self::covered(&self.ready.lock(), &(e.offset..e.offset + e.len))
    }
}

impl SegmentGate for TestGate {
    fn is_ready(&self, range: Range<u64>) -> bool {
        Self::covered(&self.ready.lock(), &range)
    }
    fn prioritize(&self, ranges: &[Range<u64>]) {
        self.prioritized.lock().extend(ranges.iter().cloned());
    }
    fn wait_ready(&self, ranges: &[Range<u64>]) -> std::io::Result<()> {
        if self.fail.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("download failed"));
        }
        self.waits.fetch_add(1, Ordering::Relaxed);
        let mut ready = self.ready.lock();
        for r in ranges {
            assert!(r.start <= r.end && r.end <= self.real.len() as u64, "range {r:?} outside the file");
            self.asked.lock().push(r.clone());
            if !Self::covered(&ready, r) {
                self.file.write_all_at(&self.real[r.start as usize..r.end as usize], r.start).unwrap();
                Self::mark(&mut ready, r.clone());
            }
        }
        Ok(())
    }
    fn is_complete(&self) -> bool {
        Self::covered(&self.ready.lock(), &(0..self.real.len() as u64))
    }
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("roll-search-index-fetching-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A flushed shard with several segments and deletions, and the model of its live docs.
/// Segment bytes with their encoded live docs.
type Segs = Vec<(Vec<u8>, Option<Vec<u8>>)>;

fn flushed(dir: &Path, seed: u64) -> (Segs, HashMap<String, MDoc>, i64) {
    // Keep every refreshed segment (the policy would merge them away for their deletes).
    let merge_policy = MergePolicyConfig { deletes_pct_allowed: 50.0, segments_per_tier: 50.0, ..Default::default() };
    let cfg = ShardConfig { dir: Some(dir.join("src")), merge_threads: 0, bp_reorder: seed.is_multiple_of(2), merge_policy, ..Default::default() };
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let s = Shard::open(cfg, schema_f(), Vec::new()).unwrap();
    let mut r = rng(seed);
    let mut live = HashMap::new();
    for i in 0..1500 {
        let d = random_doc(&mut r, i % 1100);
        let mut doc = to_indexable(&d);
        if let Some(w) = d.body.first() {
            let v = 1.0 + (d.body.len() as f32);
            doc.fields.push(IndexableField::Tokens { field: FEATURE, tokens: vec![feature_token("_feature", w.clone().into_bytes(), v).unwrap()] });
        }
        s.index(IndexOp::new(doc)).unwrap();
        live.insert(d.id.clone(), d);
        if i % 400 == 399 {
            s.refresh().unwrap();
        }
    }
    for k in (0..1100).step_by(7) {
        let id = format!("d{k}");
        s.delete(DeleteOp::new(id.clone().into_bytes())).unwrap();
        live.remove(&id);
    }
    s.refresh().unwrap();
    let out = s.flush().unwrap();
    assert!(out.segments.len() >= 3, "{} segments", out.segments.len());
    let segs = out.segments.iter().map(|f| (f.data.to_vec(), f.live_docs.clone())).collect();
    (segs, live, out.local_checkpoint)
}

fn queries() -> Vec<(&'static str, QueryRef)> {
    let t = |f: u32, w: &str| -> QueryRef { Arc::new(TermQuery::new(f, w.as_bytes().to_vec())) };
    let words = |ws: &[&str]| ws.iter().map(|w| w.as_bytes().to_vec()).collect::<Vec<_>>();
    let span = |w: &str| SpanNode::Term { field: BODY, term: w.as_bytes().to_vec() };
    vec![
        ("term", t(BODY, "w1")),
        ("term title", t(TITLE, "w2")),
        ("term tag", t(TAG, "t3")),
        ("and", Arc::new(BooleanQuery::new().add(Occur::Must, t(BODY, "w0")).add(Occur::Must, t(BODY, "w2")))),
        ("or", Arc::new(BooleanQuery::new().add(Occur::Should, t(BODY, "w3")).add(Occur::Should, t(TITLE, "w1")))),
        ("not", Arc::new(BooleanQuery::new().add(Occur::Must, t(BODY, "w0")).add(Occur::MustNot, t(TAG, "t1")))),
        ("phrase", Arc::new(PhraseQuery::new(BODY, words(&["w0", "w1"]), 0))),
        ("sloppy", Arc::new(PhraseQuery::new(BODY, words(&["w0", "w2", "w1"]), 3))),
        ("multiphrase", Arc::new(MultiPhraseQuery::new(BODY, vec![words(&["w0", "w4"]), words(&["w1"])], vec![0, 1], 0).unwrap())),
        ("prefix const", Arc::new(MultiTermQuery::prefix(BODY, b"w1".to_vec()))),
        ("prefix scoring", Arc::new(MultiTermQuery::prefix(BODY, b"w2".to_vec()).with_rewrite(RewriteMethod::ScoringBoolean))),
        ("wildcard", Arc::new(MultiTermQuery::wildcard(TITLE, "w1*", false).unwrap())),
        ("fuzzy", Arc::new(MultiTermQuery::fuzzy(BODY, "w13", 1, 0, 50, true))),
        ("synonym", Arc::new(SynonymQuery::new(BODY, words(&["w5", "w6"])))),
        ("dismax", Arc::new(DisMaxQuery::new(vec![t(BODY, "w7"), t(TITLE, "w7")], 0.3))),
        ("combined", Arc::new(CombinedFieldQuery::new(&[(BODY, 1.0), (TITLE, 2.0)], b"w3".to_vec()).unwrap())),
        ("span near", Arc::new(SpanQuery::new(SpanNode::Near { clauses: vec![span("w0"), span("w1")], slop: 2, in_order: false }).unwrap())),
        ("span or", Arc::new(SpanQuery::new(SpanNode::Or(vec![span("w8"), span("w9")])).unwrap())),
        (
            "intervals",
            Arc::new(IntervalQuery::new(BODY, IntervalsSource::ordered(vec![IntervalsSource::term("w0"), IntervalsSource::term("w2")]))),
        ),
        ("exists body", Arc::new(FieldExistsQuery { field: BODY })),
        ("exists tag", Arc::new(FieldExistsQuery { field: TAG })),
        ("range", Arc::new(PointRangeQuery::long(NUM, -10, 20))),
        ("feature", FeatureQuery::saturation(FEATURE, b"w0".to_vec(), 2.0, Some(5.0)).unwrap()),
        ("slice", Arc::new(SliceQuery::new(SliceBy::Terms { field: TAG, sliced_id_only: false }, 1, 3).unwrap())),
        ("phrase prefix", Arc::new(MultiPhrasePrefixQuery::new(BODY, vec![words(&["w0"]), words(&["w1"])], vec![0, 1], 0, 50).unwrap())),
        ("terms set", Arc::new(CoveringQuery { queries: vec![t(BODY, "w0"), t(BODY, "w1"), t(TITLE, "w0")], min: MinMatch::Constant(2) })),
        ("all", Arc::new(MatchAllQuery)),
    ]
}

/// Everything a search can observe: top hits with scores and `_id`s, counts, a sort,
/// explanations and stored fields of the hits.
fn observe(s: &Shard, q: &QueryRef) -> Vec<String> {
    let se = s.searcher();
    let mut out = Vec::new();
    let mut c = TopScoreCollector::new(40, u64::MAX);
    se.search(q, &mut c).unwrap();
    let td = c.top_docs();
    out.push(format!("total {}", td.total_hits));
    for h in &td.hits {
        let f = se.stored(h.doc).unwrap();
        out.push(format!("{:?} {} {:?}", f, h.score, se.explain(q, h.doc).unwrap().value()));
    }
    out.push(format!("count {}", se.count(q).unwrap()));
    let key = SortKey::Field(FieldSort { field: NUM, kind: SortType::Long, reverse: true, missing: Missing::Last, mode: SortMode::Max, nested: None });
    let mut fc = TopFieldCollector::new(vec![key, SortKey::Doc { reverse: false }], 20, u64::MAX, false);
    se.search(q, &mut fc).unwrap();
    for h in fc.top_docs().hits {
        out.push(format!("{:?}", se.stored(h.doc).unwrap()));
    }
    out
}

fn open_reference(segs: &[(Vec<u8>, Option<Vec<u8>>)], checkpoint: i64) -> Shard {
    let handles = segs
        .iter()
        .map(|(d, l)| {
            let h = SegmentHandle::new(Slice::from(d.clone()));
            match l {
                Some(b) => h.with_live_bytes(b).unwrap(),
                None => h,
            }
        })
        .collect();
    let cfg = ShardConfig { merge_threads: 0, local_checkpoint: Some(checkpoint), ..Default::default() };
    Shard::open(cfg, schema_f(), handles).unwrap()
}

fn open_fetching(dir: &Path, segs: &[(Vec<u8>, Option<Vec<u8>>)], checkpoint: i64) -> (Shard, Vec<Arc<TestGate>>) {
    let mut gates = Vec::new();
    let handles = segs
        .iter()
        .enumerate()
        .map(|(i, (d, l))| {
            let path = dir.join(format!("partial-{i}.seg"));
            let g = TestGate::new(d.clone(), &path);
            let data = Slice::map_file(&path).unwrap();
            gates.push(g.clone());
            let h = SegmentHandle::fetching(data, g);
            match l {
                Some(b) => h.with_live_bytes(b).unwrap(),
                None => h,
            }
        })
        .collect();
    let cfg = ShardConfig { merge_threads: 0, local_checkpoint: Some(checkpoint), dir: Some(dir.join("dst")), ..Default::default() };
    std::fs::create_dir_all(dir.join("dst")).unwrap();
    (Shard::open(cfg, schema_f(), handles).unwrap(), gates)
}

#[test]
fn queries_on_downloading_segments_equal_complete_ones() {
    for seed in [1u64, 2] {
        let dir = tmp(&format!("q{seed}"));
        let (segs, _live, cp) = flushed(&dir, seed);
        let reference = open_reference(&segs, cp);
        let (s, gates) = open_fetching(&dir, &segs, cp);
        // Opening read only what readers parse: no postings beyond `_id`, no documents.
        for g in &gates {
            assert!(!g.is_complete());
            for f in [BODY, TITLE, TAG, FEATURE] {
                assert!(!g.section_ready(SectionKind::Docs, f), "docs of field {f} fetched at open");
            }
            assert!(!g.section_ready(SectionKind::Positions, BODY));
            assert!(!g.section_ready(SectionKind::Stored, search_codec::container::GLOBAL));
            assert!(g.section_ready(SectionKind::TermsIndex, BODY) && g.section_ready(SectionKind::DocValues, NUM));
            // Eager ranges were asked for first, then postings, then documents.
            let p = g.prioritized.lock();
            assert!(!p.is_empty());
        }
        // A count-only query on doc values leaves postings alone.
        let ranged: QueryRef = Arc::new(PointRangeQuery::long(NUM, 0, 10));
        assert_eq!(s.searcher().count(&ranged).unwrap(), reference.searcher().count(&ranged).unwrap());
        assert!(gates.iter().all(|g| !g.section_ready(SectionKind::Docs, BODY)));
        // Title queries fetch title postings only.
        let tq: QueryRef = Arc::new(TermQuery::new(TITLE, b"w1".to_vec()));
        assert_eq!(s.searcher().count(&tq).unwrap(), reference.searcher().count(&tq).unwrap());
        assert!(gates.iter().any(|g| g.section_ready(SectionKind::Docs, TITLE)));
        assert!(gates.iter().all(|g| !g.section_ready(SectionKind::Docs, BODY)));
        // Term queries and fetches never wait for positions; phrases do.
        let mut c = TopScoreCollector::new(5, u64::MAX);
        s.searcher().search(&tq, &mut c).unwrap();
        for h in c.top_docs().hits {
            s.searcher().stored(h.doc).unwrap();
        }
        assert!(gates.iter().all(|g| !g.section_ready(SectionKind::Positions, TITLE)));
        let pq: QueryRef = Arc::new(PhraseQuery::new(TITLE, vec![b"w1".to_vec(), b"w2".to_vec()], 0));
        assert_eq!(s.searcher().count(&pq).unwrap(), reference.searcher().count(&pq).unwrap());
        assert!(gates.iter().any(|g| g.section_ready(SectionKind::Positions, TITLE)));
        assert!(gates.iter().all(|g| !g.section_ready(SectionKind::Positions, BODY)));
        // Each query on a fresh download, so no other query has fetched its sections first.
        for (i, (name, q)) in queries().into_iter().enumerate() {
            let sub = dir.join(format!("q{i}"));
            std::fs::create_dir_all(&sub).unwrap();
            let (fresh, _) = open_fetching(&sub, &segs, cp);
            assert_eq!(observe(&fresh, &q), observe(&reference, &q), "seed {seed}: {name}");
        }
        // Realtime and searcher gets read documents through the gate.
        for k in 0..1100 {
            let id = format!("d{k}");
            let a = s.get(id.as_bytes(), true).unwrap().map(|d| (d.seq_no, d.version, d.fields));
            let b = reference.get(id.as_bytes(), true).unwrap().map(|d| (d.seq_no, d.version, d.fields));
            assert_eq!(a, b, "{id}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn writes_do_not_wait_for_the_download() {
    let dir = tmp("w");
    let (segs, live, cp) = flushed(&dir, 3);
    let reference = open_reference(&segs, cp);
    let (s, gates) = open_fetching(&dir, &segs, cp);
    let waits: usize = gates.iter().map(|g| g.waits.load(Ordering::Relaxed)).sum();
    let mut r = rng(9);
    // Updates and deletes resolve versions from `_id` terms and doc values only.
    for k in 0..300 {
        let id = format!("d{k}");
        if k % 3 == 0 {
            for sh in [&s, &reference] {
                sh.delete(DeleteOp::new(id.clone().into_bytes())).unwrap();
            }
        } else {
            let mut d = random_doc(&mut r, k);
            d.id = id.clone();
            let a = s.index(IndexOp::new(to_indexable(&d))).unwrap();
            let b = reference.index(IndexOp::new(to_indexable(&d))).unwrap();
            assert_eq!(a, b, "{id} (live before: {})", live.contains_key(&id));
        }
        assert_eq!(s.version_of(id.as_bytes()), reference.version_of(id.as_bytes()));
    }
    assert_eq!(gates.iter().map(|g| g.waits.load(Ordering::Relaxed)).sum::<usize>(), waits, "a write waited on the download");
    // Background merges leave downloading segments alone; refresh and flush don't read them.
    s.refresh().unwrap();
    reference.refresh().unwrap();
    s.maybe_merge().unwrap();
    let out = s.flush().unwrap();
    assert!(gates.iter().all(|g| !g.is_complete()));
    let old: Vec<[u8; 16]> = segs.iter().map(|(d, _)| d[8..24].try_into().unwrap()).collect();
    for f in &out.segments {
        if old.contains(&f.id) {
            assert!(!f.new && f.path.is_none(), "a downloading segment was persisted or reported new");
        }
    }
    for (name, q) in queries() {
        assert_eq!(observe(&s, &q), observe(&reference, &q), "{name}");
    }
    // Force merge waits for every byte, then merges.
    s.force_merge(Some(1), false).unwrap();
    reference.force_merge(Some(1), false).unwrap();
    assert!(gates.iter().all(|g| g.is_complete()));
    assert_eq!(s.segments().len(), 1);
    let all: QueryRef = Arc::new(MatchAllQuery);
    assert_eq!(s.searcher().count(&all).unwrap(), reference.searcher().count(&all).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn download_failures_fail_the_request() {
    let dir = tmp("f");
    let (segs, _live, cp) = flushed(&dir, 4);
    // At open.
    {
        let path = dir.join("bad.seg");
        let g = TestGate::new(segs[0].0.clone(), &path);
        g.fail.store(true, Ordering::Relaxed);
        let h = SegmentHandle::fetching(Slice::map_file(&path).unwrap(), g);
        let cfg = ShardConfig { merge_threads: 0, ..Default::default() };
        let e = Shard::open(cfg, schema_f(), vec![h]).err().expect("open must fail");
        assert!(e.to_string().contains("download failed"), "{e}");
    }
    // On first use of postings and documents: an error, never partial results.
    let (s, gates) = open_fetching(&dir, &segs, cp);
    for g in &gates {
        g.fail.store(true, Ordering::Relaxed);
    }
    let se = s.searcher();
    let q: QueryRef = Arc::new(TermQuery::new(BODY, b"w0".to_vec()));
    let mut c = TopScoreCollector::new(10, u64::MAX);
    assert!(se.search(&q, &mut c).unwrap_err().to_string().contains("download failed"));
    assert!(se.count(&q).is_err());
    assert!(se.stored(0).is_err());
    assert!(s.get(b"d1", true).is_err());
    // Doc values and points opened eagerly keep working.
    let r: QueryRef = Arc::new(PointRangeQuery::long(NUM, 0, 10));
    assert!(se.count(&r).is_ok());
    for g in &gates {
        g.fail.store(false, Ordering::Relaxed);
    }
    assert!(se.count(&q).unwrap() > 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_first_uses_of_a_download() {
    let dir = tmp("c");
    let (segs, _live, cp) = flushed(&dir, 5);
    let reference = open_reference(&segs, cp);
    let want: Vec<Vec<String>> = queries().iter().map(|(_, q)| observe(&reference, q)).collect();
    for round in 0..3 {
        let sub = dir.join(format!("r{round}"));
        std::fs::create_dir_all(&sub).unwrap();
        let (s, gates) = open_fetching(&sub, &segs, cp);
        let qs = queries();
        std::thread::scope(|sc| {
            // Searches race to first use of the same fields while a writer updates docs.
            for t in 0..4 {
                let (s, qs, want) = (&s, &qs, &want);
                sc.spawn(move || {
                    for (i, (name, q)) in qs.iter().enumerate().skip(t).step_by(2) {
                        assert_eq!(&observe(s, q), &want[i], "round {round}: {name}");
                    }
                });
            }
            let s = &s;
            sc.spawn(move || {
                let mut r = rng(round);
                for k in 1100..1300 {
                    s.index(IndexOp::new(to_indexable(&random_doc(&mut r, k)))).unwrap();
                }
            });
        });
        assert!(gates.iter().any(|g| g.section_ready(SectionKind::Docs, BODY)));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn complete_downloads_open_without_waits_and_stay_the_stores() {
    let dir = tmp("done");
    let (segs, _live, cp) = flushed(&dir, 6);
    let path = dir.join("done.seg");
    let last = segs.last().unwrap();
    let g = TestGate::new(last.0.clone(), &path);
    g.wait_ready(std::slice::from_ref(&(0..last.0.len() as u64))).unwrap();
    let waits = g.waits.load(Ordering::Relaxed);
    let h = SegmentHandle::fetching(Slice::map_file(&path).unwrap(), g.clone());
    let h = match &last.1 {
        Some(b) => h.with_live_bytes(b).unwrap(),
        None => h,
    };
    let cfg = ShardConfig { merge_threads: 0, local_checkpoint: Some(cp), dir: Some(dir.join("dst")), ..Default::default() };
    std::fs::create_dir_all(dir.join("dst")).unwrap();
    let s = Shard::open(cfg, schema_f(), vec![h]).unwrap();
    let q: QueryRef = Arc::new(TermQuery::new(BODY, b"w0".to_vec()));
    let reference = open_reference(std::slice::from_ref(last), cp);
    assert!(s.searcher().count(&q).unwrap() > 0);
    assert_eq!(s.searcher().count(&q).unwrap(), reference.searcher().count(&q).unwrap());
    // Only `with_live_bytes`' header check waited; the open saw a complete file.
    assert!(g.waits.load(Ordering::Relaxed) <= waits + 1);
    let out = s.flush().unwrap();
    assert!(out.segments.iter().all(|f| !f.new && f.path.is_none()));
    assert_eq!(std::fs::read_dir(dir.join("dst")).unwrap().count(), 0, "the store's segment was copied");
    let _ = std::fs::remove_dir_all(&dir);
}
