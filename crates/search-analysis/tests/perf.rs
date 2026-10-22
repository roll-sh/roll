//! Allocation discipline and throughput of analysis chains.
//!
//! `steady_state_allocates_nothing` counts heap allocations with a wrapping global allocator:
//! once an analyzer has seen a document, analyzing it again must not allocate at all (token
//! records, term buffers, lookahead queues and stemmer scratch are all reused).
//! `throughput` (ignored; run with `--release -- --ignored --nocapture`) reports MB/s.
//! `find_allocating_words` (ignored) narrows a failure down to the words that allocate.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use search_analysis::{Analysis, IndexOptions};

struct Counting;

static COUNT: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ACTIVE.load(Ordering::Relaxed) {
            COUNT.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ACTIVE.load(Ordering::Relaxed) {
            COUNT.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const TEXT: &str = "The QUICK brown-fox jumped over the lazy dog's bone, didn't it? Mr. O'Neil's I.B.M. AT&T \
    sent 1,000.50 dollars to john.doe@example.com via https://www.elastic.co/guide; PowerShot SD500 wi-fi \
    Ünïcödé façade naïve straße 我是中国人 日本語のテキスト 한국어 😀 👍🏽 new york ny usa running ponies \
    ภาษาไทยง่ายนิดเดียว กรุงเทพมหานคร детьми книгами bromfietsen kinderen undersøgelserne kauppojen \
    kitaplarımızdan házainkban macchinette ordenadores fiskerne questionários dentistes orduetatik \
    biblioteques պատմություններ cheannaigh dideliems gerações";

fn analysis() -> Analysis {
    let settings = serde_json::json!({
        "filter": {
            "syn": {"type": "synonym_graph", "synonyms": ["ny, new york", "usa, united states", "quick, fast"]},
            "wdg": {"type": "word_delimiter_graph", "catenate_all": true, "preserve_original": true},
            "sh": {"type": "shingle", "min_shingle_size": 2, "max_shingle_size": 3},
            "ng": {"type": "edge_ngram", "min_gram": 1, "max_gram": 3},
            "en": {"type": "stemmer", "language": "light_english"},
            "fold": {"type": "asciifolding", "preserve_original": true},
            "cg": {"type": "common_grams", "common_words": "_english_"},
            "rep": {"type": "pattern_replace", "pattern": "(\\d+)", "replacement": "<$1>"}
        },
        "char_filter": {
            "map": {"type": "mapping", "mappings": ["& => and", "😀 => smile"]}
        },
        "analyzer": {
            "graph": {"tokenizer": "standard", "filter": ["lowercase", "syn", "flatten_graph", "en"]},
            "delim": {"tokenizer": "whitespace", "filter": ["wdg", "lowercase", "fold", "unique"]},
            "grams": {"tokenizer": "standard", "filter": ["lowercase", "sh", "ng", "cg"]},
            "chars": {"tokenizer": "standard", "char_filter": ["html_strip", "map"], "filter": ["lowercase", "rep", "porter_stem", "kstem"]}
        }
    });
    Analysis::from_settings(&settings, &IndexOptions::default()).unwrap()
}

const ANALYZERS: &[&str] = &[
    "standard",
    "simple",
    "whitespace",
    "stop",
    "keyword",
    "pattern",
    "classic",
    "english",
    "french",
    "german",
    "spanish",
    "russian",
    "cjk",
    "arabic",
    "hindi",
    "greek",
    "thai",
    "dutch",
    "danish",
    "finnish",
    "turkish",
    "hungarian",
    "italian",
    "norwegian",
    "portuguese",
    "brazilian",
    "catalan",
    "basque",
    "armenian",
    "irish",
    "lithuanian",
    "galician",
    "czech",
    "bulgarian",
    "indonesian",
    "sorani",
    "persian",
    "bengali",
    "latvian",
    "romanian",
    "estonian",
    "swedish",
    "snowball",
    "serbian",
    "fingerprint",
    "graph",
    "delim",
    "grams",
    "chars",
];

#[test]
fn steady_state_allocates_nothing() {
    let a = analysis();
    let mut failures = Vec::new();
    for name in ANALYZERS {
        let analyzer = a
            .analyzer(name)
            .unwrap_or_else(|| panic!("analyzer {name}"));
        let mut n = 0usize;
        // Warm up: chains, buffers and pools reach their steady-state sizes. (Pooled token
        // records trade term buffers between runs, so capacities take a few runs to settle.)
        for _ in 0..8 {
            analyzer.analyze(TEXT, &mut |_| n += 1);
        }
        COUNT.store(0, Ordering::SeqCst);
        ACTIVE.store(true, Ordering::SeqCst);
        analyzer.analyze(TEXT, &mut |_| n += 1);
        ACTIVE.store(false, Ordering::SeqCst);
        let allocs = COUNT.load(Ordering::SeqCst);
        if allocs != 0 {
            failures.push(format!("{name}: {allocs} allocations"));
        }
        // The fingerprint of TEXT is longer than max_output_size (255), so it has no token.
        assert!(n > 0 || *name == "fingerprint", "{name} produced no tokens");
    }
    assert!(
        failures.is_empty(),
        "allocations in steady state: {failures:?}"
    );
}

#[test]
#[ignore]
fn throughput() {
    // About 8 MB in documents of about 4 KB.
    let mut doc = String::new();
    while doc.len() < 4096 {
        doc.push_str(TEXT);
        doc.push(' ');
    }
    let docs: Vec<&str> = (0..(8 << 20) / doc.len()).map(|_| doc.as_str()).collect();
    let a = analysis();
    // The tokenizers alone, to separate tokenizing from filtering costs.
    for tk in [
        "standard",
        "whitespace",
        "classic",
        "uax_url_email",
        "letter",
    ] {
        let analyzer = a
            .build_ad_hoc_json(&serde_json::json!({ "tokenizer": tk }))
            .unwrap();
        let mut tokens = 0usize;
        // Warm up (the URL/e-mail grammar is compiled on first use).
        for d in &docs[..16] {
            analyzer.analyze(d, &mut |_| tokens += 1);
        }
        tokens = 0;
        let start = Instant::now();
        let mut bytes = 0usize;
        for d in &docs {
            analyzer.analyze(d, &mut |_| tokens += 1);
            bytes += d.len();
        }
        let secs = start.elapsed().as_secs_f64();
        println!(
            "{:>10}: {:7.1} MB/s, {:6.1} M tokens/s (tokenizer only)",
            tk,
            bytes as f64 / secs / 1e6,
            tokens as f64 / secs / 1e6
        );
    }
    for name in [
        "standard",
        "whitespace",
        "english",
        "russian",
        "dutch",
        "thai",
        "graph",
        "chars",
    ] {
        let analyzer = a.analyzer(name).unwrap();
        let mut tokens = 0usize;
        for d in &docs[..16] {
            analyzer.analyze(d, &mut |_| tokens += 1);
        }
        tokens = 0;
        let start = Instant::now();
        let mut bytes = 0usize;
        for d in &docs {
            analyzer.analyze(d, &mut |_| tokens += 1);
            bytes += d.len();
        }
        let secs = start.elapsed().as_secs_f64();
        println!(
            "{name:>10}: {:7.1} MB/s, {:6.1} M tokens/s",
            bytes as f64 / secs / 1e6,
            tokens as f64 / secs / 1e6
        );
    }
}

/// Diagnostics for `steady_state_allocates_nothing`: which words of the sample text make an
/// analyzer allocate (`ANALYZER=<name> cargo test --test perf find_allocating_words -- --ignored
/// --nocapture`).
#[test]
#[ignore]
fn find_allocating_words() {
    let a = analysis();
    let name = std::env::var("ANALYZER").unwrap_or_else(|_| "graph".into());
    let analyzer = a.analyzer(&name).unwrap();
    for _ in 0..5 {
        analyzer.analyze(TEXT, &mut |_| {});
    }
    for w in TEXT.split(' ') {
        for _ in 0..3 {
            analyzer.analyze(w, &mut |_| {});
        }
        COUNT.store(0, Ordering::SeqCst);
        ACTIVE.store(true, Ordering::SeqCst);
        analyzer.analyze(w, &mut |_| {});
        ACTIVE.store(false, Ordering::SeqCst);
        let n = COUNT.load(Ordering::SeqCst);
        if n > 0 {
            println!("{w:?}: {n}");
        }
    }
    for i in 0..5 {
        COUNT.store(0, Ordering::SeqCst);
        ACTIVE.store(true, Ordering::SeqCst);
        analyzer.analyze(TEXT, &mut |_| {});
        ACTIVE.store(false, Ordering::SeqCst);
        println!("full text run {i}: {}", COUNT.load(Ordering::SeqCst));
    }
}

/// Building a large synonym set (ignored; run with `--release -- --ignored --nocapture`):
/// production indices carry tens of thousands of rules.
#[test]
#[ignore]
fn synonym_build_time() {
    let mut rules = Vec::new();
    for i in 0..100_000u32 {
        rules.push(format!("w{i} x{i}, alt{i}, z{} q{}", i % 977, i % 13));
    }
    let settings = serde_json::json!({
        "filter": {"syn": {"type": "synonym_graph", "synonyms": rules}},
        "analyzer": {"s": {"tokenizer": "standard", "filter": ["lowercase", "syn"]}}
    });
    let start = Instant::now();
    let a = Analysis::from_settings(&settings, &IndexOptions::default()).unwrap();
    let built = start.elapsed();
    let analyzer = a.analyzer("s").unwrap();
    let mut n = 0;
    let start = Instant::now();
    for i in 0..100_000u32 {
        analyzer.analyze(&format!("the w{i} x{i} went"), &mut |_| n += 1);
    }
    println!(
        "100k rules built in {:?}; 100k queries in {:?} ({n} tokens)",
        built,
        start.elapsed()
    );
}
