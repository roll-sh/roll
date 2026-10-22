//! Randomized oracle tests: every query type against a brute-force evaluator over random
//! corpora built with random segment layouts (refreshes, updates, deletes, merges). Doc sets
//! and scores must match exactly, for exhaustive collection and for pruned top-k collection.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::*;
use rand::Rng;
use rand::rngs::StdRng;
use search_codec::int_to_byte4;
use search_codec::automaton::Automaton;
use search_index::collector::TopScoreCollector;
use search_index::query::{
    BooleanQuery, BoostQuery, ConstantScoreQuery, DisMaxQuery, DocValuesRangeQuery, DocValuesTermsQuery, DvRange,
    IndexOrDocValuesQuery, MatchAllQuery, MultiTermQuery, Occur, PhraseQuery, PointRangeQuery, RewriteMethod, SynonymQuery,
    TermQuery,
};
use search_index::similarity::TermStats;
use search_index::search::Searcher;
use search_index::similarity::Similarity;
use search_index::{QueryRef, ShardConfig};

/// A query and how to evaluate it on a model doc.
#[derive(Clone, Debug)]
pub enum Q {
    Term(u32, String, f32),
    /// Exact phrase on BODY.
    Phrase(Vec<String>, f32),
    /// Constant-score multi-term queries.
    Prefix(u32, String, RewriteMethod),
    Wildcard(u32, String),
    TermRange(u32, Option<String>, Option<String>, bool, bool),
    TermSet(u32, Vec<String>),
    /// Points / doc values ranges on NUM.
    NumRange(i64, i64, u8),
    DvTerms(Vec<String>),
    Synonym(Vec<String>),
    DisMax(Vec<Q>, f32),
    /// Fuzzy on BODY (top terms blended).
    Fuzzy(String, u32),
    Bool(Vec<(Occur, Q)>, usize),
    Const(Box<Q>, f32),
    All,
}

pub fn field_words(d: &MDoc, f: u32) -> &[String] {
    match f {
        BODY => &d.body,
        TITLE => &d.title,
        TAG => &d.tags,
        _ => &[],
    }
}

impl Q {
    /// Mirrors the boolean rewrites that change how scores are summed (Lucene flattens nested
    /// pure disjunctions and conjunctions, which changes float rounding).
    pub fn normalize(&self) -> Q {
        match self {
            Q::Const(q, b) => Q::Const(Box::new(q.normalize()), *b),
            Q::Bool(cs, msm) => {
                let mut cs: Vec<(Occur, Q)> = cs.iter().map(|(o, q)| (*o, q.normalize())).collect();
                let mut msm = *msm;
                loop {
                    let mut changed = false;
                    // A single clause unwraps.
                    if cs.len() == 1 {
                        let (o, q) = &cs[0];
                        if (msm == 1 && *o == Occur::Should) || (msm == 0 && matches!(o, Occur::Should | Occur::Must)) {
                            return q.clone();
                        }
                    }
                    if msm <= 1 {
                        let mut out = Vec::new();
                        for (o, q) in &cs {
                            if let (Occur::Should, Q::Bool(ic, im)) = (o, q)
                                && *im <= 1
                                && ic.iter().all(|(io, _)| *io == Occur::Should)
                                && !ic.is_empty()
                            {
                                out.extend(ic.iter().cloned());
                                changed = true;
                            } else {
                                out.push((*o, q.clone()));
                            }
                        }
                        cs = out;
                    }
                    if !changed {
                        let mut out = Vec::new();
                        for (o, q) in &cs {
                            if let (Occur::Must | Occur::Filter, Q::Bool(ic, 0)) = (o, q)
                                && !ic.iter().any(|(io, _)| *io == Occur::Should)
                                && !ic.iter().all(|(io, _)| *io == Occur::MustNot)
                            {
                                for (io, iq) in ic {
                                    let no = if matches!(io, Occur::Filter | Occur::MustNot) || *o == Occur::Must { *io } else { Occur::Filter };
                                    out.push((no, iq.clone()));
                                }
                                changed = true;
                            } else {
                                out.push((*o, q.clone()));
                            }
                        }
                        cs = out;
                    }
                    let shoulds = cs.iter().filter(|(o, _)| *o == Occur::Should).count();
                    if !changed && shoulds > 0 && shoulds == msm {
                        for c in cs.iter_mut() {
                            if c.0 == Occur::Should {
                                c.0 = Occur::Must;
                            }
                        }
                        msm = 0;
                        changed = true;
                    }
                    let musts: Vec<&Q> = cs.iter().filter(|(o, _)| *o == Occur::Must).map(|(_, q)| q).collect();
                    if !changed && shoulds == 0 && musts.len() == 1
                        && let Q::Bool(ic, im) = musts[0]
                        && ic.iter().all(|(io, _)| *io == Occur::Should)
                        && !ic.is_empty()
                    {
                        let (ic, im) = (ic.clone(), *im);
                        cs.retain(|(o, _)| *o != Occur::Must);
                        cs.extend(ic);
                        msm = im.max(1);
                        changed = true;
                    }
                    if !changed {
                        break;
                    }
                }
                Q::Bool(cs, msm)
            }
            q => q.clone(),
        }
    }

    pub fn build(&self) -> QueryRef {
        match self {
            Q::Term(f, t, b) => {
                let q: QueryRef = Arc::new(TermQuery::new(*f, t.clone().into_bytes()));
                if *b != 1.0 { Arc::new(BoostQuery::new(q, *b)) } else { q }
            }
            Q::Bool(cs, msm) => {
                let mut b = BooleanQuery::new().with_msm(*msm);
                for (o, q) in cs {
                    b = b.add(*o, q.build());
                }
                Arc::new(b)
            }
            Q::Const(q, b) => {
                let c: QueryRef = Arc::new(ConstantScoreQuery::new(q.build()));
                if *b != 1.0 { Arc::new(BoostQuery::new(c, *b)) } else { c }
            }
            Q::All => Arc::new(MatchAllQuery),
            Q::Phrase(ts, b) => {
                let q: QueryRef = Arc::new(PhraseQuery::new(BODY, ts.iter().map(|t| t.clone().into_bytes()).collect(), 0));
                if *b != 1.0 { Arc::new(BoostQuery::new(q, *b)) } else { q }
            }
            Q::Prefix(f, p, r) => Arc::new(MultiTermQuery::prefix(*f, p.clone().into_bytes()).with_rewrite(*r)),
            Q::Wildcard(f, p) => Arc::new(MultiTermQuery::wildcard(*f, p, false).unwrap()),
            Q::TermRange(f, l, u, il, iu) => Arc::new(MultiTermQuery::range(
                *f,
                l.clone().map(|x| x.into_bytes()),
                u.clone().map(|x| x.into_bytes()),
                *il,
                *iu,
            )),
            Q::TermSet(f, ts) => Arc::new(MultiTermQuery::set(*f, ts.iter().map(|t| t.clone().into_bytes()))),
            Q::NumRange(lo, hi, kind) => match kind {
                0 => Arc::new(PointRangeQuery::long(NUM, *lo, *hi)),
                1 => Arc::new(DocValuesRangeQuery { field: NUM, range: DvRange::Long { lower: *lo, upper: *hi } }),
                _ => Arc::new(IndexOrDocValuesQuery {
                    index: Arc::new(PointRangeQuery::long(NUM, *lo, *hi)),
                    dv: Arc::new(DocValuesRangeQuery { field: NUM, range: DvRange::Long { lower: *lo, upper: *hi } }),
                }),
            },
            Q::DvTerms(ts) => Arc::new(DocValuesTermsQuery::new(TAG, ts.iter().map(|t| t.clone().into_bytes()))),
            Q::Synonym(ts) => Arc::new(SynonymQuery::new(BODY, ts.iter().map(|t| t.clone().into_bytes()).collect())),
            Q::DisMax(qs, tie) => Arc::new(DisMaxQuery::new(qs.iter().map(|q| q.build()).collect(), *tie)),
            Q::Fuzzy(t, e) => Arc::new(MultiTermQuery::fuzzy(BODY, t, *e, 0, 50, true)),
        }
    }

    /// Score of `d`, or None when it doesn't match. `scores`: whether the context scores.
    pub fn eval(&self, s: &Searcher, d: &MDoc, scores: bool) -> Option<f32> {
        match self {
            Q::All => Some(1.0),
            Q::Term(f, t, b) => {
                let words = field_words(d, *f);
                let freq = words.iter().filter(|w| *w == t).count();
                if freq == 0 {
                    return None;
                }
                if !scores {
                    return Some(0.0);
                }
                let coll = s.collection_stats(*f).unwrap();
                let ts = s.term_stats(*f, t.as_bytes());
                let sim = Similarity::bm25().scorer(*b, &coll, &[ts]);
                let norm = if *f == TAG { 1 } else { int_to_byte4(words.len() as u32) };
                let freq = if *f == TAG { 1.0 } else { freq as f32 };
                Some(sim.score(freq, norm))
            }
            Q::Const(q, b) => q.eval(s, d, false).map(|_| *b),
            Q::Phrase(ts, b) => {
                let w = &d.body;
                let n = ts.len();
                let mut freq = 0;
                for st in 0..w.len() {
                    if st + n <= w.len() && (0..n).all(|j| w[st + j] == ts[j]) {
                        freq += 1;
                    }
                }
                if freq == 0 {
                    return None;
                }
                if !scores {
                    return Some(0.0);
                }
                let coll = s.collection_stats(BODY).unwrap();
                let stats: Vec<TermStats> =
                    ts.iter().map(|t| s.term_stats(BODY, t.as_bytes())).filter(|st| st.doc_freq > 0).collect();
                let sim = Similarity::bm25().scorer(*b, &coll, &stats);
                Some(sim.score(freq as f32, int_to_byte4(w.len() as u32)))
            }
            Q::Prefix(f, p, _) => field_words(d, *f).iter().any(|w| w.starts_with(p.as_str())).then_some(1.0),
            Q::Wildcard(f, p) => {
                let re = glob(p);
                field_words(d, *f).iter().any(|w| re(w)).then_some(1.0)
            }
            Q::TermRange(f, l, u, il, iu) => field_words(d, *f)
                .iter()
                .any(|w| {
                    l.as_ref().is_none_or(|l| if *il { w >= l } else { w > l })
                        && u.as_ref().is_none_or(|u| if *iu { w <= u } else { w < u })
                })
                .then_some(1.0),
            Q::TermSet(f, ts) => field_words(d, *f).iter().any(|w| ts.contains(w)).then_some(1.0),
            Q::NumRange(lo, hi, _) => d.nums.iter().any(|v| v >= lo && v <= hi).then_some(1.0),
            Q::DvTerms(ts) => d.tags.iter().any(|w| ts.contains(w)).then_some(1.0),
            Q::Synonym(ts) => {
                // Duplicate synonyms count once per entry, as Lucene keeps one postings per entry.
                let freq: usize = ts.iter().map(|t| d.body.iter().filter(|w| *w == t).count()).sum();
                if freq == 0 {
                    return None;
                }
                if !scores {
                    return Some(0.0);
                }
                let coll = s.collection_stats(BODY).unwrap();
                let mut df = 0;
                let mut ttf = 0;
                for t in ts {
                    let st = s.term_stats(BODY, t.as_bytes());
                    if st.doc_freq > 0 {
                        df = df.max(st.doc_freq);
                        ttf += st.total_term_freq;
                    }
                }
                let sim = Similarity::bm25().scorer(1.0, &coll, &[TermStats { doc_freq: df, total_term_freq: ttf }]);
                Some(sim.score(freq as f32, int_to_byte4(d.body.len() as u32)))
            }
            Q::DisMax(qs, tie) => {
                let mut max = 0f32;
                let mut other = 0f64;
                let mut any = false;
                for q in qs {
                    if let Some(v) = q.eval(s, d, scores) {
                        any = true;
                        if v >= max {
                            other += max as f64;
                            max = v;
                        } else {
                            other += v as f64;
                        }
                    }
                }
                if !any {
                    return None;
                }
                Some(if *tie == 0.0 { max } else { (max as f64 + other * *tie as f64) as f32 })
            }
            Q::Fuzzy(t, e) => {
                // Candidate terms: vocabulary words within the edit distance that exist in the index.
                let fboost = |c: &str| {
                    let ed = search_codec::automaton::edit_distance(t, c, true);
                    if ed == 0 { 1.0f32 } else { 1.0f32 - ed as f32 / (t.chars().count().min(c.chars().count()) as f32) }
                };
                let mut cands: Vec<String> = (0..VOCAB + 5)
                    .map(word)
                    .filter(|w| search_codec::automaton::edit_distance(t, w, true) <= *e)
                    .filter(|w| s.term_stats(BODY, w.as_bytes()).doc_freq > 0)
                    .collect();
                cands.sort_by(|a, b| fboost(b).total_cmp(&fboost(a)).then(a.as_bytes().cmp(b.as_bytes())));
                cands.truncate(50);
                let mut df = 0;
                let mut ttf = 0;
                for c in &cands {
                    let st = s.term_stats(BODY, c.as_bytes());
                    df = df.max(st.doc_freq);
                    ttf += st.total_term_freq;
                }
                let coll = s.collection_stats(BODY)?;
                let mut sum = 0f64;
                let mut any = false;
                for c in &cands {
                    let freq = d.body.iter().filter(|w| *w == c).count();
                    if freq == 0 {
                        continue;
                    }
                    any = true;
                    if scores {
                        let boost = fboost(c);
                        let sim = Similarity::bm25().scorer(boost.max(0.0), &coll, &[TermStats { doc_freq: df, total_term_freq: ttf }]);
                        sum += sim.score(freq as f32, int_to_byte4(d.body.len() as u32)) as f64;
                    }
                }
                any.then_some(if scores { sum as f32 } else { 0.0 })
            }
            Q::Bool(cs, msm) => {
                let req = cs.iter().any(|(o, _)| matches!(o, Occur::Must | Occur::Filter));
                let shoulds = cs.iter().filter(|(o, _)| *o == Occur::Should).count();
                if cs.iter().all(|(o, _)| *o == Occur::MustNot) || cs.is_empty() || shoulds < *msm {
                    return None;
                }
                let mut sum = 0f64;
                let mut matched_should = 0;
                for (o, q) in cs {
                    let sc = matches!(o, Occur::Must | Occur::Should) && scores;
                    match (o, q.eval(s, d, sc)) {
                        (Occur::Must | Occur::Filter, None) => return None,
                        (Occur::MustNot, Some(_)) => return None,
                        (Occur::Must, Some(v)) => sum += if sc { v as f64 } else { 0.0 },
                        (Occur::Should, Some(v)) => {
                            matched_should += 1;
                            sum += if sc { v as f64 } else { 0.0 };
                        }
                        _ => {}
                    }
                }
                if matched_should < *msm || (!req && matched_should == 0) {
                    return None;
                }
                Some(if scores { sum as f32 } else { 0.0 })
            }
        }
    }
}

/// A `*`/`?` glob matcher.
fn glob(p: &str) -> impl Fn(&str) -> bool {
    let p: Vec<char> = p.chars().collect();
    move |s: &str| {
        let s: Vec<char> = s.chars().collect();
        fn m(p: &[char], s: &[char]) -> bool {
            match p.first() {
                None => s.is_empty(),
                Some('*') => (0..=s.len()).any(|i| m(&p[1..], &s[i..])),
                Some('?') => !s.is_empty() && m(&p[1..], &s[1..]),
                Some(c) => s.first() == Some(c) && m(&p[1..], &s[1..]),
            }
        }
        m(&p, &s)
    }
}

pub fn random_leaf(rng: &mut StdRng, boost: f32) -> Q {
    let bw = |rng: &mut StdRng| word(zipf(rng, VOCAB + 5));
    match rng.gen_range(0..22) {
        0..=6 => Q::Term(BODY, bw(rng), boost),
        7..=8 => Q::Term(TITLE, word(zipf(rng, 22)), boost),
        9 => Q::Term(TAG, format!("t{}", rng.gen_range(0..9)), boost),
        10 => Q::All,
        11 => {
            let n = rng.gen_range(2..4);
            // Phrases of words taken from a common prefix so they match sometimes.
            let start = zipf(rng, 8);
            Q::Phrase((0..n).map(|i| if rng.gen_bool(0.5) { word(start + i) } else { word(zipf(rng, 6)) }).collect(), boost)
        }
        12 => {
            let rw = [RewriteMethod::ConstantScoreBlended, RewriteMethod::ConstantScore][rng.gen_range(0..2)];
            Q::Prefix(BODY, format!("w{}", rng.gen_range(1..7)), rw)
        }
        13 => Q::Wildcard(BODY, ["w1?", "w*5", "*3", "w?", "w1*"][rng.gen_range(0..5)].to_string()),
        14 => {
            let a = format!("t{}", rng.gen_range(0..8));
            let b = format!("t{}", rng.gen_range(0..8));
            let (l, u) = if a <= b { (a, b) } else { (b, a) };
            Q::TermRange(TAG, rng.gen_bool(0.8).then_some(l), rng.gen_bool(0.8).then_some(u), rng.r#gen(), rng.r#gen())
        }
        15 => Q::TermSet(BODY, (0..rng.gen_range(1..25)).map(|_| bw(rng)).collect()),
        16 => {
            let a = rng.gen_range(-60..60);
            let b = rng.gen_range(-60..60);
            Q::NumRange(a.min(b), a.max(b), rng.gen_range(0..3))
        }
        17 => Q::DvTerms((0..rng.gen_range(1..4)).map(|_| format!("t{}", rng.gen_range(0..9))).collect()),
        18 => Q::Synonym((0..rng.gen_range(2..4)).map(|_| bw(rng)).collect()),
        19 => Q::DisMax((0..rng.gen_range(2..4)).map(|_| Q::Term(BODY, bw(rng), boost)).collect(), [0.0, 0.3, 1.0][rng.gen_range(0..3)]),
        20 => Q::Fuzzy(format!("w{}", rng.gen_range(1..70)), rng.gen_range(0..3)),
        _ => Q::Term(BODY, bw(rng), boost),
    }
}

pub fn random_q(rng: &mut StdRng, depth: usize) -> Q {
    let r: f64 = rng.r#gen();
    let boost = [1.0f32, 1.0, 2.0, 0.5, 4.0][rng.gen_range(0..5)];
    if depth == 0 || r < 0.35 {
        return random_leaf(rng, boost);
    }
    if r < 0.42 {
        return Q::Const(Box::new(random_q(rng, depth - 1)), boost);
    }
    let n = rng.gen_range(1..6);
    let mut cs = Vec::new();
    for _ in 0..n {
        let o = match rng.gen_range(0..10) {
            0..=2 => Occur::Must,
            3..=6 => Occur::Should,
            7..=8 => Occur::Filter,
            _ => Occur::MustNot,
        };
        cs.push((o, random_q(rng, depth - 1)));
    }
    let shoulds = cs.iter().filter(|(o, _)| *o == Occur::Should).count();
    let msm = if shoulds > 1 && rng.gen_bool(0.3) { rng.gen_range(1..=shoulds) } else { 0 };
    Q::Bool(cs, msm)
}

/// Words of a field by number (model).
fn words_of(d: &MDoc, f: u32) -> &[String] {
    field_words(d, f)
}

/// Brute-force evaluation of the engine's rewritten query tree: the expected score of `d`
/// under the exact query the engine runs (`boost` flows down as weights pass it).
pub fn eval_query(s: &Searcher, q: &QueryRef, d: &MDoc, scores: bool, boost: f32) -> Option<f32> {
    use search_index::query::basic::MatchNoneQuery;
    use search_index::query::{MultiPhraseQuery, PointInSetQuery, TermsMatcher};
    let a = q.as_any();
    if let Some(b) = a.downcast_ref::<BoostQuery>() {
        return eval_query(s, &b.query, d, scores, boost * b.boost);
    }
    if a.is::<MatchAllQuery>() {
        return Some(boost);
    }
    if a.is::<MatchNoneQuery>() {
        return None;
    }
    // Ranges covering every value rewrite to field-exists queries (Lucene 10.5).
    if let Some(e) = a.downcast_ref::<search_index::query::FieldExistsQuery>() {
        let has = match e.field {
            NUM => !d.nums.is_empty(),
            TAG => !d.tags.is_empty(),
            f => !words_of(d, f).is_empty(),
        };
        return has.then_some(boost);
    }
    if let Some(c) = a.downcast_ref::<ConstantScoreQuery>() {
        return eval_query(s, &c.query, d, false, 1.0).map(|_| boost);
    }
    if let Some(t) = a.downcast_ref::<TermQuery>() {
        let words = words_of(d, t.field);
        let tt = std::str::from_utf8(&t.term).unwrap();
        let freq = words.iter().filter(|w| *w == tt).count();
        if freq == 0 {
            return None;
        }
        if !scores {
            return Some(0.0);
        }
        let coll = s.collection_stats(t.field).unwrap();
        let ts = t.stats.unwrap_or_else(|| s.term_stats(t.field, &t.term));
        let sim = Similarity::bm25().scorer(boost, &coll, &[ts]);
        let norm = if t.field == TAG { 1 } else { int_to_byte4(words.len() as u32) };
        let f = if t.field == TAG { 1.0 } else { freq as f32 };
        return Some(sim.score(f, norm));
    }
    if let Some(b) = a.downcast_ref::<BooleanQuery>() {
        let cs = &b.clauses;
        let msm = b.minimum_should_match;
        let req = cs.iter().any(|c| matches!(c.occur, Occur::Must | Occur::Filter));
        let shoulds = cs.iter().filter(|c| c.occur == Occur::Should).count();
        if cs.iter().all(|c| c.occur == Occur::MustNot) || shoulds < msm {
            return None;
        }
        let mut must_sum = 0f64;
        let mut opt_sum = 0f64;
        let mut matched = 0;
        for c in cs {
            let sc = matches!(c.occur, Occur::Must | Occur::Should) && scores;
            match (c.occur, eval_query(s, &c.query, d, sc, boost)) {
                (Occur::Must | Occur::Filter, None) => return None,
                (Occur::MustNot, Some(_)) => return None,
                (Occur::Must, Some(v)) => must_sum += if sc { v as f64 } else { 0.0 },
                (Occur::Should, Some(v)) => {
                    matched += 1;
                    opt_sum += if sc { v as f64 } else { 0.0 };
                }
                _ => {}
            }
        }
        if matched < msm || (!req && matched == 0) {
            return None;
        }
        if !scores {
            return Some(0.0);
        }
        // Lucene rounds required and optional sums separately (ReqOptSumScorer).
        let has_must = cs.iter().any(|c| c.occur == Occur::Must);
        return Some(if has_must && shoulds > 0 && msm == 0 {
            (must_sum as f32) + (opt_sum as f32)
        } else {
            (must_sum + opt_sum) as f32
        });
    }
    if let Some(m) = a.downcast_ref::<MultiTermQuery>() {
        let words = words_of(d, m.field);
        let hit = words.iter().any(|w| {
            let w = w.as_bytes();
            match &m.matcher {
                TermsMatcher::Prefix(p) => w.starts_with(p),
                TermsMatcher::Automaton { aut, .. } => aut.matches(w),
                TermsMatcher::Fuzzy { aut, .. } => aut.matches(w),
                TermsMatcher::Set(t) => t.iter().any(|x| x.as_slice() == w),
                TermsMatcher::Range { lower, upper, include_lower, include_upper } => {
                    lower.as_ref().is_none_or(|l| if *include_lower { w >= l.as_slice() } else { w > l.as_slice() })
                        && upper.as_ref().is_none_or(|u| if *include_upper { w <= u.as_slice() } else { w < u.as_slice() })
                }
            }
        });
        return hit.then_some(boost);
    }
    if let Some(p) = a.downcast_ref::<PhraseQuery>() {
        let mq = MultiPhraseQuery { field: p.field, terms: p.terms.iter().map(|t| vec![t.clone()]).collect(), positions: p.positions.clone(), slop: p.slop };
        return eval_query(s, &(Arc::new(mq) as QueryRef), d, scores, boost);
    }
    if let Some(p) = a.downcast_ref::<MultiPhraseQuery>() {
        let w = words_of(d, p.field);
        let n = p.terms.len();
        assert_eq!(p.slop, 0);
        let mut freq = 0;
        for st in 0..w.len() {
            if (0..n).all(|j| {
                let at = st + p.positions[j] as usize;
                at < w.len() && p.terms[j].iter().any(|t| t.as_slice() == w[at].as_bytes())
            }) {
                freq += 1;
            }
        }
        if freq == 0 {
            return None;
        }
        if !scores {
            return Some(0.0);
        }
        let coll = s.collection_stats(p.field).unwrap();
        let stats: Vec<TermStats> =
            p.terms.iter().flatten().map(|t| s.term_stats(p.field, t)).filter(|st| st.doc_freq > 0).collect();
        let sim = Similarity::bm25().scorer(boost, &coll, &stats);
        return Some(sim.score(freq as f32, int_to_byte4(w.len() as u32)));
    }
    if let Some(sq) = a.downcast_ref::<SynonymQuery>() {
        let w = words_of(d, sq.field);
        let mut freq = 0f32;
        for (t, b) in &sq.terms {
            freq += *b * w.iter().filter(|x| x.as_bytes() == t.as_slice()).count() as f32;
        }
        if freq == 0.0 {
            return None;
        }
        if !scores {
            return Some(0.0);
        }
        let coll = s.collection_stats(sq.field).unwrap();
        let (mut df, mut ttf) = (0, 0);
        for (t, _) in &sq.terms {
            let st = s.term_stats(sq.field, t);
            if st.doc_freq > 0 {
                df = df.max(st.doc_freq);
                ttf += st.total_term_freq;
            }
        }
        let sim = Similarity::bm25().scorer(boost, &coll, &[TermStats { doc_freq: df, total_term_freq: ttf }]);
        return Some(sim.score(freq, int_to_byte4(w.len() as u32)));
    }
    if let Some(dm) = a.downcast_ref::<DisMaxQuery>() {
        let mut max = 0f32;
        let mut other = 0f64;
        let mut any = false;
        for sub in &dm.disjuncts {
            if let Some(v) = eval_query(s, sub, d, scores, boost) {
                any = true;
                if v >= max {
                    other += max as f64;
                    max = v;
                } else {
                    other += v as f64;
                }
            }
        }
        if !any {
            return None;
        }
        return Some(if dm.tie_breaker == 0.0 { max } else { (max as f64 + other * dm.tie_breaker as f64) as f32 });
    }
    if let Some(r) = a.downcast_ref::<PointRangeQuery>() {
        let lo = search_codec::points::encode::i64_from(&r.lower);
        let hi = search_codec::points::encode::i64_from(&r.upper);
        return d.nums.iter().any(|v| *v >= lo && *v <= hi).then_some(boost);
    }
    if let Some(r) = a.downcast_ref::<DocValuesRangeQuery>() {
        let DvRange::Long { lower, upper } = r.range else { unreachable!() };
        return d.nums.iter().any(|v| *v >= lower && *v <= upper).then_some(boost);
    }
    if let Some(r) = a.downcast_ref::<IndexOrDocValuesQuery>() {
        return eval_query(s, &r.index, d, scores, boost);
    }
    if let Some(r) = a.downcast_ref::<DocValuesTermsQuery>() {
        return d.tags.iter().any(|t| r.terms.iter().any(|x| x.as_slice() == t.as_bytes())).then_some(boost);
    }
    if a.is::<PointInSetQuery>() {
        unimplemented!()
    }
    panic!("eval_query: unsupported {q:?}")
}

/// Checks one query: exhaustive hits, pruned top-k, count and explanations.
pub fn check(s: &Searcher, ids: &HashMap<u32, String>, live: &HashMap<String, MDoc>, q: &Q, rng: &mut StdRng) {
    let query = q.build();
    let by_id: HashMap<&String, u32> = ids.iter().map(|(g, i)| (i, *g)).collect();
    let mut expected: Vec<(u32, f32)> = Vec::new();
    let rewritten = s.rewrite(&query).unwrap();
    for (id, d) in live {
        let semantic = q.eval(s, d, false).is_some();
        let sc = eval_query(s, &rewritten, d, true, 1.0);
        assert_eq!(sc.is_some(), semantic, "rewrite changed matching of {id} for {q:?}");
        if let Some(sc) = sc {
            expected.push((by_id[id], sc));
        }
    }
    expected.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    // Exhaustive. Scores may differ from the flat model in the last bits: Lucene (and so the
    // engine) rounds partial sums to float at scorer boundaries (required + optional), which
    // depends on which clauses exist in each segment. ES fixtures check exact values.
    let close = |a: f32, b: f32| a == b || (a - b).abs() <= 4.0 * f32::EPSILON * a.abs().max(b.abs());
    let exp_map: HashMap<u32, f32> = expected.iter().copied().collect();
    let mut c = TopScoreCollector::new(live.len() + 1, u64::MAX);
    s.search(&query, &mut c).unwrap();
    let got = c.top_docs();
    assert_eq!(got.total_hits as usize, expected.len(), "total for {q:?}");
    let gm: HashMap<u32, f32> = got.hits.iter().map(|h| (h.doc, h.score)).collect();
    for (d, sc) in &expected {
        match gm.get(d) {
            None => panic!("missing doc {} score {sc} for {q:?}", ids[d]),
            Some(g) if !close(*g, *sc) => panic!(
                "doc {} score {g} != expected {sc} for {q:?}\nrewritten: {}\n{}",
                ids[d],
                s.rewrite(&query).unwrap().lucene_string(s.schema()),
                s.explain(&query, *d).unwrap().to_text()
            ),
            _ => {}
        }
    }
    assert_eq!(gm.len(), expected.len(), "unexpected hits for {q:?}");
    assert_eq!(s.count(&query).unwrap() as usize, expected.len(), "count for {q:?}");
    // Pruned top-k must find hits as good as the exhaustive ones (same scores, rank by rank).
    let mut exhaustive: Vec<(u32, f32)> = got.hits.iter().map(|h| (h.doc, h.score)).collect();
    exhaustive.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    for k in [1usize, 3, 10] {
        let mut c = TopScoreCollector::new(k, k as u64);
        s.search(&query, &mut c).unwrap();
        let got: Vec<(u32, f32)> = c.top_docs().hits.iter().map(|h| (h.doc, h.score)).collect();
        let want: Vec<(u32, f32)> = exhaustive.iter().take(k).copied().collect();
        assert_eq!(got, want, "top-{k} for {q:?}\nrewritten: {}", s.rewrite(&query).unwrap().lucene_string(s.schema()));
        for (d, sc) in &got {
            assert!(close(exp_map[d], *sc));
        }
    }
    // Explanations of a few docs.
    let all: Vec<u32> = ids.keys().copied().collect();
    for _ in 0..3 {
        if all.is_empty() {
            break;
        }
        let g = all[rng.gen_range(0..all.len())];
        let e = s.explain(&query, g).unwrap();
        let exp = expected.iter().find(|(d, _)| *d == g);
        assert_eq!(e.is_match(), exp.is_some(), "explain match of {} for {q:?}: {}", ids[&g], e.to_text());
        if exp.is_some() {
            assert_eq!(e.value(), gm[&g], "explain value of {} for {q:?}: {}", ids[&g], e.to_text());
        }
    }
}

#[test]
fn boolean_and_term_queries_match_brute_force() {
    // ORACLE_SEEDS=<n> sweeps more corpora.
    let seeds: u64 = std::env::var("ORACLE_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(12);
    for seed in 0..seeds {
        let corpus = build(seed, 400 + (seed as usize % 4) * 300, ShardConfig { merge_threads: 0, ..Default::default() });
        let s = corpus.shard.searcher();
        let ids = ids(&s);
        assert_eq!(ids.len(), corpus.live.len());
        let mut rng = rng(seed * 7 + 1);
        for _ in 0..60 {
            let q = random_q(&mut rng, 3);
            check(&s, &ids, &corpus.live, &q, &mut rng);
        }
    }
}

mod sorting {
    use super::*;
    use search_index::collector::{FieldSort, Missing, SortKey, SortMode, SortType, SortValue, TopFieldCollector};
    use std::cmp::Ordering;

    /// Expected key value of a doc for a numeric sort on NUM.
    fn num_key(d: &MDoc, mode: SortMode) -> Option<i64> {
        let v = &d.nums;
        if v.is_empty() {
            return None;
        }
        let mut s = v.clone();
        s.sort();
        Some(match mode {
            SortMode::Min => s[0],
            SortMode::Max => *s.last().unwrap(),
            SortMode::Sum => s.iter().sum(),
            SortMode::Avg => {
                let t: i64 = s.iter().sum();
                if s.len() > 1 { (t as f64 / s.len() as f64 + 0.5).floor() as i64 } else { t }
            }
            SortMode::Median => {
                let n = s.len();
                if n.is_multiple_of(2) { ((s[n / 2 - 1] as f64 + s[n / 2] as f64) / 2.0 + 0.5).floor() as i64 } else { s[n / 2] }
            }
        })
    }

    fn tag_key(d: &MDoc, max: bool) -> Option<String> {
        if max { d.tags.iter().max().cloned() } else { d.tags.iter().min().cloned() }
    }

    #[test]
    fn field_sorts_match_brute_force() {
        for seed in 0..6u64 {
            let corpus = build(seed + 100, 600, ShardConfig { merge_threads: 0, ..Default::default() });
            let s = corpus.shard.searcher();
            let ids = ids(&s);
            let by_id: HashMap<&String, u32> = ids.iter().map(|(g, i)| (i, *g)).collect();
            let mut rng = rng(seed);
            for _ in 0..30 {
                let mode = [SortMode::Min, SortMode::Max, SortMode::Sum, SortMode::Avg, SortMode::Median][rng.gen_range(0..5)];
                let reverse: bool = rng.r#gen();
                let missing = match rng.gen_range(0..3) {
                    0 => Missing::First,
                    1 => Missing::Last,
                    _ => Missing::Value(SortValue::Long(rng.gen_range(-60..60))),
                };
                let tag_max: bool = rng.r#gen();
                let tag_rev: bool = rng.r#gen();
                let keys = vec![
                    SortKey::Field(FieldSort { field: NUM, kind: SortType::Long, reverse, missing: missing.clone(), mode, nested: None }),
                    SortKey::Field(FieldSort {
                        field: TAG,
                        kind: SortType::Keyword,
                        reverse: tag_rev,
                        missing: Missing::Last,
                        mode: if tag_max { SortMode::Max } else { SortMode::Min },
                        nested: None,
                    }),
                    SortKey::Score { reverse: false },
                ];
                let q = random_q(&mut rng, 2);
                let query = q.build();
                // Expected order.
                let mut exp: Vec<(u32, Option<i64>, Option<String>, f32)> = Vec::new();
                let rewritten = s.rewrite(&query).unwrap();
                for (id, d) in &corpus.live {
                    if let Some(sc) = eval_query(&s, &rewritten, d, true, 1.0) {
                        let nk = num_key(d, mode).or(match &missing {
                            Missing::Value(SortValue::Long(v)) => Some(*v),
                            _ => None,
                        });
                        exp.push((by_id[id], nk, tag_key(d, tag_max), sc));
                    }
                }
                let cmp_opt = |a: &Option<i64>, b: &Option<i64>, rev: bool, first: bool| -> Ordering {
                    match (a, b) {
                        (None, None) => Ordering::Equal,
                        (None, _) => if first { Ordering::Less } else { Ordering::Greater },
                        (_, None) => if first { Ordering::Greater } else { Ordering::Less },
                        (Some(x), Some(y)) => if rev { y.cmp(x) } else { x.cmp(y) },
                    }
                };
                let first = matches!(missing, Missing::First);
                exp.sort_by(|a, b| {
                    cmp_opt(&a.1, &b.1, reverse, first)
                        .then_with(|| match (&a.2, &b.2) {
                            (None, None) => Ordering::Equal,
                            (None, _) => Ordering::Greater,
                            (_, None) => Ordering::Less,
                            (Some(x), Some(y)) => if tag_rev { y.cmp(x) } else { x.cmp(y) },
                        })
                        .then(b.3.total_cmp(&a.3))
                        .then(a.0.cmp(&b.0))
                });
                for k in [1usize, 7, 1000] {
                    let mut c = TopFieldCollector::new(keys.clone(), k, u64::MAX, false).with_searcher(&s);
                    s.search(&query, &mut c).unwrap();
                    let got = c.top_docs();
                    assert_eq!(got.total_hits as usize, exp.len());
                    let got_docs: Vec<u32> = got.hits.iter().map(|h| h.doc).collect();
                    let want: Vec<u32> = exp.iter().take(k).map(|e| e.0).collect();
                    // Scores may differ by ulps from the model; compare by doc where the
                    // keys before the score decide.
                    if got_docs != want {
                        for (i, (g, w)) in got_docs.iter().zip(&want).enumerate() {
                            if g != w {
                                let ge = exp.iter().find(|e| e.0 == *g).unwrap();
                                let we = exp.iter().find(|e| e.0 == *w).unwrap();
                                assert!(
                                    ge.1 == we.1 && ge.2 == we.2 && (ge.3 - we.3).abs() <= 1e-5 * ge.3.abs().max(1.0),
                                    "rank {i}: got {} {:?} want {} {:?} (mode {mode:?} rev {reverse} missing {missing:?})",
                                    ids[g], ge, ids[w], we
                                );
                            }
                        }
                    }
                    // Values are reported.
                    for h in &got.hits {
                        let e = exp.iter().find(|e| e.0 == h.doc).unwrap();
                        match (&h.values[0], e.1) {
                            (SortValue::Long(v), Some(x)) => assert_eq!(*v, x),
                            // Elasticsearch reports the substituted extreme for missing numbers.
                            (SortValue::Long(v), None) => {
                                let min = matches!(missing, Missing::First) ^ reverse;
                                assert_eq!(*v, if min { i64::MIN } else { i64::MAX });
                            }
                            (v, x) => panic!("sort value {v:?} vs {x:?}"),
                        }
                    }
                    // Paging with search_after matches the next slice.
                    if k == 7 && got.hits.len() == 7 {
                        let last = got.hits.last().unwrap();
                        let mut c2 = TopFieldCollector::new(keys.clone(), 7, u64::MAX, false)
                            .with_searcher(&s)
                            .with_after(last.values.clone(), Some(last.doc));
                        s.search(&query, &mut c2).unwrap();
                        let page2: Vec<u32> = c2.top_docs().hits.iter().map(|h| h.doc).collect();
                        let mut c3 = TopFieldCollector::new(keys.clone(), 14, u64::MAX, false).with_searcher(&s);
                        s.search(&query, &mut c3).unwrap();
                        let all: Vec<u32> = c3.top_docs().hits.iter().map(|h| h.doc).collect();
                        assert_eq!(page2, all[7.min(all.len())..].to_vec());
                    }
                }
            }
        }
    }
}

/// Numeric sort pruning (points-driven skipping once the queue is full) returns exactly the
/// hits of an exhaustive sort, for every direction, mode, missing order and `search_after`.
#[test]
fn pruned_numeric_sorts_equal_exhaustive() {
    use search_codec::{DocValuesType, PointsSpec};
    use search_index::collector::{FieldSort, Missing, SortKey, SortMode, SortType, SortValue, TopFieldCollector};
    use search_index::doc::{DocValue, IndexableDoc};
    use search_index::query::{MatchAllQuery, TermQuery};
    use search_index::schema::FieldSchema;
    use search_index::{DeleteOp, IndexOp, QueryRef, Shard, ShardConfig};
    const N: u32 = 40;
    const D: u32 = 41;
    const W: u32 = 42;
    let schema = search_index::Schema::new()
        .with_field(FieldSchema::new(N, "n").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
        .with_field(FieldSchema::new(D, "d").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
        .with_field(FieldSchema::new(W, "w").indexed(search_codec::IndexOptions::Docs, false))
        .unwrap();
    let mut rng = common::rng(17);
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema, Vec::new()).unwrap();
    for i in 0..20_000u32 {
        let mut doc = IndexableDoc::new(format!("d{i}").into_bytes()).term(W, if i % 3 == 0 { b"a".to_vec() } else { b"b".to_vec() });
        // Mostly increasing values (time series) with noise, some docs missing, some multi-valued.
        if i % 17 != 5 {
            let base = i as i64 * 10 + rng.gen_range(-500..500);
            doc = doc.point(N, search_codec::points::encode::i64_bytes(base).to_vec()).doc_value(N, DocValue::SortedNumeric(base));
            if i % 11 == 0 {
                let v = base + rng.gen_range(-3000..3000);
                doc = doc.point(N, search_codec::points::encode::i64_bytes(v).to_vec()).doc_value(N, DocValue::SortedNumeric(v));
            }
        }
        let f: f64 = rng.gen_range(-1e6..1e6);
        let enc = search_codec::points::encode::f64_sortable(f);
        doc = doc.point(D, search_codec::points::encode::i64_bytes(enc).to_vec()).doc_value(D, DocValue::SortedNumeric(enc));
        s.index(IndexOp::new(doc)).unwrap();
        if i % 5000 == 4999 {
            s.refresh().unwrap();
        }
        if i % 13 == 7 {
            s.delete(DeleteOp::new(format!("d{}", rng.gen_range(0..=i)).into_bytes())).unwrap();
        }
    }
    s.refresh().unwrap();
    let se = s.searcher();
    let queries: Vec<QueryRef> = vec![Arc::new(MatchAllQuery), Arc::new(TermQuery::new(W, "a"))];
    let mut pruned_any = false;
    for q in &queries {
        for (field, kind) in [(N, SortType::Long), (D, SortType::Double)] {
            for reverse in [false, true] {
                for mode in [SortMode::Min, SortMode::Max] {
                    for missing in [Missing::Last, Missing::First] {
                        let key = SortKey::Field(FieldSort { field, kind, reverse, missing: missing.clone(), mode, nested: None });
                        let full = {
                            let mut c = TopFieldCollector::new(vec![key.clone()], 25, u64::MAX, false).with_searcher(&se);
                            se.search(q, &mut c).unwrap();
                            c.top_docs()
                        };
                        let mut c = TopFieldCollector::new(vec![key.clone()], 25, 100, false).with_searcher(&se);
                        se.search(q, &mut c).unwrap();
                        let got = c.top_docs();
                        let a: Vec<(u32, Vec<SortValue>)> = full.hits.iter().map(|h| (h.doc, h.values.clone())).collect();
                        let b: Vec<(u32, Vec<SortValue>)> = got.hits.iter().map(|h| (h.doc, h.values.clone())).collect();
                        assert_eq!(b, a, "{field} {kind:?} rev={reverse} {mode:?} {missing:?}");
                        pruned_any |= got.total_hits < full.total_hits / 4;
                        // search_after from the 10th hit.
                        let after = full.hits[9].values.clone();
                        let mut c1 = TopFieldCollector::new(vec![key.clone()], 10, u64::MAX, false).with_searcher(&se).with_after(after.clone(), Some(full.hits[9].doc));
                        se.search(q, &mut c1).unwrap();
                        let mut c2 = TopFieldCollector::new(vec![key], 10, 50, false).with_searcher(&se).with_after(after, Some(full.hits[9].doc));
                        se.search(q, &mut c2).unwrap();
                        let x: Vec<u32> = c1.top_docs().hits.iter().map(|h| h.doc).collect();
                        let y: Vec<u32> = c2.top_docs().hits.iter().map(|h| h.doc).collect();
                        assert_eq!(y, x, "after: {field} rev={reverse} {mode:?} {missing:?}");
                    }
                }
            }
        }
    }
    assert!(pruned_any, "pruning never kicked in");
}
