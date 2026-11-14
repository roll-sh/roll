//! The `phrase` suggester (Elasticsearch's noisy-channel spell checker): whole-phrase
//! corrections scored by an n-gram language model over a shingle field.
//!
//! The caller analyzes the text (and owns mappings: which field holds shingles, its gram size,
//! pre/post filter analyzers) and hands over the tokens. Each unigram token becomes a
//! candidate set: the input term, terms stacked on it as synonyms, and the spelling
//! candidates of every direct generator ([`Searcher::suggest_similar`] on the generator's
//! field, with Elasticsearch's frequency threshold), scored `channel · (ttf + 1) / (Σttf + 1)`
//! and capped at `shard_size` per set. Corrections are the paths through the sets with at most
//! `max_errors` replaced terms; a path scores `exp(Σ log10(channel · P(word | history)))` —
//! Elasticsearch's mix of bases, kept for identical numbers — with stupid backoff, Laplace or
//! linear interpolation smoothing over the suggest field's term frequencies, and must beat
//! `confidence ·` the input's own score. The best `shard_size` corrections come back, best
//! first.

use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::search::Searcher;
use crate::spell::{SpellOptions, SuggestMode};

/// One analyzed token of the text.
#[derive(Clone, Debug)]
pub struct PhraseToken {
    pub term: Vec<u8>,
    pub position_increment: u32,
    /// A shingle (`shingle` token type): skipped when unigrams are required.
    pub shingle: bool,
    /// A synonym stacked on the previous token (`SYNONYM` type with increment 0).
    pub synonym: bool,
}

/// Rewrites a term through an analyzer (a generator's `pre_filter` / `post_filter`): the
/// output tokens with their position increments.
pub type TermFilter = dyn Fn(&[u8]) -> Vec<(Vec<u8>, u32)> + Send + Sync;

/// A `direct_generator`.
pub struct DirectGenerator {
    pub field: u32,
    pub spell: SpellOptions,
    /// Candidates per token (`size`, default 5).
    pub size: usize,
    pub pre_filter: Option<Box<TermFilter>>,
    pub post_filter: Option<Box<TermFilter>>,
}

/// The language model (`smoothing`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Smoothing {
    /// `stupid_backoff` (default discount 0.4; the implicit default model uses `0.4f`).
    StupidBackoff { discount: f64 },
    Laplace { alpha: f64 },
    LinearInterpolation { trigram: f64, bigram: f64, unigram: f64 },
}

/// Phrase suggester settings (Elasticsearch's defaults where it has them).
pub struct PhraseSuggestOptions {
    /// The field whose (shingle) term frequencies score phrases.
    pub field: u32,
    pub real_word_error_likelihood: f32,
    pub confidence: f32,
    /// Below 1: a fraction of the tokens; from 1: a count.
    pub max_errors: f32,
    pub separator: Vec<u8>,
    pub shard_size: usize,
    pub gram_size: usize,
    pub require_unigram: bool,
    pub token_limit: usize,
    pub smoothing: Smoothing,
    pub generators: Vec<DirectGenerator>,
}

/// One correction: the phrase's terms (and which were kept from the input), with its score.
#[derive(Clone, Debug, PartialEq)]
pub struct Correction {
    pub terms: Vec<Vec<u8>>,
    pub user_input: Vec<bool>,
    pub score: f64,
}

impl Correction {
    /// The phrase, terms joined by `separator`; corrected runs wrapped in `pre`/`post` tags.
    pub fn join(&self, separator: &[u8], tags: Option<(&[u8], &[u8])>) -> Vec<u8> {
        let mut out = Vec::new();
        let n = self.terms.len();
        for i in 0..n {
            if i > 0 {
                out.extend_from_slice(separator);
            }
            match tags {
                Some((pre, post)) if !self.user_input[i] => {
                    if i == 0 || self.user_input[i - 1] {
                        out.extend_from_slice(pre);
                    }
                    out.extend_from_slice(&self.terms[i]);
                    if i + 1 == n || self.user_input[i + 1] {
                        out.extend_from_slice(post);
                    }
                }
                _ => out.extend_from_slice(&self.terms[i]),
            }
        }
        out
    }
}

/// Corrections (best first) and the cutoff they had to beat (`Double.MIN_VALUE` without a
/// confidence).
#[derive(Clone, Debug, PartialEq)]
pub struct PhraseSuggestion {
    pub corrections: Vec<Correction>,
    pub cutoff_score: f64,
}

/// Term statistics of one field.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Stats {
    df: u64,
    ttf: u64,
}

#[derive(Clone, Debug)]
struct Candidate {
    term: Vec<u8>,
    stats: Stats,
    string_distance: f64,
    score: f64,
    user_input: bool,
}

struct CandidateSet {
    original: Candidate,
    candidates: Vec<Candidate>,
}

/// `Math.min(10_000_000, …)` paths, as Elasticsearch caps the search.
const MAX_SCORED_PATHS: u64 = 10_000_000;

impl Searcher {
    fn field_stats(&self, field: u32, term: &[u8]) -> Stats {
        let t = self.term_stats(field, term);
        Stats { df: t.doc_freq, ttf: t.total_term_freq }
    }

    fn sum_ttf(&self, field: u32) -> u64 {
        match self.collection_stats(field) {
            Some(c) => c.sum_total_term_freq,
            None => self.max_doc() as u64,
        }
    }

    /// Phrase corrections for analyzed `tokens` (Elasticsearch's `PhraseSuggester` up to the
    /// options; collation and trimming to `size` are the caller's).
    pub fn phrase_suggest(&self, tokens: &[PhraseToken], opts: &PhraseSuggestOptions) -> Result<PhraseSuggestion> {
        let empty = PhraseSuggestion { corrections: Vec::new(), cutoff_score: f64::from_bits(1) };
        let has_terms = |f: u32| self.segments().iter().any(|seg| seg.reader.terms(f).is_some());
        // Generators over fields without terms are dropped; no generator or no suggest field
        // terms: nothing to suggest.
        let gens: Vec<&DirectGenerator> = opts.generators.iter().filter(|g| has_terms(g.field)).collect();
        let Some(&g0) = gens.first() else { return Ok(empty) };
        if !has_terms(opts.field) {
            return Ok(empty);
        }
        let rwl = opts.real_word_error_likelihood as f64;
        let dict0 = self.sum_ttf(g0.field);
        let candidate = |term: Vec<u8>, stats: Stats, distance: f64, dict: u64, user: bool| Candidate {
            score: distance * ((stats.ttf as f64 + 1.0) / (dict as f64 + 1.0)),
            term,
            stats,
            string_distance: distance,
            user_input: user,
        };
        // The generator wrapper's term stats: the first generator's field, through its pre-filter.
        let stats0 = |term: &[u8]| -> Stats {
            let t = match &g0.pre_filter {
                Some(f) => f(term).pop().map(|x| x.0).unwrap_or_default(),
                None => term.to_vec(),
            };
            self.field_stats(g0.field, &t)
        };
        let mut sets: Vec<CandidateSet> = Vec::new();
        let (mut any_unigram, mut any_tokens) = (false, false);
        for t in tokens {
            any_tokens = true;
            if opts.require_unigram && t.shingle {
                continue;
            }
            any_unigram = true;
            if t.position_increment == 0 && t.synonym {
                if let Some(cur) = sets.last_mut() {
                    let st = stats0(&t.term);
                    if st.df > 0 {
                        cur.candidates.push(candidate(t.term.clone(), st, rwl, dict0, false));
                    }
                }
            } else {
                let st = stats0(&t.term);
                sets.push(CandidateSet { original: candidate(t.term.clone(), st, 1.0, dict0, true), candidates: Vec::new() });
            }
        }
        if opts.require_unigram && !any_unigram && any_tokens {
            return Err(Error::illegal("At least one unigram is required but all tokens were ngrams"));
        }
        if sets.is_empty() || sets.len() >= opts.token_limit.min(1000) {
            return Ok(empty);
        }
        for set in sets.iter_mut() {
            for g in &gens {
                self.draw_candidates(g, set, rwl)?;
            }
            if set.candidates.len() > opts.shard_size {
                // Stable: equal scores keep their (term) order.
                set.candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
                set.candidates.truncate(opts.shard_size);
            }
        }
        let scorer = WordScorer::new(self, opts, rwl);
        let mut cutoff = f64::from_bits(1);
        if opts.confidence > 0.0 {
            let path: Vec<&Candidate> = sets.iter().map(|s| &s.original).collect();
            let mut total = 0.0;
            for i in 0..path.len() {
                total += scorer.score(&path, i, opts.gram_size);
            }
            cutoff = total.exp() * opts.confidence as f64;
        }
        let corrections = find_best(&sets, &scorer, opts, cutoff)?;
        Ok(PhraseSuggestion { corrections, cutoff_score: cutoff })
    }

    /// `DirectCandidateGenerator.drawCandidates`.
    fn draw_candidates(&self, g: &DirectGenerator, set: &mut CandidateSet, rwl: f64) -> Result<()> {
        let original = &set.original;
        let term = match &g.pre_filter {
            Some(f) => f(&original.term).pop().map(|x| x.0).unwrap_or_default(),
            None => original.term.clone(),
        };
        let mut spell = g.spell.clone();
        if spell.suggest_mode != SuggestMode::Always {
            let threshold = threshold_term_frequency(self, &g.spell, original.stats.df);
            if threshold == i32::MAX as i64 {
                return Ok(());
            }
            // A larger min_doc_freq wins.
            if spell.min_doc_freq < threshold as f32 {
                spell.min_doc_freq = threshold as f32;
            }
        }
        let Ok(text) = std::str::from_utf8(&term) else { return Ok(()) };
        let dict = self.sum_ttf(g.field);
        let words = self.suggest_similar(g.field, text, g.size, &spell)?;
        let mut fresh: Vec<Candidate> = Vec::with_capacity(words.len());
        for w in words {
            let cterm = w.term.into_bytes();
            let stats = self.field_stats(g.field, &cterm);
            let distance = w.score as f64;
            match &g.post_filter {
                None => fresh.push(Candidate {
                    score: distance * ((stats.ttf as f64 + 1.0) / (dict as f64 + 1.0)),
                    term: cterm,
                    stats,
                    string_distance: distance,
                    user_input: false,
                }),
                Some(f) => {
                    let score = distance * ((stats.ttf as f64 + 1.0) / (dict as f64 + 1.0));
                    for (t, inc) in f(&cterm) {
                        if inc > 0 && t == cterm {
                            let st = self.field_stats(g.field, &t);
                            fresh.push(Candidate { term: t, stats: st, string_distance: distance, score, user_input: false });
                        } else {
                            fresh.push(Candidate { term: t, stats, string_distance: rwl, score, user_input: false });
                        }
                    }
                }
            }
        }
        // Merge, dedupe by term (fresh ones first, as a HashSet seeded with them keeps them),
        // then strongest first: score descending, equal scores by term ascending.
        let mut by_term: HashMap<Vec<u8>, Candidate> = HashMap::new();
        for c in fresh.into_iter().chain(std::mem::take(&mut set.candidates)) {
            by_term.entry(c.term.clone()).or_insert(c);
        }
        let mut all: Vec<Candidate> = by_term.into_values().collect();
        all.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.term.cmp(&b.term)));
        set.candidates = all;
        Ok(())
    }
}

/// `DirectCandidateGenerator.thresholdTermFrequency`.
fn threshold_term_frequency(s: &Searcher, spell: &SpellOptions, df: u64) -> i64 {
    if df == 0 {
        return 0;
    }
    let tf = spell.min_doc_freq;
    let plateau: i64 = if tf >= 1.0 { tf as i64 } else { (s.max_doc() as f32 * tf) as i64 };
    let df = df as f64;
    let v = df * ((df - plateau as f64).log10() * (1.0 / 5f64.log10())) + 1.0;
    // Java Math.round(double): floor(x + 0.5), NaN -> 0, saturating.
    let r = if v.is_nan() { 0 } else { (v + 0.5).floor().clamp(i64::MIN as f64, i64::MAX as f64) as i64 };
    r.max(0).min(i32::MAX as i64)
}

/// The language model over the suggest field.
struct WordScorer<'a> {
    s: &'a Searcher,
    field: u32,
    vocabulary: u64,
    num_terms: u64,
    rwl: f64,
    separator: Vec<u8>,
    smoothing: Smoothing,
    cache: std::cell::RefCell<HashMap<Vec<u8>, u64>>,
}

impl<'a> WordScorer<'a> {
    fn new(s: &'a Searcher, opts: &PhraseSuggestOptions, rwl: f64) -> WordScorer<'a> {
        let vocabulary = s.sum_ttf(opts.field);
        // Lucene's `Terms.size()` is known for a single segment only (MultiTerms says -1, and
        // Elasticsearch then uses maxDoc).
        let num_terms = match s.segments() {
            [seg] => seg.reader.terms(opts.field).map_or(s.max_doc() as u64, |t| t.stats().num_terms),
            _ => s.max_doc() as u64,
        };
        WordScorer {
            s,
            field: opts.field,
            vocabulary,
            num_terms,
            rwl,
            separator: opts.separator.clone(),
            smoothing: opts.smoothing,
            cache: Default::default(),
        }
    }

    /// Total term frequency in the suggest field (cached per phrase).
    fn frequency(&self, term: &[u8]) -> u64 {
        if let Some(&f) = self.cache.borrow().get(term) {
            return f;
        }
        let f = self.s.term_stats(self.field, term).total_term_freq;
        self.cache.borrow_mut().insert(term.to_vec(), f);
        f
    }

    fn join(&self, parts: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, p) in parts.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(&self.separator);
            }
            out.extend_from_slice(p);
        }
        out
    }

    fn channel(&self, c: &Candidate) -> f64 {
        if c.string_distance == 1.0 { self.rwl } else { c.string_distance }
    }

    fn unigram(&self, w: &Candidate) -> f64 {
        match self.smoothing {
            Smoothing::Laplace { alpha } => (alpha + self.frequency(&w.term) as f64) / (self.vocabulary as f64 + alpha * self.num_terms as f64),
            _ => (1.0 + self.frequency(&w.term) as f64) / (self.vocabulary as f64 + self.num_terms as f64),
        }
    }

    fn bigram(&self, w: &Candidate, w1: &Candidate) -> f64 {
        let count = self.frequency(&self.join(&[&w1.term, &w.term]));
        match self.smoothing {
            Smoothing::StupidBackoff { discount } => {
                if count < 1 {
                    discount * self.unigram(w)
                } else {
                    count as f64 / (w1.stats.ttf as f64 + 0.00000000001)
                }
            }
            Smoothing::Laplace { alpha } => (alpha + count as f64) / (w1.stats.ttf as f64 + alpha * self.num_terms as f64),
            Smoothing::LinearInterpolation { .. } => {
                let (_, bl, ul) = self.lambdas();
                if count < 1 {
                    ul * self.unigram(w)
                } else {
                    bl * (count as f64 / (0.5 + w1.stats.ttf as f64)) + ul * self.unigram(w)
                }
            }
        }
    }

    fn trigram(&self, w: &Candidate, w1: &Candidate, w2: &Candidate) -> f64 {
        match self.smoothing {
            Smoothing::StupidBackoff { discount } => {
                let bigram = self.frequency(&self.join(&[&w1.term, &w.term]));
                if bigram < 1 {
                    return discount * self.unigram(w);
                }
                let trigram = self.frequency(&self.join(&[&w2.term, &w1.term, &w.term]));
                if trigram < 1 {
                    return discount * (bigram as f64 / (w1.stats.ttf as f64 + 0.00000000001));
                }
                trigram as f64 / (bigram as f64 + 0.00000000001)
            }
            Smoothing::Laplace { alpha } => {
                let trigram = self.frequency(&self.join(&[&w2.term, &w1.term, &w.term]));
                (alpha + trigram as f64) / (self.frequency(&self.join(&[&w1.term, &w.term])) as f64 + alpha * self.num_terms as f64)
            }
            Smoothing::LinearInterpolation { .. } => {
                // Elasticsearch joins these in reading order w, w-1, w-2.
                let count = self.frequency(&self.join(&[&w.term, &w1.term, &w2.term]));
                if count < 1 {
                    return self.bigram(w, w1);
                }
                let (tl, _, _) = self.lambdas();
                tl * (count as f64 / (1.0 + self.frequency(&self.join(&[&w.term, &w1.term])) as f64)) + self.bigram(w, w1)
            }
        }
    }

    /// Normalized lambdas `(trigram, bigram, unigram)`.
    fn lambdas(&self) -> (f64, f64, f64) {
        let Smoothing::LinearInterpolation { trigram, bigram, unigram } = self.smoothing else { return (0.0, 0.0, 1.0) };
        let sum = unigram + bigram + trigram;
        (trigram / sum, bigram / sum, unigram / sum)
    }

    /// `WordScorer.score`: log10 of channel times the n-gram probability at `at`.
    fn score(&self, path: &[&Candidate], at: usize, gram_size: usize) -> f64 {
        let w = path[at];
        let p = if at == 0 || gram_size == 1 {
            self.unigram(w)
        } else if at == 1 || gram_size == 2 {
            self.bigram(w, path[at - 1])
        } else {
            self.trigram(w, path[at - 1], path[at - 2])
        };
        (self.channel(w) * p).log10()
    }
}

/// Elasticsearch's `Correction.compareTo`: score, then (for equal scores) earlier terms are
/// stronger, then longer paths.
fn cmp_correction(a_score: f64, a: &[&Candidate], b_score: f64, b: &[&Candidate]) -> std::cmp::Ordering {
    if a_score == b_score {
        for (x, y) in a.iter().zip(b) {
            let c = x.term.cmp(&y.term);
            if c != std::cmp::Ordering::Equal {
                return c.reverse();
            }
        }
        a.len().cmp(&b.len())
    } else {
        a_score.total_cmp(&b_score)
    }
}

struct Search<'a, 'b> {
    sets: &'a [CandidateSet],
    scorer: &'a WordScorer<'b>,
    gram_size: usize,
    max: usize,
    cutoff: f64,
    /// (score, candidate index per position: usize::MAX = the original).
    queue: Vec<(f64, Vec<usize>)>,
    paths: u64,
}

impl Search<'_, '_> {
    fn cand(&self, pos: usize, idx: usize) -> &Candidate {
        if idx == usize::MAX { &self.sets[pos].original } else { &self.sets[pos].candidates[idx] }
    }

    fn refs(&self, path: &[usize]) -> Vec<&Candidate> {
        path.iter().enumerate().map(|(i, &c)| self.cand(i, c)).collect()
    }

    fn word(&self, path: &[usize], at: usize) -> f64 {
        let refs = self.refs(&path[..=at]);
        self.scorer.score(&refs, at, self.gram_size)
    }

    fn budget(&mut self) -> Result<()> {
        self.paths += 1;
        if self.paths > MAX_SCORED_PATHS {
            return Err(Error::illegal(format!(
                "phrase suggester query is too complex and exceeded maximum combinations [{MAX_SCORED_PATHS}]; reduce token_limit"
            )));
        }
        Ok(())
    }

    /// `CandidateScorer.findCandidates`.
    fn find(&mut self, path: &mut Vec<usize>, ord: usize, left: usize, path_score: f64) -> Result<()> {
        self.budget()?;
        let n = self.sets.len();
        let alts = self.sets[ord].candidates.len();
        path.truncate(ord);
        if ord == n - 1 {
            path.push(usize::MAX);
            let s = path_score + self.word(path, ord);
            self.update(path, s);
            if left > 0 {
                for i in 0..alts {
                    self.paths += 1;
                    if self.paths > MAX_SCORED_PATHS {
                        return self.budget();
                    }
                    path[ord] = i;
                    let s = path_score + self.word(path, ord);
                    self.update(path, s);
                }
            }
        } else if left > 0 {
            path.push(usize::MAX);
            let s = path_score + self.word(path, ord);
            self.find(path, ord + 1, left, s)?;
            for i in 0..alts {
                path.truncate(ord);
                path.push(i);
                let s = path_score + self.word(path, ord);
                self.find(path, ord + 1, left - 1, s)?;
            }
        } else {
            path.push(usize::MAX);
            let s = path_score + self.word(path, ord);
            self.find(path, ord + 1, 0, s)?;
        }
        Ok(())
    }

    /// `CandidateScorer.updateTop`: a bounded min-queue of corrections.
    fn update(&mut self, path: &[usize], log_score: f64) {
        let score = log_score.exp();
        if score <= self.cutoff {
            return;
        }
        if self.queue.len() < self.max {
            self.queue.push((score, path.to_vec()));
            return;
        }
        // The weakest queued correction.
        let mut w = 0;
        for i in 1..self.queue.len() {
            let (a, b) = (self.refs(&self.queue[i].1), self.refs(&self.queue[w].1));
            if cmp_correction(self.queue[i].0, &a, self.queue[w].0, &b) == std::cmp::Ordering::Less {
                w = i;
            }
        }
        let cur = self.refs(path);
        let weakest = self.refs(&self.queue[w].1);
        if cmp_correction(self.queue[w].0, &weakest, score, &cur) == std::cmp::Ordering::Less {
            self.queue[w] = (score, path.to_vec());
        }
    }
}

/// `CandidateScorer.findBestCandiates`: best corrections first.
fn find_best(sets: &[CandidateSet], scorer: &WordScorer<'_>, opts: &PhraseSuggestOptions, cutoff: f64) -> Result<Vec<Correction>> {
    if sets.is_empty() {
        return Ok(Vec::new());
    }
    let misspellings = if opts.max_errors >= 1.0 {
        opts.max_errors as usize
    } else {
        // Java Math.round(float).
        (opts.max_errors * sets.len() as f32 + 0.5).floor().max(0.0) as usize
    };
    let mut search = Search { sets, scorer, gram_size: opts.gram_size, max: opts.shard_size, cutoff, queue: Vec::new(), paths: 0 };
    let mut path = Vec::with_capacity(sets.len());
    search.find(&mut path, 0, misspellings.max(1), 0.0)?;
    let mut queue = std::mem::take(&mut search.queue);
    queue.sort_by(|(sa, pa), (sb, pb)| cmp_correction(*sb, &search.refs(pb), *sa, &search.refs(pa)));
    Ok(queue
        .into_iter()
        .map(|(score, path)| {
            let refs = search.refs(&path);
            Correction {
                terms: refs.iter().map(|c| c.term.clone()).collect(),
                user_input: refs.iter().map(|c| c.user_input).collect(),
                score,
            }
        })
        .collect())
}
