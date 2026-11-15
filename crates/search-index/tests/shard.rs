//! Shard behaviour: versioning and conflicts, realtime get, replay, flush and reopen, nested
//! blocks, index sorting, merges, memory-driven buffer conversion and concurrency.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::sync::atomic::{AtomicBool, Ordering};

use common::*;
use rand::Rng;
use search_codec::{DocValuesType, IndexOptions, IndexSort, SortField, SortKind, SortMissing, SortSelector};
use search_index::doc::{DocValue, IndexableDoc, IndexableField, PreToken};
use search_index::query::nested::{JoinScoreMode, ToParentQuery, root_filter};
use search_index::query::{MatchAllQuery, TermQuery};
use search_index::schema::FieldSchema;
use search_index::shard::{ConflictKind, OpType, Origin, SegmentHandle, VersionType};
use search_index::{DeleteOp, IndexOp, OpResult, QueryRef, Schema, Shard, ShardConfig};

fn doc(id: &str, words: &[&str]) -> IndexableDoc {
    let mut d = IndexableDoc::new(id.as_bytes().to_vec());
    let mut off = 0;
    let toks = words
        .iter()
        .map(|w| {
            let t = PreToken::new(w.as_bytes().to_vec(), 1, off, off + w.len() as u32);
            off += w.len() as u32 + 1;
            t
        })
        .collect();
    d.fields.push(IndexableField::Tokens { field: BODY, tokens: toks });
    d
}

fn count(s: &Shard, q: QueryRef) -> u64 {
    s.searcher().count(&q).unwrap()
}

#[test]
fn versioning_and_conflicts() {
    let s = Shard::in_memory(schema());
    let r = s.index(IndexOp::new(doc("1", &["a"]))).unwrap();
    assert_eq!(r, OpResult::Indexed { seq_no: 0, primary_term: 1, version: 1, created: true });
    // op_type=create on an existing doc.
    let mut op = IndexOp::new(doc("1", &["b"]));
    op.op_type = OpType::Create;
    match s.index(op).unwrap() {
        OpResult::Conflict(c) => {
            assert_eq!(c.kind, ConflictKind::Exists { current: 1 });
            assert_eq!(c.reason("1"), "[1]: version conflict, document already exists (current version [1])");
        }
        r => panic!("{r:?}"),
    }
    // if_seq_no / if_primary_term.
    let mut op = IndexOp::new(doc("1", &["b"]));
    op.if_seq_no = Some(5);
    op.if_primary_term = Some(1);
    match s.index(op).unwrap() {
        OpResult::Conflict(c) => assert_eq!(
            c.reason("1"),
            "[1]: version conflict, required seqNo [5], primary term [1]. current document has seqNo [0] and primary term [1]"
        ),
        r => panic!("{r:?}"),
    }
    let mut op = IndexOp::new(doc("1", &["b"]));
    op.if_seq_no = Some(0);
    op.if_primary_term = Some(1);
    assert_eq!(s.index(op).unwrap(), OpResult::Indexed { seq_no: 1, primary_term: 1, version: 2, created: false });
    // External versions.
    let mut op = IndexOp::new(doc("1", &["c"]));
    op.version = Some(2);
    op.version_type = VersionType::External;
    match s.index(op).unwrap() {
        OpResult::Conflict(c) => {
            assert_eq!(c.reason("1"), "[1]: version conflict, current version [2] is higher or equal to the one provided [2]")
        }
        r => panic!("{r:?}"),
    }
    let mut op = IndexOp::new(doc("1", &["c"]));
    op.version = Some(10);
    op.version_type = VersionType::External;
    assert!(matches!(s.index(op).unwrap(), OpResult::Indexed { version: 10, .. }));
    s.refresh().unwrap();
    // Deletes and tombstones.
    assert!(matches!(s.delete(DeleteOp::new(b"1".to_vec())).unwrap(), OpResult::Deleted { version: 11, found: true, .. }));
    assert!(s.get(b"1", true).unwrap().is_none());
    assert!(matches!(s.delete(DeleteOp::new(b"1".to_vec())).unwrap(), OpResult::Deleted { version: 12, found: false, .. }));
    assert!(matches!(s.index(IndexOp::new(doc("1", &["d"]))).unwrap(), OpResult::Indexed { version: 13, created: true, .. }));
    assert!(matches!(s.delete(DeleteOp::new(b"nope".to_vec())).unwrap(), OpResult::Deleted { version: 1, found: false, .. }));
    let mut d = DeleteOp::new(b"nope2".to_vec());
    d.if_seq_no = Some(1);
    d.if_primary_term = Some(1);
    match s.delete(d).unwrap() {
        OpResult::Conflict(c) => assert_eq!(
            c.reason("nope2"),
            "[nope2]: version conflict, required seqNo [1], primary term [1]. but no document was found"
        ),
        r => panic!("{r:?}"),
    }
    s.refresh().unwrap();
    assert_eq!(count(&s, Arc::new(MatchAllQuery)), 1);
    assert_eq!(count(&s, Arc::new(TermQuery::new(BODY, "d"))), 1);
    // Every seq no is accounted for.
    assert_eq!(s.local_checkpoint(), s.max_seq_no());
}

#[test]
fn replay_tolerates_order_and_duplicates() {
    let s = Shard::in_memory(schema());
    let ops: Vec<(i64, Option<&str>)> = vec![(3, Some("c")), (1, Some("a")), (4, None), (2, Some("b")), (3, Some("c"))];
    for (seq, w) in ops {
        let origin = Origin::Replay { seq_no: seq, primary_term: 1, version: seq };
        let r = match w {
            Some(w) => {
                let mut op = IndexOp::new(doc("x", &[w]));
                op.origin = origin;
                s.index(op).unwrap()
            }
            None => {
                let mut op = DeleteOp::new(b"x".to_vec());
                op.origin = origin;
                s.delete(op).unwrap()
            }
        };
        let _ = r;
    }
    s.mark_seq_no_processed(0);
    assert!(s.get(b"x", true).unwrap().is_none(), "the delete at seq 4 wins");
    assert_eq!(s.local_checkpoint(), 4);
}

#[test]
fn flush_and_reopen_from_files() {
    let dir = std::env::temp_dir().join(format!("roll-search-index-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cfg = ShardConfig { dir: Some(dir.clone()), merge_threads: 0, ..Default::default() };
    let s = Shard::open(cfg.clone(), schema(), Vec::new()).unwrap();
    for i in 0..300 {
        s.index(IndexOp::new(doc(&format!("{i}"), &["w", if i % 2 == 0 { "even" } else { "odd" }]))).unwrap();
        if i % 100 == 99 {
            s.refresh().unwrap();
        }
    }
    for i in 0..50 {
        s.delete(DeleteOp::new(format!("{i}").into_bytes())).unwrap();
    }
    s.index(IndexOp::new(doc("7", &["w", "seven"]))).unwrap();
    let out = s.flush().unwrap();
    assert_eq!(out.local_checkpoint, out.max_seq_no);
    assert!(out.segments.iter().all(|f| f.path.as_ref().is_some_and(|p| p.exists())));
    let handles: Vec<SegmentHandle> = out
        .segments
        .iter()
        .map(|f| {
            let h = SegmentHandle::from_file(f.path.clone().unwrap()).unwrap();
            match &f.live_docs {
                Some(b) => h.with_live_bytes(b).unwrap(),
                None => h,
            }
        })
        .collect();
    let cfg2 = ShardConfig { local_checkpoint: Some(out.local_checkpoint), ..cfg };
    let s2 = Shard::open(cfg2, schema(), handles).unwrap();
    assert_eq!(count(&s2, Arc::new(MatchAllQuery)), 251);
    assert_eq!(count(&s2, Arc::new(TermQuery::new(BODY, "even"))), 125);
    assert_eq!(count(&s2, Arc::new(TermQuery::new(BODY, "seven"))), 1);
    let g = s2.get(b"7", true).unwrap().unwrap();
    assert_eq!(g.version, 3, "indexed, deleted, indexed again");
    // Writes continue after the recovered checkpoint.
    let r = s2.index(IndexOp::new(doc("new", &["x"]))).unwrap();
    assert!(matches!(r, OpResult::Indexed { seq_no, .. } if seq_no == out.max_seq_no + 1));
    s2.force_merge(Some(1), false).unwrap();
    s2.refresh().unwrap();
    assert_eq!(s2.segments().len(), 1);
    assert_eq!(count(&s2, Arc::new(MatchAllQuery)), 252);
    drop(s);
    drop(s2);
    let _ = std::fs::remove_dir_all(&dir);
}

fn nested_schema(sort: bool) -> Schema {
    let mut s = Schema::new()
        .with_field(FieldSchema::new(BODY, "body").indexed(IndexOptions::DocsAndFreqsAndPositions, true))
        .unwrap()
        .with_field(FieldSchema::new(30, "n").with_doc_values(DocValuesType::Numeric))
        .unwrap()
        .with_field(FieldSchema::new(31, "_nested_path").indexed(IndexOptions::Docs, false))
        .unwrap();
    if sort {
        s = s.with_index_sort(Some(IndexSort {
            fields: vec![SortField {
                field: 30,
                kind: SortKind::Long,
                reverse: false,
                missing: SortMissing::Last,
                selector: SortSelector::Min,
            }],
        }));
    }
    s
}

fn nested_doc(id: &str, n: i64, children: &[&[&str]]) -> IndexableDoc {
    let mut d = doc(id, &["parent"]);
    d.fields.push(IndexableField::DocValue { field: 30, value: DocValue::Numeric(n) });
    for c in children {
        let mut cd = doc("", c);
        cd.fields.push(IndexableField::Term { field: 31, term: bytes::Bytes::from_static(b"kids") });
        d.nested.push(cd);
    }
    d
}

#[test]
fn nested_blocks_join_and_survive_updates_sorting_and_merges() {
    for sort in [false, true] {
        let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, nested_schema(sort), Vec::new()).unwrap();
        let mut rng = rng(5);
        let mut model: HashMap<String, (i64, Vec<Vec<String>>)> = HashMap::new();
        for round in 0..400 {
            let id = format!("p{}", rng.gen_range(0..60));
            if rng.gen_bool(0.15) {
                s.delete(DeleteOp::new(id.clone().into_bytes())).unwrap();
                model.remove(&id);
            } else {
                let n = rng.gen_range(0..100);
                let kids: Vec<Vec<String>> = (0..rng.gen_range(0..4))
                    .map(|_| (0..rng.gen_range(1..3)).map(|_| format!("k{}", rng.gen_range(0..5))).collect())
                    .collect();
                let refs: Vec<Vec<&str>> = kids.iter().map(|k| k.iter().map(|x| x.as_str()).collect()).collect();
                let slices: Vec<&[&str]> = refs.iter().map(|v| v.as_slice()).collect();
                s.index(IndexOp::new(nested_doc(&id, n, &slices))).unwrap();
                model.insert(id, (n, kids));
            }
            if round % 37 == 0 {
                s.refresh().unwrap();
            }
            if round % 150 == 149 {
                s.force_merge(Some(1), false).unwrap();
            }
        }
        s.refresh().unwrap();
        let searcher = s.searcher();
        let parents = searcher.count(&root_filter()).unwrap();
        assert_eq!(parents as usize, model.len(), "sort={sort}");
        for k in 0..5 {
            let kt = format!("k{k}");
            let child: QueryRef = Arc::new(search_index::query::BooleanQuery::new()
                .must(Arc::new(TermQuery::new(BODY, kt.clone())))
                .filter(Arc::new(TermQuery::new(31, "kids"))));
            let q: QueryRef = Arc::new(ToParentQuery { child, parent_filter: root_filter(), score_mode: JoinScoreMode::Avg });
            let want = model.values().filter(|(_, kids)| kids.iter().any(|c| c.contains(&kt))).count();
            assert_eq!(searcher.count(&q).unwrap() as usize, want, "k{k} sort={sort}");
            let mut c = search_index::collector::TopScoreCollector::new(100, u64::MAX);
            searcher.search(&q, &mut c).unwrap();
            for h in c.top_docs().hits {
                let e = searcher.explain(&q, h.doc).unwrap();
                assert!(e.is_match());
                assert_eq!(e.value(), h.score);
                assert!(e.description.starts_with("Score based on "), "{}", e.description);
            }
        }
        if sort {
            // Index-sorted segments keep parents in key order.
            for seg in searcher.segments() {
                let dv = seg.reader.numeric(30).unwrap();
                let mut last = i64::MIN;
                for d in 0..seg.max_doc() {
                    if let Some(v) = dv.get(d) {
                        assert!(v >= last);
                        last = v;
                    }
                }
            }
        }
    }
}

#[test]
fn memory_pressure_converts_buffers_without_publishing() {
    let cfg = ShardConfig { max_buffer_bytes: 256 * 1024, merge_threads: 0, ..Default::default() };
    let s = Shard::open(cfg, schema(), Vec::new()).unwrap();
    let mut rng = rng(1);
    for i in 0..3000 {
        let d = random_doc(&mut rng, i);
        s.index(IndexOp::new(to_indexable(&d))).unwrap();
    }
    assert!(s.buffer_bytes() <= 2 * 256 * 1024 + (1 << 20), "{}", s.buffer_bytes());
    assert!(!s.segments().is_empty(), "buffers were converted");
    assert_eq!(s.searcher().num_docs(), 0, "nothing published before refresh");
    s.refresh().unwrap();
    assert_eq!(s.searcher().num_docs(), 3000);
    let st = s.stats();
    assert_eq!(st.num_docs, 3000);
}

/// With background threads, the write that crosses the buffer budget hands the conversion to a
/// thread of its own; memory stays bounded (writers far over budget wait) and nothing is
/// published before a refresh.
#[test]
fn memory_pressure_converts_buffers_in_the_background() {
    let cfg = ShardConfig { max_buffer_bytes: 256 * 1024, merge_threads: 1, ..Default::default() };
    let s = Shard::open(cfg, schema(), Vec::new()).unwrap();
    let mut rng = rng(1);
    for i in 0..3000 {
        let d = random_doc(&mut rng, i);
        s.index(IndexOp::new(to_indexable(&d))).unwrap();
        assert!(s.buffer_bytes() <= 2 * 256 * 1024 + (1 << 20), "{}", s.buffer_bytes());
    }
    let start = std::time::Instant::now();
    while s.segments().is_empty() {
        assert!(start.elapsed() < Duration::from_secs(30), "no background conversion");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(s.searcher().num_docs(), 0, "nothing published before refresh");
    s.refresh().unwrap();
    assert_eq!(s.searcher().num_docs(), 3000);
}

#[test]
fn stats_count_refreshes_flushes_and_merges() {
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(), Vec::new()).unwrap();
    let mut rng = rng(2);
    for i in 0..600 {
        s.index(IndexOp::new(to_indexable(&random_doc(&mut rng, i)))).unwrap();
        if i % 200 == 199 {
            s.refresh().unwrap();
        }
    }
    // A refresh with nothing new still counts as a call (Elasticsearch's `refresh.total`).
    assert!(!s.refresh().unwrap());
    s.flush().unwrap();
    let before = s.stats();
    assert_eq!((before.refreshes_total, before.refresh_calls, before.flushes_total), (3, 4, 1));
    assert!(before.refresh_time > Duration::ZERO && before.flush_time > Duration::ZERO);
    assert_eq!((before.merges_running, before.merges_current_docs, before.merges_current_bytes), (0, 0, 0));
    let (docs, bytes): (u64, u64) = s.segments().iter().map(|g| (g.max_doc as u64, g.size_bytes)).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    let merges = before.merges_total;
    s.force_merge(Some(1), false).unwrap();
    let after = s.stats();
    assert_eq!(after.segments, 1);
    assert_eq!(after.merges_total, merges + 1);
    assert_eq!(after.merges_total_docs - before.merges_total_docs, docs);
    assert_eq!(after.merges_total_bytes - before.merges_total_bytes, bytes);
    assert!(after.merges_total_time > before.merges_total_time);
    assert_eq!((after.merges_current_docs, after.merges_current_bytes), (0, 0));
}

#[test]
fn concurrent_writes_refreshes_merges_and_searches() {
    let cfg = ShardConfig { merge_threads: 2, max_buffer_bytes: 1 << 20, ..Default::default() };
    let s = Shard::open(cfg, schema(), Vec::new()).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let writers = 4;
    let per = 1500;
    std::thread::scope(|sc| {
        // Searchers check snapshot invariants while everything runs.
        for _ in 0..2 {
            let s = s.clone();
            let stop = stop.clone();
            sc.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let se = s.searcher();
                    let all = se.count(&(Arc::new(MatchAllQuery) as QueryRef)).unwrap();
                    assert_eq!(all, se.num_docs() as u64);
                    let mut c = search_index::collector::TopScoreCollector::new(10, 10);
                    se.search(&(Arc::new(TermQuery::new(BODY, "w1")) as QueryRef), &mut c).unwrap();
                }
            });
        }
        {
            let s = s.clone();
            let stop = stop.clone();
            sc.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    s.refresh().unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(3));
                }
            });
        }
        let handles: Vec<_> = (0..writers)
            .map(|w| {
                let s = s.clone();
                sc.spawn(move || {
                    let mut rng = rng(w as u64 + 77);
                    let mut final_state: HashMap<String, Option<usize>> = HashMap::new();
                    for i in 0..per {
                        // Disjoint id spaces per writer, plus a shared hot id.
                        let k = rng.gen_range(0..200);
                        let id = format!("w{w}-{k}");
                        if rng.gen_bool(0.2) {
                            s.delete(DeleteOp::new(id.clone().into_bytes())).unwrap();
                            final_state.insert(id, None);
                        } else {
                            let mut d = doc(&id, &["w1", &format!("v{i}")]);
                            d.stored.push(search_index::doc::StoredField {
                                field: 20,
                                value: search_codec::stored::OwnedStoredValue::Long(i as i64),
                            });
                            s.index(IndexOp::new(d)).unwrap();
                            final_state.insert(id, Some(i));
                        }
                        s.index(IndexOp::new(doc("hot", &["w1"]))).unwrap();
                    }
                    final_state
                })
            })
            .collect();
        let mut finals = Vec::new();
        for h in handles {
            finals.push(h.join().unwrap());
        }
        stop.store(true, Ordering::Relaxed);
        s.refresh().unwrap();
        s.wait_for_merges();
        s.refresh().unwrap();
        let se = s.searcher();
        let mut live = 1; // hot
        for f in &finals {
            for (id, st) in f {
                let g = s.get(id.as_bytes(), true).unwrap();
                match st {
                    None => assert!(g.is_none(), "{id} should be deleted"),
                    Some(i) => {
                        live += 1;
                        let g = g.unwrap_or_else(|| panic!("{id} missing"));
                        let v = g.fields.iter().find(|(f, _)| *f == 20).unwrap();
                        assert_eq!(v.1, search_codec::stored::OwnedStoredValue::Long(*i as i64));
                        // The searcher agrees with realtime get.
                        let ng = search_index::shard::get_from_searcher(&se, id.as_bytes()).unwrap().unwrap();
                        assert_eq!(ng.fields, g.fields);
                    }
                }
            }
        }
        if se.num_docs() as usize != live {
            #[allow(clippy::type_complexity)]
            let mut by: HashMap<Vec<u8>, Vec<(usize, u32, i64, i64)>> = HashMap::new();
            for (si, seg) in se.segments().iter().enumerate() {
                for d in 0..seg.max_doc() {
                    if !seg.is_live(d) {
                        continue;
                    }
                    let f = seg.reader.stored().document(d).unwrap();
                    let id = f.iter().find(|(n, _)| *n == 0).map(|(_, v)| match v {
                        search_codec::stored::OwnedStoredValue::Bytes(b) => b.clone(),
                        _ => vec![],
                    }).unwrap();
                    let seq = seg.reader.numeric(1).unwrap().get(d).unwrap();
                    let ver = seg.reader.numeric(2).unwrap().get(d).unwrap();
                    by.entry(id).or_default().push((si, d, seq, ver));
                }
            }
            let dups: Vec<_> = by.iter().filter(|(_, v)| v.len() > 1).take(5).collect();
            panic!("{} live, expected {live}; {} ids duplicated, e.g. {:?}", se.num_docs(), by.values().filter(|v| v.len() > 1).count(),
                dups.iter().map(|(k, v)| (String::from_utf8_lossy(k).to_string(), v)).collect::<Vec<_>>());
        }
        let hot = s.get(b"hot", true).unwrap().unwrap();
        assert_eq!(hot.version as usize, writers * per);
        assert_eq!(s.local_checkpoint(), s.max_seq_no());
    });
}

#[test]
fn concurrent_matrix_keeps_one_live_doc_per_id() {
    let mut failures = Vec::new();
    for writers in [1usize, 4] {
        for refresher in [false, true] {
            for merges in [0usize, 2] {
                for budget in [1usize << 20, 64 << 20] {
                    let cfg = ShardConfig { merge_threads: merges, max_buffer_bytes: budget, ..Default::default() };
                    let s = Shard::open(cfg, schema(), Vec::new()).unwrap();
                    let stop = Arc::new(AtomicBool::new(false));
                    let finals: Vec<HashMap<String, Option<usize>>> = std::thread::scope(|sc| {
                        if refresher {
                            let s = s.clone();
                            let stop = stop.clone();
                            sc.spawn(move || {
                                while !stop.load(Ordering::Relaxed) {
                                    s.refresh().unwrap();
                                    std::thread::sleep(std::time::Duration::from_millis(2));
                                }
                            });
                        }
                        let hs: Vec<_> = (0..writers)
                            .map(|w| {
                                let s = s.clone();
                                sc.spawn(move || {
                                    let mut rng = rng(w as u64 + 77);
                                    let mut fs: HashMap<String, Option<usize>> = HashMap::new();
                                    for i in 0..1500 {
                                        let id = format!("w{w}-{}", rng.gen_range(0..200));
                                        if rng.gen_bool(0.2) {
                                            s.delete(DeleteOp::new(id.clone().into_bytes())).unwrap();
                                            fs.insert(id, None);
                                        } else {
                                            s.index(IndexOp::new(doc(&id, &["w1"]))).unwrap();
                                            fs.insert(id, Some(i));
                                        }
                                        if !refresher && i % 100 == 0 {
                                            s.refresh().unwrap();
                                        }
                                    }
                                    fs
                                })
                            })
                            .collect();
                        let r = hs.into_iter().map(|h| h.join().unwrap()).collect();
                        stop.store(true, Ordering::Relaxed);
                        r
                    });
                    s.refresh().unwrap();
                    s.wait_for_merges();
                    s.refresh().unwrap();
                    let live: usize = finals.iter().map(|f| f.values().filter(|v| v.is_some()).count()).sum();
                    let got = s.searcher().num_docs() as usize;
                    if got != live {
                        failures.push(format!("writers {writers} refresher {refresher} merges {merges} budget {budget}: {got} vs {live}"));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Reopens a commit with a published view.
fn reopen_with(out: &search_index::shard::FlushOutcome, view: Option<search_index::PublishedView>) -> Shard {
    let handles: Vec<SegmentHandle> = out
        .segments
        .iter()
        .map(|f| {
            let h = SegmentHandle::new(f.data.clone());
            match &f.live_docs {
                Some(b) => h.with_live_bytes(b).unwrap(),
                None => h,
            }
        })
        .collect();
    let cfg = ShardConfig { merge_threads: 0, local_checkpoint: Some(out.local_checkpoint), ..Default::default() };
    Shard::open_published(cfg, schema(), handles, view).unwrap()
}

/// Reopens a commit from its zero-copy bytes and live docs.
fn reopen(out: &search_index::shard::FlushOutcome) -> Shard {
    let handles: Vec<SegmentHandle> = out
        .segments
        .iter()
        .map(|f| {
            let h = SegmentHandle::new(f.data.clone());
            match &f.live_docs {
                Some(b) => h.with_live_bytes(b).unwrap(),
                None => h,
            }
        })
        .collect();
    let cfg = ShardConfig { merge_threads: 0, local_checkpoint: Some(out.local_checkpoint), ..Default::default() };
    Shard::open(cfg, schema(), handles).unwrap()
}

#[test]
fn flush_commits_hold_whole_updates_under_concurrent_writes() {
    // An update deletes the old version and buffers the new one under one shared lock; a
    // commit must hold both halves or neither, whatever runs between the conversion and the
    // live-docs snapshot.
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(), Vec::new()).unwrap();
    for i in 0..50 {
        s.index(IndexOp::new(doc(&format!("d{i}"), &["v"]))).unwrap();
    }
    s.refresh().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (s, stop) = (s.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                s.index(IndexOp::new(doc(&format!("d{}", n % 50), &["v"]))).unwrap();
                n += 1;
                if n.is_multiple_of(500) {
                    s.refresh().unwrap();
                }
            }
            n
        })
    };
    let mut last_gen = HashMap::new();
    for round in 0..150 {
        let out = s.flush().unwrap();
        let r = reopen(&out);
        assert_eq!(count(&r, Arc::new(MatchAllQuery)), 50, "commit {round} lost or duplicated documents");
        assert!(out.local_checkpoint <= out.max_seq_no);
        // Live docs that changed since the previous commit are reported as changed.
        for f in &out.segments {
            if let Some(prev) = last_gen.insert(f.id, f.live_docs.clone()) {
                assert!(f.live_changed || prev == f.live_docs, "unchanged live docs reported as changed or vice versa");
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    assert!(writer.join().unwrap() > 0);
}

#[test]
fn a_rejected_write_reports_the_seq_no_it_used() {
    let s = Shard::in_memory(schema());
    let ok = |s: &Shard, id: &str| match s.index(IndexOp::new(doc(id, &["w"]))).unwrap() {
        OpResult::Indexed { seq_no, .. } => seq_no,
        r => panic!("{r:?}"),
    };
    assert_eq!(ok(&s, "a"), 0);
    // A Lucene-level failure (an immense analyzed token) uses seq no 1, like Elasticsearch.
    let huge = IndexableDoc::new(b"b".to_vec())
        .field(IndexableField::Tokens { field: BODY, tokens: vec![PreToken::new(vec![b'y'; 40_000], 1, 0, 1)] });
    let e = s.index(IndexOp::new(huge)).unwrap_err();
    assert_eq!(e.seq_no(), Some((1, 1)));
    assert_eq!((e.kind(), e.status()), ("illegal_argument_exception", 400));
    assert!(e.to_string().starts_with("Document contains at least one immense term in field=\"body\""), "{e}");
    // An immense keyword (`Term`) fails while parsing in Elasticsearch: no seq no, and its
    // reason is the mapper's (verified on Elasticsearch 9.5.5, as `caused_by`).
    let kw = IndexableDoc::new(b"k".to_vec()).term(TAG, vec![b'y'; 40_000]);
    let e = s.index(IndexOp::new(kw.clone())).unwrap_err();
    assert_eq!(e.seq_no(), None);
    let prefix = format!("[{}]", vec!["121"; 30].join(", "));
    assert_eq!(
        e.to_string(),
        format!(
            "Document contains at least one immense term in field=\"tag\" (whose UTF8 encoding is longer than the max length 32766), all of which were skipped. Please correct the analyzer to not produce such terms. The prefix of the first immense term is: '{prefix}...'"
        )
    );
    // In a nested child too.
    let nested = IndexableDoc::new(b"n".to_vec()).child(kw);
    assert_eq!(s.index(IndexOp::new(nested)).unwrap_err().seq_no(), None);
    assert_eq!(ok(&s, "c"), 2);
    assert_eq!((s.max_seq_no(), s.local_checkpoint()), (2, 2));
    // Recovery replays the WAL the caller wrote: ops 0 and 2, and the no-op for 1.
    let r = Shard::in_memory(schema());
    for (seq, id) in [(2, "c"), (0, "a")] {
        let mut op = IndexOp::new(doc(id, &["w"]));
        op.origin = Origin::Replay { seq_no: seq, primary_term: 1, version: 1 };
        r.index(op).unwrap();
    }
    r.mark_seq_no_processed(1);
    assert_eq!(r.local_checkpoint(), 2);
    assert_eq!(r.flush().unwrap().local_checkpoint, 2);
}

#[test]
fn force_merge_returns_under_steady_indexing() {
    let cfg = ShardConfig { merge_threads: 1, ..Default::default() };
    let s = Shard::open(cfg, schema(), Vec::new()).unwrap();
    for i in 0..8000 {
        s.index(IndexOp::new(doc(&format!("o{i}"), &["old", "w"]))).unwrap();
        if i % 2000 == 1999 {
            s.refresh().unwrap();
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (s, stop) = (s.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                for _ in 0..50 {
                    s.index(IndexOp::new(doc(&format!("n{n}"), &["new"]))).unwrap();
                    n += 1;
                }
                s.refresh().unwrap();
                std::thread::sleep(Duration::from_millis(2));
            }
            n
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    let (tx, rx) = std::sync::mpsc::channel();
    let before = s.stats().merges_total_docs;
    let fm = {
        let s = s.clone();
        std::thread::spawn(move || tx.send(s.force_merge(Some(1), false)).unwrap())
    };
    // Re-merging the whole shard each round (what merging every current segment does) never
    // ends while refreshes keep coming; each round would merge everything again.
    let r = rx.recv_timeout(Duration::from_secs(30));
    let merged = s.stats().merges_total_docs - before;
    stop.store(true, Ordering::Relaxed);
    let new_docs = writer.join().unwrap();
    r.expect("force merge did not return while indexing went on").unwrap();
    fm.join().unwrap();
    // The originals are merged once; refreshed segments only by natural (logarithmic) merges.
    assert!(merged < 2 * 8000 + 8 * new_docs, "{merged} docs merged for 8000 + {new_docs} new");
    // Every doc present at the call ended up in one segment.
    s.refresh().unwrap();
    let se = s.searcher();
    let old = Arc::new(TermQuery::new(BODY, "old")) as QueryRef;
    let per_segment: Vec<u32> = se
        .segments()
        .iter()
        .map(|seg| {
            let w = old.weight(&se, search_index::search::ScoreMode::CompleteNoScores, 1.0).unwrap();
            w.count(seg).unwrap().unwrap_or(0)
        })
        .collect();
    assert_eq!(per_segment.iter().filter(|&&c| c > 0).count(), 1, "{per_segment:?}");
    assert_eq!(per_segment.iter().sum::<u32>(), 8000);
}

#[test]
fn tombstones_are_accounted_and_pruned_by_deletes() {
    // Long enough for the last tombstone to outlive the checks below on a loaded machine.
    let cfg = ShardConfig { merge_threads: 0, gc_deletes: Duration::from_millis(500), ..Default::default() };
    let s = Shard::open(cfg, schema(), Vec::new()).unwrap();
    for i in 0..1000 {
        s.index(IndexOp::new(doc(&format!("{i}"), &["w"]))).unwrap();
    }
    let st = s.stats();
    assert_eq!(st.version_map_entries, 1000);
    let per_doc = st.version_map_bytes / 1000;
    assert!(per_doc >= 64, "{per_doc}");
    // Buffered docs' entries go with their buffer.
    s.refresh().unwrap();
    assert_eq!((s.stats().version_map_entries, s.stats().version_map_bytes), (0, 0));
    // Deletes of ids that exist or not leave tombstones, counted too.
    for i in 0..5000 {
        s.delete(DeleteOp::new(format!("x{i}").into_bytes())).unwrap();
    }
    let st = s.stats();
    assert_eq!(st.version_map_entries, 5000);
    assert!(st.version_map_bytes >= 5000 * 64, "{}", st.version_map_bytes);
    assert_eq!(st.buffer_bytes, 0, "tombstones are not indexing buffer");
    // No refresh: later deletes prune the expired ones (every max(gc_deletes / 4, 100ms)).
    std::thread::sleep(Duration::from_millis(800));
    s.delete(DeleteOp::new(b"last".to_vec())).unwrap();
    let st = s.stats();
    assert!(st.version_map_entries <= 1, "{}", st.version_map_entries);
    assert!(st.version_map_bytes < 1000, "{}", st.version_map_bytes);
    // A recent tombstone still answers version checks.
    let mut op = IndexOp::new(doc("last", &["w"]));
    op.op_type = OpType::Create;
    let r = s.index(op).unwrap();
    assert!(matches!(r, OpResult::Indexed { created: true, version: 2, .. }), "{r:?}");
}

#[test]
fn ops_on_buffered_docs_do_not_wait_for_the_segment_write() {
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(), Vec::new()).unwrap();
    let mut rng = rng(7);
    for i in 0..50_000 {
        s.index(IndexOp::new(to_indexable(&random_doc(&mut rng, i)))).unwrap();
    }
    let ids: Vec<String> = (0..50_000).step_by(997).map(|i| format!("d{i}")).collect();
    let started = Arc::new(AtomicBool::new(false));
    let refresh = {
        let (s, started) = (s.clone(), started.clone());
        std::thread::spawn(move || {
            started.store(true, Ordering::Release);
            let t = std::time::Instant::now();
            s.refresh().unwrap();
            t.elapsed()
        })
    };
    while !started.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(20));
    let mut worst = Duration::ZERO;
    for id in &ids {
        let t = std::time::Instant::now();
        assert!(s.get(id.as_bytes(), true).unwrap().is_some());
        s.index(IndexOp::new(doc(id, &["updated"]))).unwrap();
        worst = worst.max(t.elapsed());
    }
    let took = refresh.join().unwrap();
    // A frozen buffer is only read while it is written: gets and updates of its docs (and of
    // every id in their version-map stripes) go on meanwhile.
    if took > Duration::from_millis(150) {
        assert!(worst < took / 4, "an op took {worst:?} during a {took:?} refresh");
    }
    s.refresh().unwrap();
    assert_eq!(count(&s, Arc::new(TermQuery::new(BODY, "updated"))), ids.len() as u64);
    assert_eq!(s.searcher().num_docs(), 50_000);
}

#[test]
fn the_shard_doc_limit_counts_buffered_nested_and_deleted_docs() {
    let cfg = ShardConfig { merge_threads: 0, max_docs: 10, ..Default::default() };
    let s = Shard::open(cfg.clone(), schema(), Vec::new()).unwrap();
    for i in 0..9 {
        s.index(IndexOp::new(doc(&format!("{i}"), &["w"]))).unwrap();
    }
    // A two-doc block does not fit; nothing of it is kept and no seq no is used.
    let block = doc("p", &["w"]).child(doc("", &["c"]));
    let e = s.index(IndexOp::new(block)).unwrap_err();
    let want = "Number of documents in the shard cannot exceed [10]; for more information, see [https://www.elastic.co/docs/deploy-manage/production-guidance/optimize-performance/size-shards?version=9.5#troubleshooting-max-docs-limit]";
    assert_eq!((e.kind(), e.status(), e.to_string().as_str(), e.seq_no()), ("illegal_argument_exception", 400, want, None));
    s.delete(DeleteOp::new(b"0".to_vec())).unwrap();
    s.index(IndexOp::new(doc("9", &["w"]))).unwrap();
    assert_eq!(s.pending_docs(), 10);
    // Deleted docs count until merged away; updates add a doc too.
    assert!(s.index(IndexOp::new(doc("1", &["again"]))).is_err());
    assert!(s.delete(DeleteOp::new(b"1".to_vec())).is_err(), "deletes are rejected at the limit, as Elasticsearch's");
    assert_eq!((s.max_seq_no(), s.local_checkpoint()), (10, 10), "no seq no was used by the rejections");
    s.refresh().unwrap();
    s.force_merge(Some(1), false).unwrap();
    assert_eq!(s.pending_docs(), 9);
    s.index(IndexOp::new(doc("10", &["w"]))).unwrap();
    s.refresh().unwrap();
    assert_eq!(s.searcher().num_docs(), 10);
    // A shard opened over more docs than the limit refuses to open.
    let out = s.flush().unwrap();
    let handles: Vec<SegmentHandle> = out.segments.iter().map(|f| SegmentHandle::new(f.data.clone())).collect();
    let small = ShardConfig { max_docs: 9, ..cfg };
    assert!(Shard::open(small, schema(), handles).is_err());
}

/// Tombstones of a commit point come back with what remained of `gc_deletes` (external
/// versions after a reopen behave as before it), expired ones are ignored by primary version
/// checks even before they are pruned, and replayed operations still see them.
#[test]
fn tombstones_round_trip_through_a_commit_and_expire_with_gc_deletes() {
    let s = Shard::in_memory(schema());
    let mut op = IndexOp::new(doc("1", &["a"]));
    op.version = Some(10);
    op.version_type = VersionType::External;
    s.index(op).unwrap();
    let mut del = DeleteOp::new(b"1".to_vec());
    del.version = Some(11);
    del.version_type = VersionType::External;
    assert!(matches!(s.delete(del).unwrap(), OpResult::Deleted { found: true, version: 11, .. }));
    s.delete(DeleteOp::new(b"never".to_vec())).unwrap();
    let out = s.flush().unwrap();
    let tombs = s.tombstones(out.local_checkpoint);
    assert_eq!(tombs.len(), 2);
    assert!(s.tombstones(0).is_empty(), "only tombstones the commit covers");
    let t1 = tombs.iter().find(|t| &t.0[..] == b"1").unwrap();
    assert_eq!((t1.1, t1.2), (1, 11));

    let ext = |v: i64| {
        let mut op = IndexOp::new(doc("1", &["b"]));
        op.version = Some(v);
        op.version_type = VersionType::External;
        op
    };
    // Reopened without them: the delete is forgotten.
    let bare = reopen(&out);
    assert!(matches!(bare.index(ext(5)).unwrap(), OpResult::Indexed { version: 5, .. }));
    // Seeded: the delete still decides.
    let s2 = reopen(&out);
    s2.seed_tombstones(tombs.clone());
    match s2.index(ext(5)).unwrap() {
        OpResult::Conflict(c) => assert_eq!(c.reason("1"), "[1]: version conflict, current version [11] is higher or equal to the one provided [5]"),
        r => panic!("{r:?}"),
    }
    // A replayed older operation is stale against the tombstone.
    let mut old = IndexOp::new(doc("1", &["old"]));
    old.origin = Origin::Replay { seq_no: 0, primary_term: 1, version: 10 };
    assert_eq!(s2.index(old).unwrap(), OpResult::Stale { seq_no: 0 });
    // Past gc_deletes (shortened live) primaries no longer see it, replays still do.
    s2.set_gc_deletes(Duration::from_millis(20));
    std::thread::sleep(Duration::from_millis(40));
    let mut old = IndexOp::new(doc("1", &["old"]));
    old.origin = Origin::Replay { seq_no: 0, primary_term: 1, version: 10 };
    assert_eq!(s2.index(old).unwrap(), OpResult::Stale { seq_no: 0 });
    assert!(matches!(s2.index(ext(5)).unwrap(), OpResult::Indexed { version: 5, created: true, .. }));
    assert!(s2.tombstones(i64::MAX).iter().all(|t| &t.0[..] != b"1"));
    // Expired tombstones are not seeded, and seeding never overrides a newer state.
    let s3 = reopen(&out);
    s3.set_gc_deletes(Duration::from_millis(20));
    s3.seed_tombstones(tombs.iter().map(|t| (t.0.clone(), t.1, t.2, Duration::from_secs(1))));
    assert!(s3.tombstones(i64::MAX).is_empty());
    s3.index(IndexOp::new(doc("1", &["live"]))).unwrap();
    s3.seed_tombstones(tombs.clone());
    assert!(s3.get(b"1", true).unwrap().is_some(), "a buffered doc wins over a seeded tombstone");
}

/// After a replay, seq nos handed out but never logged (a crash between the two) are filled
/// with no-ops, so the local checkpoint reaches the max seq no.
#[test]
fn seq_no_gaps_are_filled() {
    let s = Shard::in_memory(schema());
    for seq in [0, 1, 3, 4, 7] {
        let mut op = IndexOp::new(doc(&format!("{seq}"), &["w"]));
        op.origin = Origin::Replay { seq_no: seq, primary_term: 1, version: 1 };
        s.index(op).unwrap();
    }
    assert_eq!((s.local_checkpoint(), s.max_seq_no()), (1, 7));
    assert_eq!(s.fill_seq_no_gaps(), 3, "seq nos 2, 5 and 6");
    assert_eq!(s.local_checkpoint(), 7);
    assert_eq!(s.fill_seq_no_gaps(), 0);
    let r = s.index(IndexOp::new(doc("next", &["w"]))).unwrap();
    assert!(matches!(r, OpResult::Indexed { seq_no: 8, .. }));
}

/// Elasticsearch's soft-deletes retention: deletes of recent operations don't make small
/// segments merge (a fully deleted segment stays), until the retention lease moves past them.
#[test]
fn retained_deletes_do_not_trigger_merges() {
    let cfg = ShardConfig { merge_threads: 0, ..Default::default() };
    let s = Shard::open(cfg, schema(), Vec::new()).unwrap();
    s.index(IndexOp::new(doc("1", &["a"]))).unwrap();
    s.index(IndexOp::new(doc("2", &["a"]))).unwrap();
    s.refresh().unwrap();
    s.index(IndexOp::new(doc("3", &["a"]))).unwrap();
    s.refresh().unwrap();
    s.delete(DeleteOp::new(b"3".to_vec())).unwrap();
    s.delete(DeleteOp::new(b"1".to_vec())).unwrap();
    s.refresh().unwrap();
    // As Elasticsearch 9.5.5 shows it: `_0 1 1`, `_1 0 1` (it adds a third segment of delete
    // tombstones, which roll keeps in the version map instead).
    let segs: Vec<(u32, u32)> = s.segments().iter().map(|g| (g.max_doc - g.del_count, g.del_count)).collect();
    assert_eq!(segs, vec![(1, 1), (0, 1)]);
    assert_eq!(s.retained_from(), 0, "a new shard retains everything until the lease syncs");
    // Forced merges still expunge.
    s.force_merge(None, true).unwrap();
    s.refresh().unwrap();
    let segs: Vec<(u32, u32)> = s.segments().iter().map(|g| (g.max_doc - g.del_count, g.del_count)).collect();
    assert_eq!(segs, vec![(1, 0)]);
}

/// The soft-deletes retention lease travels with a commit: a shard reopened with it keeps the
/// deletes it retained (no merge reclaims them right after the reopen), one reopened without
/// it starts the lease at the commit's checkpoint.
#[test]
fn a_reopened_shard_keeps_its_retention_lease() {
    let build = || {
        let cfg = ShardConfig { merge_threads: 0, ..Default::default() };
        let s = Shard::open(cfg, schema(), Vec::new()).unwrap();
        s.index(IndexOp::new(doc("1", &["a"]))).unwrap();
        s.index(IndexOp::new(doc("2", &["a"]))).unwrap();
        s.refresh().unwrap();
        s.index(IndexOp::new(doc("3", &["a"]))).unwrap();
        s.refresh().unwrap();
        s.delete(DeleteOp::new(b"3".to_vec())).unwrap();
        s.delete(DeleteOp::new(b"1".to_vec())).unwrap();
        s.refresh().unwrap();
        s
    };
    let segs = |s: &Shard| s.segments().iter().map(|g| (g.max_doc - g.del_count, g.del_count)).collect::<Vec<_>>();
    let s = build();
    let lease = s.retention_lease();
    assert_eq!((lease.0, lease.1), (0, -1), "a new shard retains everything");
    let out = s.flush().unwrap();
    let kept = reopen(&out);
    kept.seed_retention_lease(lease.0, lease.1, lease.2);
    assert_eq!(kept.retained_from(), 0);
    kept.index(IndexOp::new(doc("4", &["a"]))).unwrap();
    kept.refresh().unwrap();
    assert_eq!(segs(&kept), vec![(1, 1), (0, 1), (1, 0)], "the retained deletes stay");
    let fresh = reopen(&out);
    assert_eq!(fresh.retained_from(), out.local_checkpoint + 1);
    // A lease is never moved backwards past what the shard retains.
    fresh.seed_retention_lease(fresh.retained_from() + 5, 0, std::time::Duration::ZERO);
    assert_eq!(fresh.retained_from(), out.local_checkpoint + 1);
    fresh.index(IndexOp::new(doc("4", &["a"]))).unwrap();
    fresh.refresh().unwrap();
    assert!(segs(&fresh).iter().all(|&(_, del)| del == 0), "the deletes are reclaimed: {:?}", segs(&fresh));
}

/// `Shard::set_leaf_sort`: searchers list segments by a point field's max value, descending,
/// segments without the field last (Elasticsearch's `@timestamp` leaf sorter), and only once it
/// is set.
#[test]
fn leaf_sort_orders_segments_by_max_point() {
    const TS: u32 = 40;
    let schema = schema().with_field(FieldSchema::new(TS, "@timestamp").with_points(search_codec::info::PointsSpec::new(1, 8))).unwrap();
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema, Vec::new()).unwrap();
    let ts_doc = |id: &str, ts: Option<i64>| {
        let mut d = doc(id, &["w"]);
        if let Some(t) = ts {
            d.fields.push(IndexableField::Point { field: TS, packed: search_codec::points::encode::i64_bytes(t).to_vec() });
        }
        d
    };
    for (id, ts) in [("old", Some(10)), ("none", None), ("new", Some(30)), ("mid", Some(20))] {
        s.index(IndexOp::new(ts_doc(id, ts))).unwrap();
        s.refresh().unwrap();
    }
    // Each segment by the timestamp it holds.
    let ids = |s: &Shard| -> Vec<Option<i64>> {
        s.searcher().segments().iter().map(|seg| seg.reader.points(TS).map(|p| search_codec::points::encode::i64_from(p.max_packed_value()))).collect()
    };
    let (old, none, new, mid) = (Some(10), None, Some(30), Some(20));
    assert_eq!(ids(&s), [old, none, new, mid]);
    let generation = s.searcher().generation();
    s.set_leaf_sort(Some(TS));
    assert!(s.searcher().generation() > generation);
    assert_eq!(ids(&s), [new, mid, old, none]);
    s.set_leaf_sort(None);
    assert_eq!(ids(&s), [old, none, new, mid]);
}

/// A commit taken with writes nobody refreshed yet says what the searcher showed
/// (`FlushOutcome::published`): reopened with it, searches and non-realtime gets see exactly
/// that (unrefreshed docs hidden, docs deleted since the refresh still found), realtime gets see
/// everything, and the next refresh publishes the rest.
#[test]
fn a_commit_reopens_with_the_searcher_it_had() {
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(), Vec::new()).unwrap();
    for i in 0..10 {
        s.index(IndexOp::new(doc(&format!("{i}"), &["w"]))).unwrap();
    }
    s.refresh().unwrap();
    // Not refreshed: two new docs, one delete and one update of refreshed docs.
    s.index(IndexOp::new(doc("new1", &["w", "fresh"]))).unwrap();
    s.index(IndexOp::new(doc("new2", &["w", "fresh"]))).unwrap();
    s.delete(DeleteOp::new(b"3".to_vec())).unwrap();
    s.index(IndexOp::new(doc("4", &["w", "fresh"]))).unwrap();
    // Memory pressure wrote some of it as a segment without publishing it.
    s.write_buffers().unwrap();
    s.index(IndexOp::new(doc("new3", &["w"]))).unwrap();
    let all = || Arc::new(MatchAllQuery) as QueryRef;
    let fresh = || Arc::new(TermQuery::new(BODY, "fresh")) as QueryRef;
    assert_eq!((count(&s, all()), count(&s, fresh())), (10, 0));
    let out = s.flush().unwrap();
    let view = out.published.clone().expect("the commit holds unrefreshed writes");
    // The segment memory pressure wrote, and the buffer the flush converted.
    assert_eq!(view.hidden, vec![1, 2], "{view:?}");
    assert_eq!(view.deleted.len(), 1, "doc 3 and the old doc 4: {view:?}");
    assert_eq!(view.deleted[0].1.len(), 2, "{view:?}");
    // Flushing published nothing.
    assert_eq!((count(&s, all()), count(&s, fresh())), (10, 0));

    let open = |view: Option<search_index::PublishedView>| {
        let handles: Vec<SegmentHandle> = out
            .segments
            .iter()
            .map(|f| {
                let h = SegmentHandle::new(f.data.clone());
                match &f.live_docs {
                    Some(b) => h.with_live_bytes(b).unwrap(),
                    None => h,
                }
            })
            .collect();
        let cfg = ShardConfig { merge_threads: 0, local_checkpoint: Some(out.local_checkpoint), ..Default::default() };
        Shard::open_published(cfg, schema(), handles, view).unwrap()
    };
    let r = open(Some(view.clone()));
    assert_eq!((count(&r, all()), count(&r, fresh())), (10, 0));
    assert!(r.has_pending_changes());
    assert!(r.get(b"3", false).unwrap().is_some(), "deleted after the refresh: still searchable");
    assert!(r.get(b"3", true).unwrap().is_none(), "but gone for realtime reads");
    assert!(r.get(b"new1", false).unwrap().is_none());
    assert_eq!(r.get(b"new1", true).unwrap().unwrap().seq_no, 10);
    assert_eq!(r.get(b"4", false).unwrap().unwrap().version, 1);
    assert_eq!(r.get(b"4", true).unwrap().unwrap().version, 2);
    // A leaf sort reorders what is shown without revealing the rest.
    r.set_leaf_sort(Some(BODY));
    assert_eq!(count(&r, all()), 10);
    r.set_leaf_sort(None);
    assert!(r.refresh().unwrap(), "the refresh publishes the hidden segment and deletes");
    assert_eq!((count(&r, all()), count(&r, fresh())), (12, 3));
    assert!(r.get(b"3", false).unwrap().is_none());
    // Without the view everything is searchable, as after a restart; a commit of a shard
    // whose searcher shows everything carries no view.
    let r2 = open(None);
    assert_eq!((count(&r2, all()), count(&r2, fresh())), (12, 3));
    assert!(r.flush().unwrap().published.is_none());
    // A view that doesn't fit the segments is refused.
    let bad = search_index::PublishedView { hidden: vec![7], ..Default::default() };
    let handles = vec![SegmentHandle::new(out.segments[0].data.clone())];
    assert!(Shard::open_published(ShardConfig::default(), schema(), handles, Some(bad)).is_err());
}

/// A merge installed after the last refresh: the searcher still shows the merged-away
/// segments, which a view over the commit can't express, so the commit counts as all
/// published.
#[test]
fn a_commit_describes_a_searcher_over_segments_merged_since() {
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(), Vec::new()).unwrap();
    // Refresh until the merge that runs inline after a refresh merges something: the searcher
    // still shows the merged segments.
    let mut n = 0;
    while s.segments().len() == s.searcher().segments().len() {
        assert!(n < 100, "no merge");
        s.index(IndexOp::new(doc(&format!("{n}"), &["w"]))).unwrap();
        s.refresh().unwrap();
        n += 1;
    }
    let all = || Arc::new(MatchAllQuery) as QueryRef;
    let fresh = || Arc::new(TermQuery::new(BODY, "fresh")) as QueryRef;
    // Not refreshed: a new doc, and a delete of a doc the merged segment holds.
    s.index(IndexOp::new(doc("x", &["w", "fresh"]))).unwrap();
    s.delete(DeleteOp::new(b"0".to_vec())).unwrap();
    let out = s.flush().unwrap();
    let view = out.published.clone().expect("the merge is traced back to what the searcher showed");
    assert_eq!(view.merged.len(), 1, "{view:?}");
    assert!(view.merged[0].1.is_empty(), "every doc of the merged segment was shown: {view:?}");
    assert_eq!(count(&s, all()), n);
    let r = reopen_with(&out, Some(view));
    assert_eq!((count(&r, all()), count(&r, fresh())), (n, 0), "the reopened shard shows what the searcher showed");
    assert!(r.get(b"0", false).unwrap().is_some(), "deleted after the refresh: still searchable");
    assert!(r.get(b"0", true).unwrap().is_none());
    assert!(r.refresh().unwrap());
    assert_eq!((count(&r, all()), count(&r, fresh())), (n, 1));
    // Once a refresh published the merged segment, commits need no trace of it.
    assert!(s.refresh().unwrap());
    assert!(s.flush().unwrap().published.is_none());
}


/// Natural merges BP-reorder only results of at least 2^18 docs (the size Lucene's
/// `BPReorderingMergePolicy` suggests for natural merges); a forced merge reorders from 2^16.
#[test]
fn bp_reorders_forced_merges_and_only_large_natural_ones() {
    use search_index::merge::policy::MergePolicyConfig;
    const N: usize = 100_000;
    // Docs alternate between two vocabularies: BP pulls each topic together, merge order keeps
    // them interleaved.
    let load = |segments_per_tier: f64| {
        let merge_policy = MergePolicyConfig { segments_per_tier, ..Default::default() };
        let s = Shard::open(ShardConfig { merge_threads: 0, merge_policy, ..Default::default() }, schema(), Vec::new()).unwrap();
        for i in 0..N {
            let topic = if i % 2 == 0 { 'a' } else { 'b' };
            let words: Vec<String> = (0..4).map(|w| format!("{topic}{}", (i / 2 + w) % 8)).collect();
            let words: Vec<&str> = words.iter().map(String::as_str).collect();
            s.index(IndexOp::new(doc(&format!("{i}"), &words))).unwrap();
            if i % 10_000 == 9_999 {
                s.refresh().unwrap();
            }
        }
        s.refresh().unwrap();
        s
    };
    // Share of neighbouring docs (in doc id order) of the same topic.
    let clustered = |s: &Shard| {
        let mut docs: Vec<(u32, String)> = ids(&s.searcher()).into_iter().collect();
        docs.sort();
        assert_eq!(docs.len(), N);
        let parity = |id: &str| id.parse::<usize>().unwrap() % 2;
        docs.windows(2).filter(|w| parity(&w[0].1) == parity(&w[1].1)).count() as f64 / (docs.len() - 1) as f64
    };
    // Two segments per tier: the refreshes merge naturally, into a segment of more than 2^16
    // docs (which BP used to reorder) but less than 2^18.
    let natural = load(2.0);
    let largest = natural.segments().iter().map(|g| g.max_doc).max().unwrap();
    assert!(largest >= 1 << 16, "{:?}", natural.segments().iter().map(|g| g.max_doc).collect::<Vec<_>>());
    assert!(clustered(&natural) < 0.01, "a natural merge under 2^18 docs was reordered");
    // Fifty per tier: the refreshes stay apart until a forced merge joins them.
    let forced = load(50.0);
    assert!(forced.segments().len() > 1);
    forced.force_merge(Some(1), false).unwrap();
    assert_eq!(forced.segments().len(), 1);
    assert!(clustered(&forced) > 0.95, "the forced merge kept the merge order");
}

/// `index.codec: best_compression` (`Shard::set_best_compression`): segments written after it
/// store the same values in fewer bytes.
#[test]
fn best_compression_stores_the_same_values_smaller() {
    let stored_bytes = |best: bool| {
        let s = Shard::in_memory(schema());
        s.set_best_compression(best);
        for i in 0..3000 {
            let mut d = doc(&i.to_string(), &["w1"]);
            let src = format!(r#"{{"name":"place {i}","country":"{}","population":{},"tags":["city","town"]}}"#, ["FR", "DE", "IT"][i % 3], i * 37 % 1000);
            d.stored.push(search_index::doc::StoredField { field: 20, value: search_codec::stored::OwnedStoredValue::Bytes(src.into_bytes()) });
            s.index(IndexOp::new(d)).unwrap();
        }
        s.refresh().unwrap();
        let se = s.searcher();
        let mut values = Vec::new();
        for seg in se.segments().iter() {
            for d in 0..seg.max_doc() {
                values.push(seg.reader.stored().document(d).unwrap());
            }
        }
        let size: u64 = se.segments().iter().map(|seg| seg.reader.size_in_bytes()).sum();
        (size, values)
    };
    let (default, a) = stored_bytes(false);
    let (best, b) = stored_bytes(true);
    assert_eq!(a, b);
    assert!(best < default, "best_compression {best} B, default {default} B");
}
