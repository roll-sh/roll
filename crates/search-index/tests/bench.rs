//! Benchmarks (ignored by default): `cargo test -p search-index --release --test bench --
//! --ignored --nocapture`. A Zipfian corpus (vocabulary 50 000, 40 tokens per doc on average)
//! measures indexing throughput (1 and 4 writers), refresh latency, and query latency of term,
//! conjunction, disjunction, phrase and top-k queries on one merged segment.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use search_codec::{DocValuesType, IndexOptions, PointsSpec};
use search_index::collector::{TopScoreCollector, TotalHitCountCollector};
use search_index::doc::{DocValue, IndexableDoc, IndexableField, PreToken};
use search_index::query::{BooleanQuery, PhraseQuery, TermQuery};
use search_index::schema::FieldSchema;
use search_index::{IndexOp, QueryRef, Schema, Shard, ShardConfig};

const BODY: u32 = 10;
const NUM: u32 = 11;
const VOCAB: usize = 50_000;

fn schema() -> Schema {
    Schema::new()
        .with_field(FieldSchema::new(BODY, "body").indexed(IndexOptions::DocsAndFreqsAndPositions, true))
        .unwrap()
        .with_field(FieldSchema::new(NUM, "num").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
}

/// Zipf(1) sampler over `n` ranks via the inverse of the harmonic CDF approximation.
struct Zipf {
    cdf: Vec<f64>,
}

impl Zipf {
    fn new(n: usize) -> Zipf {
        let mut cdf = Vec::with_capacity(n);
        let mut s = 0.0;
        for i in 1..=n {
            s += 1.0 / i as f64;
            cdf.push(s);
        }
        for c in cdf.iter_mut() {
            *c /= s;
        }
        Zipf { cdf }
    }
    fn sample(&self, rng: &mut StdRng) -> usize {
        let u: f64 = rng.r#gen();
        self.cdf.partition_point(|&c| c < u).min(self.cdf.len() - 1)
    }
}

fn word(i: usize) -> Vec<u8> {
    format!("t{i}").into_bytes()
}

fn make_doc(rng: &mut StdRng, z: &Zipf, i: usize) -> IndexableDoc {
    let n = rng.gen_range(10..70);
    let mut off = 0u32;
    let tokens = (0..n)
        .map(|_| {
            let w = word(z.sample(rng));
            let t = PreToken::new(w.clone(), 1, off, off + w.len() as u32);
            off += w.len() as u32 + 1;
            t
        })
        .collect();
    let v = rng.gen_range(0..1_000_000i64);
    IndexableDoc::new(format!("d{i}").into_bytes())
        .field(IndexableField::Tokens { field: BODY, tokens })
        .field(IndexableField::Point { field: NUM, packed: search_codec::points::encode::i64_bytes(v).to_vec() })
        .field(IndexableField::DocValue { field: NUM, value: DocValue::SortedNumeric(v) })
}

fn docs(n: usize, seed: u64) -> Vec<IndexableDoc> {
    let z = Zipf::new(VOCAB);
    let mut rng = StdRng::seed_from_u64(seed);
    (0..n).map(|i| make_doc(&mut rng, &z, i)).collect()
}

fn per_sec(n: usize, d: Duration) -> f64 {
    n as f64 / d.as_secs_f64()
}

#[test]
#[ignore]
fn bench_indexing_and_refresh() {
    let n = 200_000;
    let all = docs(n, 1);
    // One writer.
    let s = Shard::in_memory(schema());
    let t = Instant::now();
    for d in all.iter().cloned() {
        s.index(IndexOp::new(d)).unwrap();
    }
    s.refresh().unwrap();
    let e = t.elapsed();
    println!("indexing, 1 writer: {:.0} docs/s ({n} docs in {:.2?})", per_sec(n, e), e);
    // Four writers.
    let s = Arc::new(Shard::in_memory(schema()));
    let chunks: Vec<Vec<IndexableDoc>> = all.chunks(n / 4).map(|c| c.to_vec()).collect();
    let t = Instant::now();
    std::thread::scope(|sc| {
        for c in chunks {
            let s = s.clone();
            sc.spawn(move || {
                for d in c {
                    s.index(IndexOp::new(d)).unwrap();
                }
            });
        }
    });
    s.refresh().unwrap();
    let e = t.elapsed();
    println!("indexing, 4 writers: {:.0} docs/s", per_sec(n, e));
    // Refresh latency after small batches, merging in the background (`in_memory` shards
    // merge inline, inside the refresh that adds the tenth segment of a tier).
    let s = Shard::open(ShardConfig::default(), schema(), Vec::new()).unwrap();
    let more = docs(20_000, 2);
    let mut lat = Vec::new();
    for c in more.chunks(1000) {
        for d in c.iter().cloned() {
            s.index(IndexOp::new(d)).unwrap();
        }
        let t = Instant::now();
        s.refresh().unwrap();
        lat.push(t.elapsed());
    }
    if std::env::var_os("BENCH_REFRESH_ALL").is_some() {
        println!("refresh latencies: {lat:?}");
    }
    lat.sort();
    println!("refresh after 1000 docs: median {:.2?}, max {:.2?}", lat[lat.len() / 2], lat[lat.len() - 1]);
}

fn time_queries(name: &str, s: &search_index::Searcher, qs: &[QueryRef], k: usize, threshold: u64) {
    // Warm up.
    for q in qs.iter().take(10) {
        let mut c = TopScoreCollector::new(k, threshold);
        s.search(q, &mut c).unwrap();
    }
    let t = Instant::now();
    let mut hits = 0u64;
    for q in qs {
        let mut c = TopScoreCollector::new(k, threshold);
        s.search(q, &mut c).unwrap();
        hits += c.top_docs().total_hits;
    }
    let e = t.elapsed();
    println!("{name}: {:.1} µs/query ({} queries, avg total {})", e.as_secs_f64() * 1e6 / qs.len() as f64, qs.len(), hits / qs.len() as u64);
}

#[test]
#[ignore]
fn bench_queries() {
    let n = 300_000;
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(), Vec::new()).unwrap();
    for d in docs(n, 7) {
        s.index(IndexOp::new(d)).unwrap();
    }
    s.refresh().unwrap();
    s.force_merge(Some(1), false).unwrap();
    let se = s.searcher();
    let mut rng = StdRng::seed_from_u64(9);
    let pick = |rng: &mut StdRng, lo: usize, hi: usize| word(rng.gen_range(lo..hi));
    let tq = |w: Vec<u8>| -> QueryRef { Arc::new(TermQuery::new(BODY, w)) };
    let m = 300;
    let freq: Vec<QueryRef> = (0..m).map(|_| tq(pick(&mut rng, 0, 20))).collect();
    let mid: Vec<QueryRef> = (0..m).map(|_| tq(pick(&mut rng, 100, 1000))).collect();
    let and2: Vec<QueryRef> = (0..m)
        .map(|_| Arc::new(BooleanQuery::new().must(tq(pick(&mut rng, 0, 50))).must(tq(pick(&mut rng, 50, 500)))) as QueryRef)
        .collect();
    let or2: Vec<QueryRef> = (0..m)
        .map(|_| Arc::new(BooleanQuery::new().should(tq(pick(&mut rng, 0, 50))).should(tq(pick(&mut rng, 50, 500)))) as QueryRef)
        .collect();
    let or4: Vec<QueryRef> = (0..m)
        .map(|_| {
            let mut b = BooleanQuery::new();
            for _ in 0..4 {
                b = b.should(tq(pick(&mut rng, 0, 2000)));
            }
            Arc::new(b) as QueryRef
        })
        .collect();
    let phrase: Vec<QueryRef> =
        (0..m).map(|_| Arc::new(PhraseQuery::new(BODY, vec![pick(&mut rng, 0, 30), pick(&mut rng, 0, 30)], 0)) as QueryRef).collect();
    let sloppy: Vec<QueryRef> =
        (0..m).map(|_| Arc::new(PhraseQuery::new(BODY, vec![pick(&mut rng, 0, 30), pick(&mut rng, 0, 30)], 2)) as QueryRef).collect();
    println!("corpus: {n} docs, 1 segment");
    time_queries("term (frequent), top-10, total>=1000", &se, &freq, 10, 1000);
    time_queries("term (frequent), top-10, exact total", &se, &freq, 10, u64::MAX);
    time_queries("term (medium), top-10", &se, &mid, 10, 1000);
    time_queries("AND of 2, top-10", &se, &and2, 10, 1000);
    time_queries("OR of 2, top-10 (MaxScore)", &se, &or2, 10, 1000);
    time_queries("OR of 2, exact total", &se, &or2, 10, u64::MAX);
    time_queries("OR of 4, top-10", &se, &or4, 10, 1000);
    time_queries("exact phrase, top-10", &se, &phrase, 10, 1000);
    time_queries("sloppy phrase ~2, top-10", &se, &sloppy, 10, 1000);
    let t = Instant::now();
    for q in &freq {
        se.count(q).unwrap();
    }
    println!("count (term, shortcut): {:.1} µs/query", t.elapsed().as_secs_f64() * 1e6 / freq.len() as f64);
    let t = Instant::now();
    for q in &or2 {
        let mut c = TotalHitCountCollector::default();
        se.search(q, &mut c).unwrap();
    }
    println!("count (OR of 2, iterate): {:.1} µs/query", t.elapsed().as_secs_f64() * 1e6 / or2.len() as f64);
}

#[test]
#[ignore]
fn bench_breakdown() {
    use search_index::{Collector, LeafCollector, Scorable, ScoreMode};
    let n = 300_000;
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(), Vec::new()).unwrap();
    for d in docs(n, 7) {
        s.index(IndexOp::new(d)).unwrap();
    }
    s.refresh().unwrap();
    s.force_merge(Some(1), false).unwrap();
    let se = s.searcher();
    let seg = &se.segments()[0];
    let terms: Vec<Vec<u8>> = (0..20).map(word).collect();
    let tr = seg.reader.terms(BODY).unwrap();
    // 1. Raw cursor iteration.
    let t = Instant::now();
    let mut total = 0u64;
    for _ in 0..5 {
        for w in &terms {
            let st = tr.seek_exact(w).unwrap();
            let mut c = tr.postings(&st, search_codec::PostingsFlags::FREQS);
            loop {
                let d = c.next();
                if d == search_codec::NO_MORE_DOCS {
                    break;
                }
                total += c.freq() as u64;
            }
        }
    }
    let docs_n: u64 = terms.iter().map(|w| tr.seek_exact(w).unwrap().doc_freq as u64).sum::<u64>() * 5;
    println!("cursor next+freq: {:.2} ns/doc ({total})", t.elapsed().as_nanos() as f64 / docs_n as f64);
    // 2. Scorer next + score.
    let qs: Vec<QueryRef> = terms.iter().map(|w| Arc::new(TermQuery::new(BODY, w.clone())) as QueryRef).collect();
    let t = Instant::now();
    let mut acc = 0f32;
    for _ in 0..5 {
        for q in &qs {
            let w = se.create_weight(q, ScoreMode::Complete).unwrap();
            let mut sc = w.scorer(seg).unwrap().unwrap();
            loop {
                let d = sc.next();
                if d == search_codec::NO_MORE_DOCS {
                    break;
                }
                acc += sc.score();
            }
        }
    }
    println!("scorer next+score: {:.2} ns/doc ({acc})", t.elapsed().as_nanos() as f64 / docs_n as f64);
    // 3. Bulk scorer into a trivial collector.
    struct Sum(f64);
    struct L<'a>(&'a mut f64);
    impl LeafCollector for L<'_> {
        fn collect(&mut self, _d: u32, s: &mut dyn Scorable) -> search_index::Result<()> {
            *self.0 += s.score() as f64;
            Ok(())
        }
    }
    impl Collector for Sum {
        fn score_mode(&self) -> ScoreMode {
            ScoreMode::Complete
        }
        fn leaf<'a>(&'a mut self, _s: &search_index::SegmentContext) -> search_index::Result<Box<dyn LeafCollector + 'a>> {
            Ok(Box::new(L(&mut self.0)))
        }
    }
    let t = Instant::now();
    let mut c = Sum(0.0);
    for _ in 0..5 {
        for q in &qs {
            se.search(q, &mut c).unwrap();
        }
    }
    println!("bulk + trivial collector: {:.2} ns/doc ({})", t.elapsed().as_nanos() as f64 / docs_n as f64, c.0);
    for variant in ["default", "term"] {
        let t = Instant::now();
        let mut acc = 0f64;
        for _ in 0..5 {
            for q in &qs {
                let w = se.create_weight(q, ScoreMode::Complete).unwrap();
                let mut bs: Box<dyn search_index::BulkScorer> = if variant == "default" {
                    Box::new(search_index::search::DefaultBulkScorer::new(w.scorer(seg).unwrap().unwrap()))
                } else {
                    w.bulk_scorer(seg, ScoreMode::Complete).unwrap().unwrap()
                };
                let mut l = L(&mut acc);
                bs.score(&mut l, None, 0, search_codec::NO_MORE_DOCS).unwrap();
            }
        }
        println!("bulk {variant}: {:.2} ns/doc ({acc})", t.elapsed().as_nanos() as f64 / docs_n as f64);
    }
    // 4. Top-10 exact.
    let t = Instant::now();
    for _ in 0..5 {
        for q in &qs {
            let mut c = TopScoreCollector::new(10, u64::MAX);
            se.search(q, &mut c).unwrap();
        }
    }
    println!("top-10 exact: {:.2} ns/doc", t.elapsed().as_nanos() as f64 / docs_n as f64);
}

#[test]
#[ignore]
fn bench_conjunction_breakdown() {
    use search_index::ScoreMode;
    let n = 300_000;
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(), Vec::new()).unwrap();
    for d in docs(n, 7) {
        s.index(IndexOp::new(d)).unwrap();
    }
    s.refresh().unwrap();
    s.force_merge(Some(1), false).unwrap();
    let se = s.searcher();
    let seg = &se.segments()[0];
    let tr = seg.reader.terms(BODY).unwrap();
    let mut rng = StdRng::seed_from_u64(9);
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..200).map(|_| (word(rng.gen_range(0..50)), word(rng.gen_range(50..500)))).collect();
    // Raw cursor leapfrog.
    let t = Instant::now();
    let mut hits = 0u64;
    let mut leads = 0u64;
    for (a, b) in &pairs {
        let (sa, sb) = (tr.seek_exact(a).unwrap(), tr.seek_exact(b).unwrap());
        let (mut x, mut y) = (tr.postings(&sa, search_codec::PostingsFlags::FREQS), tr.postings(&sb, search_codec::PostingsFlags::FREQS));
        leads += sb.doc_freq as u64;
        let mut d = y.next();
        while d != search_codec::NO_MORE_DOCS {
            let e = x.advance(d);
            if e == d {
                hits += 1;
                d = y.next();
            } else {
                d = y.advance(e);
            }
        }
    }
    println!("cursor leapfrog: {:.1} ns/lead doc ({hits} hits)", t.elapsed().as_nanos() as f64 / leads as f64);
    let qs: Vec<QueryRef> = pairs
        .iter()
        .map(|(a, b)| Arc::new(BooleanQuery::new().must(Arc::new(TermQuery::new(BODY, a.clone()))).must(Arc::new(TermQuery::new(BODY, b.clone())))) as QueryRef)
        .collect();
    for mode in [ScoreMode::CompleteNoScores, ScoreMode::Complete, ScoreMode::TopScores] {
        let t = Instant::now();
        let mut h = 0u64;
        for q in &qs {
            let w = se.create_weight(q, mode).unwrap();
            let mut sc = w.scorer(seg).unwrap().unwrap();
            loop {
                let d = sc.next();
                if d == search_codec::NO_MORE_DOCS {
                    break;
                }
                if mode.needs_scores() {
                    let _ = sc.score();
                }
                h += 1;
            }
        }
        println!("conjunction scorer {mode:?}: {:.1} ns/lead doc ({h} hits)", t.elapsed().as_nanos() as f64 / leads as f64);
    }
    // BENCH_ROUNDS=<n> repeats the top-10 searches (for a profiler).
    let rounds: usize = std::env::var("BENCH_ROUNDS").ok().and_then(|r| r.parse().ok()).unwrap_or(1);
    for _ in 0..rounds {
        let t = Instant::now();
        for q in &qs {
            let mut c = TopScoreCollector::new(10, 1000);
            se.search(q, &mut c).unwrap();
        }
        println!("search top-10: {:.1} ns/lead doc", t.elapsed().as_nanos() as f64 / leads as f64);
    }
}

/// The query sets shared with the Lucene harness (`benches/lucene/`): one line per query,
/// `<set>\t<op>\t<term> <term>...` where op is `term`, `and`, `or`, `phrase` or `sloppy2`.
fn comparison_queries() -> Vec<(&'static str, &'static str, Vec<Vec<u8>>)> {
    let mut rng = StdRng::seed_from_u64(9);
    let mut out = Vec::new();
    let m = 300;
    let mut pick = |lo: usize, hi: usize| word(rng.gen_range(lo..hi));
    for _ in 0..m {
        out.push(("term-frequent", "term", vec![pick(0, 20)]));
    }
    for _ in 0..m {
        out.push(("term-medium", "term", vec![pick(100, 1000)]));
    }
    for _ in 0..m {
        out.push(("and2", "and", vec![pick(0, 50), pick(50, 500)]));
    }
    for _ in 0..m {
        out.push(("and3", "and", vec![pick(0, 50), pick(0, 200), pick(50, 2000)]));
    }
    for _ in 0..m {
        out.push(("or2", "or", vec![pick(0, 50), pick(50, 500)]));
    }
    for _ in 0..m {
        out.push(("or4", "or", (0..4).map(|_| pick(0, 2000)).collect()));
    }
    for _ in 0..m {
        out.push(("phrase", "phrase", vec![pick(0, 30), pick(0, 30)]));
    }
    for _ in 0..m {
        out.push(("phrase3", "phrase", vec![pick(0, 30), pick(0, 30), pick(0, 30)]));
    }
    for _ in 0..m {
        out.push(("sloppy2", "sloppy2", vec![pick(0, 30), pick(0, 30)]));
    }
    // Sorted by the numeric field (top 10 by `num` descending): every doc, a frequent term.
    for _ in 0..m {
        out.push(("sort-all", "sortall", vec![]));
    }
    for _ in 0..m {
        out.push(("sort-term", "sortterm", vec![pick(0, 20)]));
    }
    out
}

/// Runs the comparison query sets against the same corpus the Lucene harness indexes and
/// prints `set µs/query` (best of 5 rounds, top-10 with a total-hits threshold of 1000). With
/// `BENCH_EXPORT=<dir>` it also writes `corpus.txt` and `queries.txt` for the Lucene harness.
/// `BENCH_BP=0` disables BP reordering on the force merge (Lucene's default).
#[test]
#[ignore]
fn bench_vs_lucene() {
    use std::collections::BTreeMap;
    use std::io::Write;
    let n = 300_000;
    let all = docs(n, 7);
    let qs = comparison_queries();
    if let Ok(dir) = std::env::var("BENCH_EXPORT") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut f = std::io::BufWriter::new(std::fs::File::create(dir.join("corpus.txt")).unwrap());
        for d in &all {
            for fld in &d.fields {
                if let IndexableField::Tokens { tokens, .. } = fld {
                    let line: Vec<&[u8]> = tokens.iter().map(|t| &t.term[..]).collect();
                    f.write_all(&line.join(&b' ')).unwrap();
                    f.write_all(b"\n").unwrap();
                }
            }
        }
        let mut f = std::io::BufWriter::new(std::fs::File::create(dir.join("nums.txt")).unwrap());
        for d in &all {
            for fld in &d.fields {
                if let IndexableField::DocValue { value: DocValue::SortedNumeric(v), .. } = fld {
                    writeln!(f, "{v}").unwrap();
                }
            }
        }
        let mut f = std::io::BufWriter::new(std::fs::File::create(dir.join("queries.txt")).unwrap());
        for (set, op, terms) in &qs {
            let t: Vec<String> = terms.iter().map(|t| String::from_utf8_lossy(t).into_owned()).collect();
            writeln!(f, "{set}\t{op}\t{}", t.join(" ")).unwrap();
        }
    }
    let bp = std::env::var("BENCH_BP").map_or(true, |v| v != "0");
    let s = Shard::open(ShardConfig { merge_threads: 0, bp_reorder: bp, ..Default::default() }, schema(), Vec::new()).unwrap();
    for d in all {
        s.index(IndexOp::new(d)).unwrap();
    }
    s.refresh().unwrap();
    s.force_merge(Some(1), false).unwrap();
    let se = s.searcher();
    let mut sets: BTreeMap<&str, Vec<QueryRef>> = BTreeMap::new();
    let tq = |w: &Vec<u8>| -> QueryRef { Arc::new(TermQuery::new(BODY, w.clone())) };
    for (set, op, terms) in &qs {
        let q: QueryRef = match *op {
            "sortall" => Arc::new(search_index::query::MatchAllQuery),
            "sortterm" => tq(&terms[0]),
            "term" => tq(&terms[0]),
            "and" => Arc::new(terms.iter().fold(BooleanQuery::new(), |b, t| b.must(tq(t)))),
            "or" => Arc::new(terms.iter().fold(BooleanQuery::new(), |b, t| b.should(tq(t)))),
            "phrase" => Arc::new(PhraseQuery::new(BODY, terms.clone(), 0)),
            _ => Arc::new(PhraseQuery::new(BODY, terms.clone(), 2)),
        };
        sets.entry(set).or_default().push(q);
    }
    println!("roll: {n} docs, 1 segment, bp={bp}");
    // BENCH_ONLY=<set> BENCH_ROUNDS=<n> keep one set running (for a profiler).
    let only = std::env::var("BENCH_ONLY").ok();
    let rounds: usize = std::env::var("BENCH_ROUNDS").ok().and_then(|r| r.parse().ok()).unwrap_or(5);
    for (set, qs) in &sets {
        if only.as_deref().is_some_and(|o| o != *set) {
            continue;
        }
        let mut best = f64::MAX;
        let mut hits = 0u64;
        for _ in 0..rounds {
            hits = 0;
            let t = Instant::now();
            for q in qs {
                if set.starts_with("sort-") {
                    use search_index::collector::{FieldSort, Missing, SortKey, SortMode, SortType, TopFieldCollector};
                    let key = SortKey::Field(FieldSort { field: NUM, kind: SortType::Long, reverse: true, missing: Missing::Last, mode: SortMode::Max, nested: None });
                    // BENCH_SORT_THRESHOLD: total hits counted exactly (e.g. `max` disables pruning).
                    let threshold = std::env::var("BENCH_SORT_THRESHOLD").ok().map_or(1000, |v| v.parse().unwrap_or(u64::MAX));
                    let mut c = TopFieldCollector::new(vec![key], 10, threshold, false).with_searcher(&se);
                    se.search(q, &mut c).unwrap();
                    hits += c.top_docs().total_hits;
                    continue;
                }
                let mut c = TopScoreCollector::new(10, 1000);
                se.search(q, &mut c).unwrap();
                hits += c.top_docs().total_hits;
            }
            best = best.min(t.elapsed().as_secs_f64() * 1e6 / qs.len() as f64);
        }
        println!("{set:14} {best:9.1} µs/query  (avg total {})", hits / qs.len() as u64);
    }
}

/// Intra-segment concurrency: the comparison sets with exact totals (`track_total_hits:
/// true`, where every match is visited) on one 300k-doc segment, sequential vs split into
/// `BENCH_PARALLEL` (4) partitions.
#[test]
#[ignore]
fn bench_partitions() {
    use std::collections::BTreeMap;
    use search_index::SearchControl;
    use search_index::collector::TopScoreManager;
    use search_index::search::SearchLimits;
    let n = 300_000;
    let s = Shard::open(ShardConfig { merge_threads: 0, bp_reorder: false, ..Default::default() }, schema(), Vec::new()).unwrap();
    for d in docs(n, 7) {
        s.index(IndexOp::new(d)).unwrap();
    }
    s.refresh().unwrap();
    s.force_merge(Some(1), false).unwrap();
    let se = s.searcher();
    let threads: usize = std::env::var("BENCH_PARALLEL").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
    let par = se.with_limits(SearchLimits { parallelism: threads, parallel_min_docs: 30_000, ..SearchLimits::default() });
    println!("{} slices: {:?}", par.slices().len(), par.slices());
    let tq = |w: &Vec<u8>| -> QueryRef { Arc::new(TermQuery::new(BODY, w.clone())) };
    let mut sets: BTreeMap<&str, Vec<QueryRef>> = BTreeMap::new();
    for (set, op, terms) in comparison_queries() {
        let q: QueryRef = match op {
            "term" => tq(&terms[0]),
            "and" => Arc::new(terms.iter().fold(BooleanQuery::new(), |b, t| b.must(tq(t)))),
            "or" => Arc::new(terms.iter().fold(BooleanQuery::new(), |b, t| b.should(tq(t)))),
            "phrase" => Arc::new(PhraseQuery::new(BODY, terms.clone(), 0)),
            _ => continue,
        };
        sets.entry(set).or_default().push(q);
    }
    let ctl = SearchControl::default();
    for (set, qs) in &sets {
        let (mut seq, mut conc) = (f64::MAX, f64::MAX);
        for _ in 0..3 {
            let t = Instant::now();
            let mut a = 0u64;
            for q in qs {
                let mut c = TopScoreCollector::new(10, u64::MAX);
                se.search(q, &mut c).unwrap();
                a += c.top_docs().total_hits;
            }
            seq = seq.min(t.elapsed().as_secs_f64() * 1e6 / qs.len() as f64);
            let t = Instant::now();
            let mut b = 0u64;
            for q in qs {
                let m = TopScoreManager { n: 10, threshold: u64::MAX, after: None, min_score: None };
                b += par.search_concurrent(q, &m, &ctl).unwrap().total_hits;
            }
            conc = conc.min(t.elapsed().as_secs_f64() * 1e6 / qs.len() as f64);
            assert_eq!(a, b);
        }
        println!("{set:14} sequential {seq:9.1} µs  {threads} partitions {conc:9.1} µs  ({:.2}x)", seq / conc);
        if std::env::var_os("BENCH_PART_DETAIL").is_some() {
            // Each partition alone, on this thread.
            let parts: Vec<_> = par.slices().into_iter().flatten().collect();
            let mut line = String::new();
            for p in &parts {
                let t = Instant::now();
                for q in qs {
                    let w = se.create_weight(q, search_index::ScoreMode::Complete).unwrap();
                    let mut c = TopScoreCollector::new(10, u64::MAX);
                    search_index::search::search_partitions(std::slice::from_ref(p), w.as_ref(), &mut c, &ctl).unwrap();
                }
                line += &format!(" {:.1}", t.elapsed().as_secs_f64() * 1e6 / qs.len() as f64);
            }
            println!("    per partition µs:{line}");
        }
    }
}

/// Where exact phrase time goes on the comparison corpus: the docs conjunction alone, the
/// conjunction plus reading every position, counting phrase matches, and top-10.
#[test]
#[ignore]
fn bench_phrase_breakdown() {
    let n = 300_000;
    let bp = std::env::var("BENCH_BP").map_or(true, |v| v != "0");
    let s = Shard::open(ShardConfig { merge_threads: 0, bp_reorder: bp, ..Default::default() }, schema(), Vec::new()).unwrap();
    for d in docs(n, 7) {
        s.index(IndexOp::new(d)).unwrap();
    }
    s.refresh().unwrap();
    s.force_merge(Some(1), false).unwrap();
    let se = s.searcher();
    let seg = &se.segments()[0];
    let tr = seg.reader.terms(BODY).unwrap();
    // BENCH_SET=phrase3 measures the three-term phrases (the raw loops use their first two).
    let set = std::env::var("BENCH_SET").unwrap_or_else(|_| "phrase".into());
    let pairs: Vec<Vec<Vec<u8>>> = comparison_queries().into_iter().filter(|(s, _, _)| *s == set).map(|(_, _, t)| t).collect();
    let rounds: usize = std::env::var("BENCH_ROUNDS").ok().and_then(|r| r.parse().ok()).unwrap_or(1);
    let per_q = |t: Instant| t.elapsed().as_nanos() as f64 / 1e3 / pairs.len() as f64;
    for _ in 0..rounds {
        let t = Instant::now();
        let mut cands = 0u64;
        let mut pos = 0u64;
        for p in &pairs {
            let (sa, sb) = (tr.seek_exact(&p[0]).unwrap(), tr.seek_exact(&p[1]).unwrap());
            let (mut x, mut y) = (tr.postings(&sa, search_codec::PostingsFlags::FREQS), tr.postings(&sb, search_codec::PostingsFlags::FREQS));
            let mut d = x.next();
            while d != search_codec::NO_MORE_DOCS {
                let e = y.advance(d);
                if e == d {
                    cands += 1;
                    d = x.next();
                } else {
                    d = x.advance(e);
                }
            }
        }
        println!("docs conjunction: {:.1} µs/query ({} candidates/query)", per_q(t), cands / pairs.len() as u64);
        if std::env::var_os("BENCH_CONJ_ONLY").is_some() {
            continue;
        }
        let t = Instant::now();
        for p in &pairs {
            let (sa, sb) = (tr.seek_exact(&p[0]).unwrap(), tr.seek_exact(&p[1]).unwrap());
            let (mut x, mut y) = (tr.postings(&sa, search_codec::PostingsFlags::POSITIONS), tr.postings(&sb, search_codec::PostingsFlags::POSITIONS));
            let mut d = x.next();
            while d != search_codec::NO_MORE_DOCS {
                let e = y.advance(d);
                if e == d {
                    for _ in 0..x.freq() {
                        pos += x.next_position() as u64;
                    }
                    for _ in 0..y.freq() {
                        pos += y.next_position() as u64;
                    }
                    d = x.next();
                } else {
                    d = x.advance(e);
                }
            }
        }
        println!("conjunction + all positions: {:.1} µs/query ({pos})", per_q(t));
        let qs: Vec<QueryRef> = pairs.iter().map(|p| Arc::new(PhraseQuery::new(BODY, p.clone(), 0)) as QueryRef).collect();
        let t = Instant::now();
        let mut hits = 0u64;
        for q in &qs {
            hits += se.count(q).unwrap();
        }
        println!("phrase count: {:.1} µs/query ({} hits/query)", per_q(t), hits / qs.len() as u64);
        // The same without a collector, and the approximation alone (the terms' conjunction
        // through the scorer stack).
        let t = Instant::now();
        let mut h = 0u64;
        for q in &qs {
            let w = se.create_weight(q, search_index::ScoreMode::CompleteNoScores).unwrap();
            let mut sc = w.scorer(seg).unwrap().unwrap();
            while sc.next() != search_codec::NO_MORE_DOCS {
                h += 1;
            }
        }
        println!("phrase scorer loop: {:.1} µs/query ({} hits/query)", per_q(t), h / qs.len() as u64);
        let t = Instant::now();
        let mut h = 0u64;
        for q in &qs {
            let w = se.create_weight(q, search_index::ScoreMode::CompleteNoScores).unwrap();
            let mut sc = w.scorer(seg).unwrap().unwrap();
            while sc.approx_next() != search_codec::NO_MORE_DOCS {
                h += 1;
            }
        }
        println!("phrase approximation loop: {:.1} µs/query ({} candidates/query)", per_q(t), h / qs.len() as u64);
        let t = Instant::now();
        for q in &qs {
            let mut c = TopScoreCollector::new(10, 1000);
            se.search(q, &mut c).unwrap();
        }
        println!("phrase top-10: {:.1} µs/query", per_q(t));
    }
}

/// Cost of confirming `match_only_text` phrase candidates from their values
/// (`SourceConfirmedQuery`): µs per candidate doc.
#[test]
#[ignore]
fn bench_source_confirmed() {
    use search_index::query::source_confirmed::{LeafSourceValues, SourceValues};
    use search_index::query::{ConstantScoreQuery, SourceConfirmedQuery};
    struct Vals(Arc<Vec<String>>);
    struct Leaf(Arc<Vec<String>>, u32);
    impl SourceValues for Vals {
        fn leaf(&self, seg: &search_index::SegmentContext) -> search_index::Result<Box<dyn LeafSourceValues>> {
            Ok(Box::new(Leaf(self.0.clone(), seg.base)))
        }
    }
    impl LeafSourceValues for Leaf {
        fn values(&mut self, doc: u32) -> search_index::Result<Vec<String>> {
            Ok(vec![self.0[(self.1 + doc) as usize].clone()])
        }
    }
    let a = search_analysis::Analysis::global().analyzer("standard").unwrap();
    let schema = Schema::new().with_field(FieldSchema::new(BODY, "body").indexed(IndexOptions::Docs, false)).unwrap();
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema, Vec::new()).unwrap();
    let z = Zipf::new(2000);
    let mut rng = StdRng::seed_from_u64(3);
    let n = 20_000;
    let texts: Vec<String> = (0..n)
        .map(|_| (0..rng.gen_range(8..40)).map(|_| format!("t{}", z.sample(&mut rng))).collect::<Vec<_>>().join(" "))
        .collect();
    for (i, t) in texts.iter().enumerate() {
        s.index(IndexOp::new(IndexableDoc::new(format!("d{i}").into_bytes()).text(BODY, &a, t.clone()))).unwrap();
    }
    s.refresh().unwrap();
    s.force_merge(Some(1), false).unwrap();
    let se = s.searcher();
    let vals = Arc::new(texts);
    let mut cands = 0u64;
    let mut hits = 0u64;
    let t = Instant::now();
    for (x, y) in [(0, 1), (1, 2), (0, 3), (2, 5), (4, 7)] {
        let terms = vec![word(x), word(y)];
        let approx: QueryRef = Arc::new(BooleanQuery::new().filter(Arc::new(TermQuery::new(BODY, terms[0].clone()))).filter(Arc::new(TermQuery::new(BODY, terms[1].clone()))));
        cands += se.count(&approx).unwrap();
        let q: QueryRef = Arc::new(ConstantScoreQuery::new(Arc::new(SourceConfirmedQuery::new(Arc::new(PhraseQuery::new(BODY, terms, 0)), BODY, a.clone(), Arc::new(Vals(vals.clone()))))));
        hits += se.count(&q).unwrap();
    }
    let el = t.elapsed();
    println!("source confirmed: {cands} candidates, {hits} hits, {:.2} µs/candidate", el.as_secs_f64() * 1e6 / cands as f64);
}
