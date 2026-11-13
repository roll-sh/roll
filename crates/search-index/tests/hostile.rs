//! Hostile inputs: invalid documents fail with Lucene's / Elasticsearch's messages and leave
//! the shard consistent; corrupt segments, term vectors and live docs fail to open instead of
//! panicking or allocating from counts they claim (the 1 GiB allocation cap of `common`
//! applies); abusive queries fail with the documented errors or run within bounds.

mod common;

use std::sync::Arc;

use common::{BODY, NUM, TAG, schema, to_indexable};
use rand::{Rng, RngCore};
use search_codec::Slice;
use search_index::collector::{
    CollapseCollector, CollapseField, FieldSort, Missing, SortKey, SortMode, SortType, SortValue, TopFieldCollector, TopScoreCollector,
};
use search_index::doc::{DocValue, IndexableDoc, IndexableField, PreToken};
use search_index::highlight::{HighlightOptions, highlight};
use search_index::query::intervals::IntervalsSource;
use search_index::query::{
    BooleanQuery, IntervalQuery, KnnQuery, MultiTermQuery, PhraseQuery, QueryVector, RewriteMethod, SpanNode, SpanQuery, TermQuery,
};
use search_index::{IndexOp, QueryRef, Shard, ShardConfig};

fn shard_with_docs(n: usize) -> Shard {
    let mut rng = common::rng(3);
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(), Vec::new()).unwrap();
    for i in 0..n {
        s.index(IndexOp::new(to_indexable(&common::random_doc(&mut rng, i)))).unwrap();
    }
    s.refresh().unwrap();
    s
}

fn err_text(r: search_index::Result<impl std::fmt::Debug>) -> String {
    match r {
        Ok(v) => panic!("expected an error, got {v:?}"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn invalid_documents_are_rejected_and_leave_no_trace() {
    let s = shard_with_docs(50);
    let before = s.searcher().num_docs();
    let tok = |t: &[u8], inc: u32, a: u32, b: u32| PreToken::new(t.to_vec(), inc, a, b);
    let bad: Vec<(IndexableDoc, &str)> = vec![
        (IndexableDoc::new(b"x1".to_vec()).field(IndexableField::Tokens { field: BODY, tokens: vec![tok(&vec![b'a'; 40_000], 1, 0, 1)] }), "immense term"),
        (
            IndexableDoc::new(b"x2".to_vec()).field(IndexableField::Tokens { field: BODY, tokens: vec![tok(b"a", 1, 5, 6), tok(b"b", 1, 2, 3)] }),
            "offsets must not go backwards",
        ),
        (
            IndexableDoc::new(b"x3".to_vec()).field(IndexableField::Tokens { field: BODY, tokens: vec![tok(b"a", 1, 0, 1), tok(b"b", i32::MAX as u32, 2, 3)] }),
            "position",
        ),
        (IndexableDoc::new(b"x4".to_vec()).field(IndexableField::Point { field: NUM, packed: vec![1, 2, 3] }), "point"),
        (
            IndexableDoc::new(b"x5".to_vec()).field(IndexableField::DocValue { field: TAG, value: DocValue::SortedSet(vec![b'z'; 40_000].into()) }),
            "too large",
        ),
        (IndexableDoc::new(Vec::<u8>::new()), "_id"),
        (IndexableDoc::new(b"x7".to_vec()).field(IndexableField::Term { field: 999, term: b"a".to_vec().into() }), "unknown field"),
    ];
    for (doc, want) in bad {
        let e = err_text(s.index(IndexOp::new(doc)));
        assert!(e.contains(want), "error {e:?} should mention {want:?}");
    }
    // A good doc after the bad ones; nothing of the bad ones is visible.
    s.index(IndexOp::new(IndexableDoc::new(b"ok".to_vec()).field(IndexableField::Tokens { field: BODY, tokens: vec![tok(b"zz", 1, 0, 2)] }))).unwrap();
    s.refresh().unwrap();
    let se = s.searcher();
    assert_eq!(se.num_docs(), before + 1);
    for id in ["x1", "x2", "x3", "x4", "x5", "x7"] {
        assert!(s.get(id.as_bytes(), true).unwrap().is_none(), "{id} is visible");
    }
    let q: QueryRef = Arc::new(TermQuery::new(BODY, b"a".to_vec()));
    assert_eq!(se.count(&q).unwrap(), 0);
    s.force_merge(Some(1), false).unwrap();
    assert_eq!(s.searcher().num_docs(), before + 1);
}

#[test]
fn corrupt_segments_fail_to_open() {
    let s = shard_with_docs(300);
    let out = s.flush().unwrap();
    let seg = &out.segments[0];
    let good = seg.data.as_slice().to_vec();
    let mut rng = common::rng(11);
    let open = |bytes: Vec<u8>| {
        let h = search_index::SegmentHandle::new(Slice::from(bytes));
        Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(), vec![h])
    };
    // The intact copy opens.
    assert_eq!(open(good.clone()).unwrap().searcher().num_docs(), 300);
    // Garbage, truncations and bit flips never panic.
    for i in 0..300 {
        let mut b = good.clone();
        match i % 4 {
            0 => {
                let n = rng.gen_range(0..64);
                b = vec![0u8; n];
                rng.fill_bytes(&mut b);
            }
            1 => b.truncate(rng.gen_range(0..b.len())),
            2 => {
                for _ in 0..rng.gen_range(1..8) {
                    let k = rng.gen_range(0..b.len());
                    b[k] ^= 1 << rng.gen_range(0..8);
                }
            }
            _ => {
                // Huge counts in plausible places: overwrite a random 8-byte window with 0xff.
                let k = rng.gen_range(0..b.len().saturating_sub(8));
                b[k..k + 8].fill(0xff);
            }
        }
        if let Ok(sh) = open(b) {
            // A flip the checksums missed must still not break searching.
            let se = sh.searcher();
            let q: QueryRef = Arc::new(TermQuery::new(BODY, b"w1".to_vec()));
            let mut c = TopScoreCollector::new(10, 100);
            let _ = se.search(&q, &mut c);
        }
    }
    // Corrupt live docs.
    for _ in 0..50 {
        let mut b = vec![0u8; rng.gen_range(0..40)];
        rng.fill_bytes(&mut b);
        let h = search_index::SegmentHandle::new(Slice::from(good.clone()));
        let _ = h.with_live_bytes(&b);
    }
}

#[test]
fn corrupt_term_vectors_fail_to_decode() {
    let mut rng = common::rng(5);
    for _ in 0..5000 {
        let mut b = vec![0u8; rng.gen_range(0..64)];
        rng.fill_bytes(&mut b);
        if rng.gen_bool(0.3) && !b.is_empty() {
            b[0] &= 7;
            if b.len() > 2 {
                b[1] = 0xff;
                b[2] = 0x7f;
            }
        }
        let _ = search_index::termvectors::decode(&b);
    }
}

#[test]
fn abusive_queries_fail_cleanly() {
    let s = shard_with_docs(400);
    let se = s.searcher();
    let run = |q: QueryRef| -> search_index::Result<u64> {
        let mut c = TopScoreCollector::new(10, 1000);
        se.search(&q, &mut c)?;
        Ok(c.top_docs().total_hits)
    };
    // Too many clauses.
    let mut b = BooleanQuery::new();
    for i in 0..5000 {
        b = b.should(Arc::new(TermQuery::new(BODY, format!("t{i}").into_bytes())));
    }
    assert!(err_text(run(Arc::new(b))).contains("maxClauseCount"));
    // A scoring rewrite expanding beyond the clause limit (60 terms, limit lowered).
    let limited = se.with_limits(search_index::search::SearchLimits { max_clause_count: 10, ..Default::default() });
    let q: QueryRef = Arc::new(MultiTermQuery::prefix(BODY, b"w".to_vec()).with_rewrite(RewriteMethod::ScoringBoolean));
    let mut c = TopScoreCollector::new(10, 10);
    assert!(err_text(limited.search(&q, &mut c)).contains("maxClauseCount"));
    // Pathological automata.
    assert!(MultiTermQuery::regexp(BODY, "(a|aa){1000}{1000}", 0xff, false, 10_000).is_err());
    let w = MultiTermQuery::wildcard(BODY, &"*a".repeat(200), false);
    if let Ok(w) = w {
        run(Arc::new(w)).unwrap();
    }
    // Long phrases, wide spans and interval explosions.
    let terms: Vec<Vec<u8>> = (0..2000).map(|i| common::word(i % 60).into_bytes()).collect();
    run(Arc::new(PhraseQuery::new(BODY, terms.clone(), 5))).unwrap();
    let near = SpanNode::Near { clauses: terms.iter().take(200).map(|t| SpanNode::Term { field: BODY, term: t.clone() }).collect(), slop: 3, in_order: false };
    run(Arc::new(SpanQuery::new(near).unwrap())).unwrap();
    let wide = |i: usize| IntervalsSource::or(vec![IntervalsSource::phrase_terms([common::word(i), common::word(i + 1)]), IntervalsSource::phrase_terms([common::word(i + 2), common::word(i + 3)])]);
    let blow = IntervalsSource::ordered((0..14).map(wide).collect());
    let blow = IntervalsSource::max_gaps(3, blow);
    assert!(err_text(run(Arc::new(IntervalQuery::new(BODY, blow)))).contains("Too many disjunctions"));
    // Deep nesting.
    let mut q: QueryRef = Arc::new(TermQuery::new(BODY, b"w1".to_vec()));
    for i in 0..64 {
        let b = BooleanQuery::new().should(q).should(Arc::new(TermQuery::new(BODY, common::word(i % 60).into_bytes())));
        q = Arc::new(b);
    }
    run(q).unwrap();
    // Vectors of the wrong shape and bad search_after / collapse inputs.
    let k: QueryRef = Arc::new(KnnQuery::new(BODY, QueryVector::Float(vec![1.0; 3]), 3, 10));
    let _ = run(k);
    let keys = vec![SortKey::Field(FieldSort { field: NUM, kind: SortType::Long, reverse: false, missing: Missing::Last, mode: SortMode::Min, nested: None })];
    let mut c = TopFieldCollector::new(keys, 10, 100, false).with_searcher(&se).with_after(vec![SortValue::Long(1), SortValue::Long(2)], None);
    let all: QueryRef = Arc::new(search_index::query::MatchAllQuery);
    assert!(err_text(se.search(&all, &mut c)).contains("search_after"));
    let mut c = CollapseCollector::new(CollapseField::Keyword(TAG), Vec::new(), 10, 100, false);
    let e = se.search(&all, &mut c);
    if let Err(e) = e {
        assert!(e.to_string().contains("single valued"), "{e}");
    }
    // Highlighting a field longer than the analyzed offset limit.
    let long = "word ".repeat(300_000);
    let opts = HighlightOptions { index_max_analyzed_offset: 1000, ..Default::default() };
    let hq: QueryRef = Arc::new(TermQuery::new(BODY, b"word".to_vec()));
    let e = highlight(&hq, &[long.as_str()], None, BODY, &opts).unwrap_err().to_string();
    assert!(e.contains("max_analyzed_offset"), "{e}");
    let opts = HighlightOptions { index_max_analyzed_offset: 1000, max_analyzed_offset: Some(100), ..Default::default() };
    highlight(&hq, &[long.as_str()], None, BODY, &opts).unwrap();
}


/// Suggesters, joins and slices with abusive inputs stay bounded.
#[test]
fn abusive_suggest_and_join_inputs_stay_bounded() {
    use search_index::phrase_suggest::{DirectGenerator, PhraseSuggestOptions, PhraseToken, Smoothing};
    use search_index::query::{JoinQuery, JoinScoreMode, MultiPhrasePrefixQuery, SliceBy, SliceQuery};
    use search_index::spell::{SpellOptions, SuggestMode};
    let shard = shard_with_docs(400);
    let s = shard.searcher();
    // Very long and odd spelling inputs.
    let long = "x".repeat(4000);
    let opts = SpellOptions { suggest_mode: SuggestMode::Always, min_word_length: 1, ..Default::default() };
    for t in [long.as_str(), "", "\u{1F600}\u{1F600}", "a"] {
        s.suggest_similar(BODY, t, 5, &opts).unwrap();
    }
    assert_eq!(err_text(s.suggest_similar(BODY, "abc", 5, &SpellOptions { max_edits: 3, ..Default::default() })), "Invalid maxEdits");
    // Phrase suggester: the token limit bounds the search; many candidates per token still finish.
    let tokens: Vec<PhraseToken> =
        (0..9).map(|i| PhraseToken { term: common::word(i).into_bytes(), position_increment: 1, shingle: false, synonym: false }).collect();
    let opts = PhraseSuggestOptions {
        field: BODY,
        real_word_error_likelihood: 0.95,
        confidence: 0.0,
        max_errors: 9.0,
        separator: b" ".to_vec(),
        shard_size: 5,
        gram_size: 3,
        require_unigram: true,
        token_limit: 10,
        smoothing: Smoothing::StupidBackoff { discount: 0.4 },
        generators: vec![DirectGenerator {
            field: BODY,
            spell: SpellOptions { suggest_mode: SuggestMode::Always, min_word_length: 1, ..Default::default() },
            size: 5,
            pre_filter: None,
            post_filter: None,
        }],
    };
    let r = s.phrase_suggest(&tokens, &opts);
    assert!(r.is_ok() || err_text(r).contains("exceeded maximum combinations"));
    let many: Vec<PhraseToken> = (0..20).map(|i| PhraseToken { term: common::word(i).into_bytes(), ..tokens[0].clone() }).collect();
    assert!(s.phrase_suggest(&many, &opts).unwrap().corrections.is_empty(), "over the token limit");
    // A join over a field without doc values matches nothing; slices validate their bounds.
    let j: QueryRef = Arc::new(JoinQuery::new(TAG, Arc::new(TermQuery::new(BODY, "w1")), Arc::new(TermQuery::new(BODY, "w2")), JoinScoreMode::Max));
    assert_eq!(s.count(&j).unwrap(), 0);
    assert_eq!(err_text(SliceQuery::new(SliceBy::DocId, 3, 3)), "max must be greater than id");
    // A phrase prefix over every term stays within max_expansions.
    let mpp: QueryRef = Arc::new(MultiPhrasePrefixQuery::new(BODY, vec![vec![b"w".to_vec()]], vec![0], 0, 3).unwrap());
    let r = s.rewrite(&mpp).unwrap();
    assert!(r.lucene_string(s.schema()).matches(' ').count() <= 2, "{}", r.lucene_string(s.schema()));
}
