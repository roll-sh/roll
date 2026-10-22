//! `more_like_this`: picks the most characteristic terms of "like" texts and documents and
//! rewrites to a disjunction of them (Elasticsearch's `XMoreLikeThis`).
//!
//! Terms are counted per source (like texts: tokens analyzed with the first field's analyzer;
//! like documents: their term vectors, field by field), filtered by `min_term_freq`, word
//! length, stop words, skip (unlike) terms and doc frequency bounds, and scored `tf · idf`
//! with the classic idf `1 + ln((numDocs + 1) / (docFreq + 1))`. The best `max_query_terms`
//! become `SHOULD` term clauses, optionally boosted relative to the lowest kept score
//! (`boost_terms`), and `minimum_should_match` applies to each part.
//!
//! The order of the clauses (it shows in explanations and fixes the float summation order)
//! is Elasticsearch's: words are visited in Java `HashMap<String, _>` iteration order and
//! leave a Lucene `PriorityQueue` lowest score first; both are emulated here.

use std::any::Any;
use std::collections::HashSet;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::query::basic::{BoostQuery, MatchNoneQuery};
use crate::query::boolean::{BooleanQuery, Occur};
use crate::query::term::{TermQuery, TermStates};
use crate::schema::Schema;
use crate::search::{Query, QueryRef, QueryVisitor, ScoreMode, Searcher, Weight};

/// Elasticsearch's `minimum_should_match` arithmetic (`Queries.calculateMinShouldMatch`):
/// integers, negative integers, percentages, negative percentages and conditional specs
/// (`3<90%`, `2<-25% 9<-3`). The result is not capped at `optional`.
pub fn calculate_min_should_match(optional: usize, spec: &str) -> Result<i64> {
    let count = optional as i64;
    let spec = spec.trim();
    if spec.contains('<') {
        let mut result = count;
        let compact = squeeze_less_than(spec);
        for part in compact.split_whitespace() {
            let (bound, rest) = part.split_once('<').ok_or_else(|| number_error(part))?;
            let upper: i64 = bound.parse().map_err(|_| number_error(bound))?;
            if count <= upper {
                return Ok(result);
            }
            result = calculate_min_should_match(optional, rest)?;
        }
        return Ok(result);
    }
    let result = if let Some(p) = spec.strip_suffix('%') {
        let percent: i32 = p.parse().map_err(|_| number_error(p))?;
        let calc = ((count as i32).wrapping_mul(percent)) as f32 * (1.0f32 / 100.0);
        if calc < 0.0 { count + calc as i64 } else { calc as i64 }
    } else {
        let calc: i64 = spec.parse::<i32>().map_err(|_| number_error(spec))? as i64;
        if calc < 0 { count + calc } else { calc }
    };
    Ok(result.max(0))
}

fn squeeze_less_than(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            // Drop whitespace next to '<'.
            let mut j = i;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            let next_lt = j < chars.len() && chars[j] == '<';
            let prev_lt = out.ends_with('<');
            if !next_lt && !prev_lt {
                out.push(' ');
            }
            i = j;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn number_error(s: &str) -> Error {
    Error::es("number_format_exception", format!("For input string: \"{s}\""), 400)
}

/// Tokens of one like text considered (`XMoreLikeThis.DEFAULT_MAX_NUM_TOKENS_PARSED`).
pub const MAX_NUM_TOKENS_PARSED: usize = 5000;

/// Java's `String.hashCode` of UTF-8 bytes (decoded lossily, hashed over UTF-16 units).
pub fn java_string_hash(b: &[u8]) -> i32 {
    String::from_utf8_lossy(b).encode_utf16().fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(c as i32))
}

/// Indices of `keys` (distinct, in insertion order) in Java `HashMap` iteration order.
pub fn java_hashmap_order(keys: &[&[u8]]) -> Vec<usize> {
    let mut cap = 16usize;
    while keys.len() > cap * 3 / 4 {
        cap *= 2;
    }
    let mut idx: Vec<(usize, usize)> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| {
            let h = java_string_hash(k);
            let spread = (h ^ ((h as u32) >> 16) as i32) as u32 as usize;
            (spread & (cap - 1), i)
        })
        .collect();
    idx.sort();
    idx.into_iter().map(|(_, i)| i).collect()
}

/// Options of a `more_like_this` query (Elasticsearch defaults).
#[derive(Clone, Debug)]
pub struct MoreLikeThis {
    /// Fields in request order (doc frequencies are looked up in all of them; the field with
    /// the largest one wins a like-text term).
    pub fields: Vec<u32>,
    /// `min_term_freq` (2).
    pub min_term_freq: u32,
    /// `max_query_terms` (25).
    pub max_query_terms: usize,
    /// `min_doc_freq` (5).
    pub min_doc_freq: u64,
    /// `max_doc_freq` (unbounded).
    pub max_doc_freq: u64,
    /// `min_word_length` (0: off).
    pub min_word_length: usize,
    /// `max_word_length` (0: off).
    pub max_word_length: usize,
    pub stop_words: HashSet<Vec<u8>>,
    /// `boost_terms` (0: off).
    pub boost_terms: f32,
    /// `minimum_should_match` ("30%").
    pub minimum_should_match: Option<String>,
}

impl Default for MoreLikeThis {
    fn default() -> Self {
        MoreLikeThis {
            fields: Vec::new(),
            min_term_freq: 2,
            max_query_terms: 25,
            min_doc_freq: 5,
            max_doc_freq: i32::MAX as u64,
            min_word_length: 0,
            max_word_length: 0,
            stop_words: HashSet::new(),
            boost_terms: 0.0,
            minimum_should_match: Some("30%".into()),
        }
    }
}

/// A like document: per field, its terms with frequencies (term vectors, in term order).
pub type LikeDoc = Vec<(u32, Vec<(Vec<u8>, u32)>)>;

/// The query; rewrites to the term disjunction.
#[derive(Clone, Debug, Default)]
pub struct MoreLikeThisQuery {
    pub mlt: MoreLikeThis,
    /// Like texts, each as its analyzed terms in token order (first field's analyzer).
    pub like_texts: Vec<Vec<Vec<u8>>>,
    /// Like documents (stored docs or artificial ones), as term vectors.
    pub like_docs: Vec<LikeDoc>,
    /// Unlike terms: never selected.
    pub skip: HashSet<(u32, Vec<u8>)>,
    /// Elasticsearch's description of the original query (`like:[…]`, shown when nothing
    /// was selected).
    pub description: String,
}

struct ScoreTerm {
    word: Vec<u8>,
    field: u32,
    score: f32,
}

/// Lucene's `PriorityQueue` (1-based binary heap, `lessThan = a.score < b.score`).
struct FreqQ {
    heap: Vec<Option<ScoreTerm>>,
    size: usize,
}

impl FreqQ {
    fn new(max: usize) -> FreqQ {
        let mut heap = Vec::with_capacity(max + 2);
        heap.resize_with(max + 2, || None);
        FreqQ { heap, size: 0 }
    }
    fn less(a: &ScoreTerm, b: &ScoreTerm) -> bool {
        a.score < b.score
    }
    fn add(&mut self, t: ScoreTerm) {
        self.size += 1;
        if self.heap.len() <= self.size {
            self.heap.resize_with(self.size + 1, || None);
        }
        self.heap[self.size] = Some(t);
        self.up_heap(self.size);
    }
    fn top(&self) -> Option<&ScoreTerm> {
        self.heap.get(1).and_then(|x| x.as_ref())
    }
    fn top_mut(&mut self) -> Option<&mut ScoreTerm> {
        self.heap.get_mut(1).and_then(|x| x.as_mut())
    }
    fn pop(&mut self) -> Option<ScoreTerm> {
        if self.size == 0 {
            return None;
        }
        let r = self.heap[1].take();
        self.heap.swap(1, self.size);
        self.size -= 1;
        self.down_heap(1);
        r
    }
    fn up_heap(&mut self, orig: usize) {
        let mut i = orig;
        let node = self.heap[i].take();
        let mut j = i >> 1;
        while j > 0 && Self::less(node.as_ref().unwrap(), self.heap[j].as_ref().unwrap()) {
            self.heap[i] = self.heap[j].take();
            i = j;
            j >>= 1;
        }
        self.heap[i] = node;
    }
    fn down_heap(&mut self, mut i: usize) {
        if self.size == 0 {
            return;
        }
        let node = self.heap[i].take();
        let mut j = i << 1;
        let mut k = j + 1;
        if k <= self.size && Self::less(self.heap[k].as_ref().unwrap(), self.heap[j].as_ref().unwrap()) {
            j = k;
        }
        while j <= self.size && Self::less(self.heap[j].as_ref().unwrap(), node.as_ref().unwrap()) {
            self.heap[i] = self.heap[j].take();
            i = j;
            j = i << 1;
            k = j + 1;
            if k <= self.size && Self::less(self.heap[k].as_ref().unwrap(), self.heap[j].as_ref().unwrap()) {
                j = k;
            }
        }
        self.heap[i] = node;
    }
}

impl MoreLikeThisQuery {
    fn is_noise(&self, w: &[u8]) -> bool {
        let len = String::from_utf8_lossy(w).encode_utf16().count();
        let m = &self.mlt;
        (m.min_word_length > 0 && len < m.min_word_length)
            || (m.max_word_length > 0 && len > m.max_word_length)
            || m.stop_words.contains(w)
    }

    /// Selects the terms of one word → tf map (`fields`: where doc freqs are looked up).
    fn queue(&self, s: &Searcher, words: &[(Vec<u8>, u32)], fields: &[u32]) -> FreqQ {
        let num_docs = s.num_docs() as i64;
        let limit = self.mlt.max_query_terms.min(words.len());
        let mut q = FreqQ::new(limit);
        let keys: Vec<&[u8]> = words.iter().map(|(w, _)| w.as_slice()).collect();
        for i in java_hashmap_order(&keys) {
            let (word, tf) = (&words[i].0, words[i].1);
            if self.mlt.min_term_freq > 0 && tf < self.mlt.min_term_freq {
                continue;
            }
            let mut top_field = fields[0];
            let mut doc_freq = 0u64;
            for &f in fields {
                let df = TermStates::build(s, f, word).doc_freq;
                if df > doc_freq {
                    top_field = f;
                    doc_freq = df;
                }
            }
            if self.mlt.min_doc_freq > 0 && doc_freq < self.mlt.min_doc_freq {
                continue;
            }
            if doc_freq > self.mlt.max_doc_freq || doc_freq == 0 {
                continue;
            }
            let idf = ((((num_docs + 1) as f64) / ((doc_freq + 1) as f64)).ln() + 1.0) as f32;
            let score = tf as f32 * idf;
            if q.size < limit {
                q.add(ScoreTerm { word: word.clone(), field: top_field, score });
            } else if let Some(t) = q.top()
                && t.score < score
            {
                let t = q.top_mut().unwrap();
                *t = ScoreTerm { word: word.clone(), field: top_field, score };
                q.down_heap(1);
            }
        }
        q
    }

    /// Pops the queue into `SHOULD` term clauses.
    fn add_clauses(&self, mut q: FreqQ, b: &mut BooleanQuery) {
        let mut best = -1f32;
        while let Some(t) = q.pop() {
            let tq: QueryRef = Arc::new(TermQuery::new(t.field, t.word));
            let clause: QueryRef = if self.mlt.boost_terms != 0.0 {
                if best == -1.0 {
                    best = t.score;
                }
                Arc::new(BoostQuery::new(tq, self.mlt.boost_terms * t.score / best))
            } else {
                tq
            };
            b.clauses.push(crate::query::boolean::BooleanClause { occur: Occur::Should, query: clause });
        }
    }

    fn with_msm(&self, mut b: BooleanQuery) -> Result<BooleanQuery> {
        if let Some(spec) = &self.mlt.minimum_should_match {
            let optional = b.clauses.iter().filter(|c| c.occur == Occur::Should).count();
            let m = calculate_min_should_match(optional, spec)?;
            if m > 0 {
                b = b.with_msm(m as usize);
            }
        }
        Ok(b)
    }

    /// The term disjunction (`None`: no term was selected).
    pub fn build(&self, s: &Searcher) -> Result<Option<BooleanQuery>> {
        if self.mlt.fields.is_empty() {
            return Err(Error::illegal("more_like_this requires 'fields' to be non-empty"));
        }
        let mut outer = BooleanQuery::new();
        if !self.like_docs.is_empty() {
            // Field by field (Java HashSet order of the names), all docs' vectors summed.
            let mut fields: Vec<u32> = Vec::new();
            for d in &self.like_docs {
                for (f, _) in d {
                    if !fields.contains(f) {
                        fields.push(*f);
                    }
                }
            }
            let names: Vec<String> = fields.iter().map(|&f| s.schema().name(f)).collect();
            let name_bytes: Vec<&[u8]> = names.iter().map(|n| n.as_bytes()).collect();
            let mut part = BooleanQuery::new();
            for i in java_hashmap_order(&name_bytes) {
                let f = fields[i];
                let mut words: Vec<(Vec<u8>, u32)> = Vec::new();
                for d in &self.like_docs {
                    for (df, terms) in d {
                        if *df != f {
                            continue;
                        }
                        for (t, freq) in terms {
                            if self.is_noise(t) || self.skip.contains(&(f, t.clone())) {
                                continue;
                            }
                            match words.iter_mut().find(|(w, _)| w == t) {
                                Some(e) => e.1 += freq,
                                None => words.push((t.clone(), *freq)),
                            }
                        }
                    }
                }
                let q = self.queue(s, &words, &[f]);
                self.add_clauses(q, &mut part);
            }
            let part = self.with_msm(part)?;
            outer = outer.should(Arc::new(part));
        }
        if !self.like_texts.is_empty() {
            let first = self.mlt.fields[0];
            let mut words: Vec<(Vec<u8>, u32)> = Vec::new();
            for text in &self.like_texts {
                // Lucene's `maxNumTokensParsed` (5000) per text.
                for t in text.iter().take(MAX_NUM_TOKENS_PARSED) {
                    if self.is_noise(t) || self.skip.contains(&(first, t.clone())) {
                        continue;
                    }
                    match words.iter_mut().find(|(w, _)| w == t) {
                        Some(e) => e.1 += 1,
                        None => words.push((t.clone(), 1)),
                    }
                }
            }
            let q = self.queue(s, &words, &self.mlt.fields);
            let mut part = BooleanQuery::new();
            self.add_clauses(q, &mut part);
            let part = self.with_msm(part)?;
            outer = outer.should(Arc::new(part));
        }
        let any = outer.clauses.iter().any(|c| c.query.as_any().downcast_ref::<BooleanQuery>().is_some_and(|b| !b.clauses.is_empty()));
        Ok(any.then_some(outer))
    }
}

impl Query for MoreLikeThisQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let q = s.rewrite(&(Arc::new(self.clone()) as QueryRef))?;
        q.weight(s, mode, boost)
    }

    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        Ok(Some(match self.build(s)? {
            Some(b) => Arc::new(b),
            None => Arc::new(MatchNoneQuery::new("no terms selected by more_like_this")),
        }))
    }

    fn lucene_string(&self, _schema: &Schema) -> String {
        if !self.description.is_empty() {
            return self.description.clone();
        }
        let texts: Vec<String> =
            self.like_texts.iter().map(|t| t.iter().map(|w| String::from_utf8_lossy(w).into_owned()).collect::<Vec<_>>().join(" ")).collect();
        format!("like:[{}]", texts.join(", "))
    }

    fn visit(&self, _v: &mut dyn QueryVisitor) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimum_should_match_like_elasticsearch() {
        let c = |s: &str| calculate_min_should_match(5, s).unwrap();
        assert_eq!(c("10"), 10);
        assert_eq!(c("-1"), 4);
        assert_eq!(c("30%"), 1);
        assert_eq!(c("-30%"), 4);
        assert_eq!(c("2<-25% 9<-3"), 4);
        assert_eq!(c("2 < -25%  9 <-3"), 4);
        assert_eq!(c("3<90%"), 4);
        assert_eq!(c("5<1"), 5);
        assert_eq!(c("150%"), 7);
        assert!(calculate_min_should_match(5, "x").is_err());
    }

    #[test]
    fn java_hash_order() {
        assert_eq!(java_string_hash(b"t"), 116);
        assert_eq!(java_string_hash(b"apple"), 93029210);
        // "t" (116 & 15 = 4) before "u" (5), "a" (97 & 15 = 1) first.
        assert_eq!(java_hashmap_order(&[b"u", b"t", b"a"]), vec![2, 1, 0]);
    }
}
