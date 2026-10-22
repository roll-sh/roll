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
    let cfg = ShardConfig { merge_threads: 0, gc_deletes: Duration::from_millis(1), ..Default::default() };
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
    std::thread::sleep(Duration::from_millis(250));
    s.delete(DeleteOp::new(b"last".to_vec())).unwrap();
    let st = s.stats();
    assert!(st.version_map_entries <= 1, "{}", st.version_map_entries);
    assert!(st.version_map_bytes < 1000, "{}", st.version_map_bytes);
    // A recent tombstone still answers version checks.
    let mut op = IndexOp::new(doc("last", &["w"]));
    op.op_type = OpType::Create;
    assert!(matches!(s.index(op).unwrap(), OpResult::Indexed { created: true, version: 2, .. }));
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
