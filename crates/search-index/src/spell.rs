//! Spelling suggestions from the index's terms (Lucene's `DirectSpellChecker`, as
//! Elasticsearch's `term` suggester runs it per token).
//!
//! Candidates are the terms within `max_edits` (Damerau, after an exact prefix) found by a
//! Levenshtein automaton over every segment's dictionary, merged in term order with their
//! summed doc frequencies. One pass at edit distance 1 runs first and a pass at `max_edits`
//! only when it found fewer than `num_sug · max_inspections` terms. Each pass replays
//! `FuzzyTermsEnum` exactly, because its pruning decides which terms are seen: a bounded
//! queue keeps the best terms by similarity (the edit boost `1 - ed / min(len)`, or the
//! chosen string distance), and as the queue's worst score rises the enumeration drops to
//! smaller edit distances — one term late, as Lucene applies the change on its next step.
//! The survivors are sorted by score (or frequency) and cut to `num_sug`.

use std::collections::{BTreeMap, HashSet};

use search_codec::automaton::{Dfa, edit_distance};

use crate::error::{Error, Result};
use crate::search::Searcher;

/// When to suggest (`suggest_mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestMode {
    /// Only for terms not in the index (`missing`, the default).
    Missing,
    /// Only terms more frequent than the input (`popular`).
    Popular,
    /// Always (`always`).
    Always,
}

/// How suggestions are scored (`string_distance`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StringDistance {
    /// The automaton's edit boost (`internal`, the default).
    Internal,
    /// Lucene's `LuceneLevenshteinDistance` (code points, transpositions).
    DamerauLevenshtein,
    /// Lucene's `LevenshteinDistance` (UTF-16 units).
    Levenshtein,
    /// Lucene's `JaroWinklerDistance` (threshold 0.7).
    JaroWinkler,
    /// Lucene's `NGramDistance` (bigrams).
    NGram,
}

/// Suggestion order (`sort`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestSort {
    /// Score, then frequency, then term.
    Score,
    /// Frequency, then score, then term.
    Frequency,
}

/// `DirectSpellChecker` settings, with Elasticsearch's defaults.
#[derive(Clone, Debug)]
pub struct SpellOptions {
    pub accuracy: f32,
    pub max_edits: u32,
    pub max_inspections: u32,
    /// `max_term_freq`: inputs in more docs than this (a fraction of max doc below 1) get no
    /// suggestions.
    pub max_term_freq: f32,
    pub prefix_length: usize,
    pub min_word_length: usize,
    /// `min_doc_freq`: suggestions need more docs than this (a fraction below 1).
    pub min_doc_freq: f32,
    pub string_distance: StringDistance,
    pub sort: SuggestSort,
    pub suggest_mode: SuggestMode,
    pub lowercase_terms: bool,
}

impl Default for SpellOptions {
    fn default() -> SpellOptions {
        SpellOptions {
            accuracy: 0.5,
            max_edits: 2,
            max_inspections: 5,
            max_term_freq: 0.01,
            prefix_length: 1,
            min_word_length: 4,
            min_doc_freq: 0.0,
            string_distance: StringDistance::Internal,
            sort: SuggestSort::Score,
            suggest_mode: SuggestMode::Missing,
            lowercase_terms: false,
        }
    }
}

/// A suggested term.
#[derive(Clone, Debug, PartialEq)]
pub struct SuggestWord {
    pub term: String,
    pub score: f32,
    /// Docs containing it (deleted ones included, as in Lucene).
    pub freq: u32,
}

#[derive(Clone, Debug)]
struct ScoreTerm {
    term: Vec<u8>,
    /// What the queue orders by: the edit boost, or the string distance's score.
    boost: f32,
    df: u32,
}

/// `ScoreTerm.compareTo`: lower boost first; equal boosts put the larger term first (so it
/// leaves the queue first).
fn worse(a: &ScoreTerm, b: &ScoreTerm) -> bool {
    if a.boost == b.boost { a.term > b.term } else { a.boost < b.boost }
}

impl Searcher {
    /// Suggestions for `term` in `field` (Lucene's `DirectSpellChecker.suggestSimilar`).
    pub fn suggest_similar(&self, field: u32, term: &str, num_sug: usize, opts: &SpellOptions) -> Result<Vec<SuggestWord>> {
        if opts.max_edits < 1 || opts.max_edits > 2 {
            return Err(Error::illegal("Invalid maxEdits"));
        }
        for f in [opts.min_doc_freq, opts.max_term_freq] {
            if f >= 1.0 && f != f as i32 as f32 {
                return Err(Error::illegal("Fractional absolute document frequencies are not allowed"));
            }
        }
        let text_len = term.chars().count();
        if text_len < opts.min_word_length {
            return Ok(Vec::new());
        }
        let lowered;
        let term = if opts.lowercase_terms {
            lowered = term.to_lowercase();
            lowered.as_str()
        } else {
            term
        };
        let mut docfreq = self.term_stats(field, term.as_bytes()).doc_freq as i64;
        if opts.suggest_mode == SuggestMode::Missing && docfreq > 0 {
            return Ok(Vec::new());
        }
        let max_doc = self.max_doc() as f32;
        if opts.max_term_freq >= 1.0 {
            if docfreq as f32 > opts.max_term_freq {
                return Ok(Vec::new());
            }
        } else if docfreq > (opts.max_term_freq * max_doc).ceil() as i64 {
            return Ok(Vec::new());
        }
        if opts.suggest_mode != SuggestMode::Popular {
            docfreq = 0;
        }
        if opts.min_doc_freq >= 1.0 {
            docfreq = docfreq.max(opts.min_doc_freq as i64);
        } else if opts.min_doc_freq > 0.0 {
            docfreq = docfreq.max((opts.min_doc_freq * max_doc) as i64 - 1);
        }
        let inspections = num_sug.saturating_mul(opts.max_inspections as usize);
        let mut terms = self.spell_pass(field, term, inspections, docfreq, 1, opts);
        if opts.max_edits > 1 && terms.len() < inspections {
            let mut seen: HashSet<Vec<u8>> = terms.iter().map(|t| t.term.clone()).collect();
            for t in self.spell_pass(field, term, inspections, docfreq, opts.max_edits, opts) {
                if seen.insert(t.term.clone()) {
                    terms.push(t);
                }
            }
        }
        let mut out: Vec<SuggestWord> =
            terms.into_iter().map(|t| SuggestWord { term: String::from_utf8_lossy(&t.term).into_owned(), score: t.boost, freq: t.df }).collect();
        // Best first: the comparator reversed; equal keys put the smaller term (UTF-16) first.
        out.sort_by(|a, b| {
            let (x, y) = match opts.sort {
                SuggestSort::Score => (b.score.total_cmp(&a.score), b.freq.cmp(&a.freq)),
                SuggestSort::Frequency => (b.freq.cmp(&a.freq), b.score.total_cmp(&a.score)),
            };
            x.then(y).then_with(|| a.term.encode_utf16().cmp(b.term.encode_utf16()))
        });
        out.truncate(num_sug);
        Ok(out)
    }

    /// One `suggestSimilar` pass at `ed_max` edits.
    fn spell_pass(&self, field: u32, term: &str, num_sug: usize, docfreq: i64, ed_max: u32, opts: &SpellOptions) -> Vec<ScoreTerm> {
        let prefix = opts.prefix_length.max(ed_max as usize - 1);
        let qchars: Vec<char> = term.chars().collect();
        let term_len = qchars.len();
        let p = prefix.min(term_len);
        let qsuffix: String = qchars[p..].iter().collect();
        // Terms the ed_max automaton accepts, merged across segments, with their summed doc
        // freqs.
        let dfa = Dfa::levenshtein(term, ed_max, prefix, true);
        let mut merged: BTreeMap<Vec<u8>, u32> = BTreeMap::new();
        for seg in self.segments() {
            let Some(tr) = seg.reader.terms(field) else { continue };
            let mut it = tr.intersect(&dfa);
            while it.next().is_some() {
                let df = it.doc_freq();
                *merged.entry(it.term().to_vec()).or_insert(0) += df;
            }
        }
        let mut queue: Vec<ScoreTerm> = Vec::new();
        let worst = |q: &[ScoreTerm]| -> usize { (0..q.len()).fold(0, |w, i| if worse(&q[i], &q[w]) { i } else { w }) };
        let mut cur_max = ed_max;
        let mut this_bottom = f32::NEG_INFINITY;
        let mut attr_bottom = f32::NEG_INFINITY;
        let mut queued = false;
        for (cand, &df) in &merged {
            let Ok(cs) = std::str::from_utf8(cand) else { continue };
            let cchars: Vec<char> = cs.chars().collect();
            let csuffix: String = cchars[p.min(cchars.len())..].iter().collect();
            let ed = edit_distance(&qsuffix, &csuffix, true);
            // FuzzyTermsEnum.next(): apply a bottom change seen on the previous term.
            if queued {
                queued = false;
                while cur_max > 0 {
                    let max_boost = 1.0f32 - cur_max as f32 / term_len as f32;
                    if this_bottom < max_boost {
                        break;
                    }
                    cur_max -= 1;
                }
            }
            if ed > cur_max {
                continue;
            }
            let boost = if ed == 0 { 1.0 } else { 1.0f32 - ed as f32 / cchars.len().min(term_len) as f32 };
            if attr_bottom != this_bottom {
                this_bottom = attr_bottom;
                queued = true;
            }
            // DirectSpellChecker's consumer.
            let mut score = boost;
            if queue.len() >= num_sug && num_sug > 0 && score <= queue[worst(&queue)].boost {
                continue;
            }
            if cand.as_slice() == term.as_bytes() {
                continue;
            }
            if (df as i64) <= docfreq {
                continue;
            }
            if opts.string_distance != StringDistance::Internal {
                score = distance(opts.string_distance, term, cs);
            }
            if score < opts.accuracy {
                continue;
            }
            queue.push(ScoreTerm { term: cand.clone(), boost: score, df });
            if queue.len() > num_sug {
                let w = worst(&queue);
                queue.swap_remove(w);
            }
            attr_bottom = if queue.len() >= num_sug && !queue.is_empty() { queue[worst(&queue)].boost } else { f32::NEG_INFINITY };
        }
        queue
    }
}

/// Lucene's string distances (`getDistance(target = input, other = candidate)`).
pub fn distance(kind: StringDistance, target: &str, other: &str) -> f32 {
    match kind {
        StringDistance::Internal | StringDistance::DamerauLevenshtein => lucene_levenshtein(target, other),
        StringDistance::Levenshtein => levenshtein(target, other),
        StringDistance::JaroWinkler => jaro_winkler(target, other),
        StringDistance::NGram => ngram(target, other, 2),
    }
}

/// `LuceneLevenshteinDistance`: Damerau (optimal string alignment) over code points,
/// normalized by the shorter length.
fn lucene_levenshtein(target: &str, other: &str) -> f32 {
    let t: Vec<char> = target.chars().collect();
    let o: Vec<char> = other.chars().collect();
    let (n, m) = (t.len(), o.len());
    if n == 0 || m == 0 {
        return if n == m { 0.0 } else { n.max(m) as f32 };
    }
    let w = m + 1;
    let mut d = vec![0usize; (n + 1) * w];
    for i in 0..=n {
        d[i * w] = i;
    }
    for (j, v) in d.iter_mut().enumerate().take(m + 1) {
        *v = j;
    }
    for j in 1..=m {
        let tj = o[j - 1];
        for i in 1..=n {
            let cost = usize::from(t[i - 1] != tj);
            let mut v = (d[(i - 1) * w + j] + 1).min(d[i * w + j - 1] + 1).min(d[(i - 1) * w + j - 1] + cost);
            if i > 1 && j > 1 && t[i - 1] == o[j - 2] && t[i - 2] == o[j - 1] {
                v = v.min(d[(i - 2) * w + j - 2] + cost);
            }
            d[i * w + j] = v;
        }
    }
    1.0f32 - d[n * w + m] as f32 / m.min(n) as f32
}

/// `LevenshteinDistance` over UTF-16 units, normalized by the longer length.
fn levenshtein(target: &str, other: &str) -> f32 {
    let sa: Vec<u16> = target.encode_utf16().collect();
    let o: Vec<u16> = other.encode_utf16().collect();
    let (n, m) = (sa.len(), o.len());
    if n == 0 || m == 0 {
        return if n == m { 1.0 } else { 0.0 };
    }
    let mut p: Vec<usize> = (0..=n).collect();
    let mut d = vec![0usize; n + 1];
    for (j, &tj) in o.iter().enumerate() {
        d[0] = j + 1;
        for i in 1..=n {
            let cost = usize::from(sa[i - 1] != tj);
            d[i] = (d[i - 1] + 1).min(p[i] + 1).min(p[i - 1] + cost);
        }
        std::mem::swap(&mut p, &mut d);
    }
    1.0f32 - p[n] as f32 / m.max(n) as f32
}

/// `JaroWinklerDistance` (threshold 0.7) over UTF-16 units, Lucene's arithmetic included.
fn jaro_winkler(s1: &str, s2: &str) -> f32 {
    let a: Vec<u16> = s1.encode_utf16().collect();
    let b: Vec<u16> = s2.encode_utf16().collect();
    let (max, min) = if a.len() > b.len() { (&a, &b) } else { (&b, &a) };
    let range = (max.len() / 2).saturating_sub(1);
    let mut match_idx = vec![usize::MAX; min.len()];
    let mut flags = vec![false; max.len()];
    let mut matches = 0usize;
    for (mi, &c1) in min.iter().enumerate() {
        let lo = mi.saturating_sub(range);
        let hi = (mi + range + 1).min(max.len());
        for xi in lo..hi {
            if !flags[xi] && c1 == max[xi] {
                match_idx[mi] = xi;
                flags[xi] = true;
                matches += 1;
                break;
            }
        }
    }
    let ms1: Vec<u16> = (0..min.len()).filter(|&i| match_idx[i] != usize::MAX).map(|i| min[i]).collect();
    let ms2: Vec<u16> = (0..max.len()).filter(|&i| flags[i]).map(|i| max[i]).collect();
    let transpositions = ms1.iter().zip(&ms2).filter(|(x, y)| x != y).count();
    let mut prefix = 0usize;
    for mi in 0..min.len() {
        // Lucene compares the original strings here.
        if a.get(mi) == b.get(mi) {
            prefix += 1;
        } else {
            break;
        }
    }
    let m = matches as f32;
    if m == 0.0 {
        return 0.0;
    }
    let j = (m / a.len() as f32 + m / b.len() as f32 + (m - (transpositions / 2) as f32) / m) / 3.0;
    if j < 0.7 { j } else { j + 0.1f32.min(1.0 / max.len() as f32) * prefix as f32 * (1.0 - j) }
}

/// `NGramDistance(n)` over UTF-16 units.
fn ngram(source: &str, target: &str, n: usize) -> f32 {
    let s: Vec<u16> = source.encode_utf16().collect();
    let t: Vec<u16> = target.encode_utf16().collect();
    let (sl, tl) = (s.len(), t.len());
    if sl == 0 || tl == 0 {
        return if sl == tl { 1.0 } else { 0.0 };
    }
    if sl < n || tl < n {
        let cost = (0..sl.min(tl)).filter(|&i| s[i] == t[i]).count();
        return cost as f32 / sl.max(tl) as f32;
    }
    let mut sa = vec![0u16; sl + n - 1];
    sa[n - 1..].copy_from_slice(&s);
    let mut p: Vec<f32> = (0..=sl).map(|i| i as f32).collect();
    let mut d = vec![0f32; sl + 1];
    let mut tj = vec![0u16; n];
    for j in 1..=tl {
        if j < n {
            for (ti, x) in tj.iter_mut().enumerate() {
                *x = if ti < n - j { 0 } else { t[ti - (n - j)] };
            }
        } else {
            tj.copy_from_slice(&t[j - n..j]);
        }
        d[0] = j as f32;
        for i in 1..=sl {
            let mut cost = 0usize;
            let mut tn = n;
            for ni in 0..n {
                if sa[i - 1 + ni] != tj[ni] {
                    cost += 1;
                } else if sa[i - 1 + ni] == 0 {
                    tn -= 1;
                }
            }
            let ec = cost as f32 / tn as f32;
            d[i] = (d[i - 1] + 1.0).min(p[i] + 1.0).min(p[i - 1] + ec);
        }
        std::mem::swap(&mut p, &mut d);
    }
    1.0f32 - p[sl] / tl.max(sl) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_distances_follow_lucene() {
        assert_eq!(lucene_levenshtein("abcd", "abdc"), 0.75);
        assert_eq!(lucene_levenshtein("", ""), 0.0);
        assert_eq!(levenshtein("kitten", "sitting"), 1.0 - 3.0 / 7.0);
        assert_eq!(levenshtein("", ""), 1.0);
        assert!((jaro_winkler("martha", "marhta") - 0.9611111).abs() < 1e-6);
        assert_eq!(jaro_winkler("abc", "xyz"), 0.0);
        assert_eq!(ngram("ab", "ab", 2), 1.0);
        assert_eq!(ngram("a", "b", 2), 0.0);
        assert!(ngram("hello", "hallo", 2) < 1.0);
    }
}
