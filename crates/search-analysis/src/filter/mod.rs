//! Token filters.
//!
//! Each filter is a port of the Lucene filter Elasticsearch's factory instantiates. They share a
//! few building blocks defined here: [`WordSet`] (Lucene's `CharArraySet`, optionally
//! case-insensitive with `Character.toLowerCase` folding), [`Filtering`] (Lucene's
//! `FilteringTokenFilter`, which turns removed tokens into position increments) and [`Map`]
//! (in-place term rewriting). Filters rewrite the chain's [`Tok`] in place and keep any lookahead
//! in reusable buffers, so none of them allocates per token once warmed up.

pub(crate) mod ascii_folding_table;
pub(crate) mod basic;
pub(crate) mod branch;
pub(crate) mod case;
pub(crate) mod fixed_shingle;
pub(crate) mod folding;
pub(crate) mod graph;
pub(crate) mod misc;
pub(crate) mod ngram;
pub(crate) mod shingle;
pub(crate) mod synonym;
pub(crate) mod word_delimiter;

use std::collections::HashSet;
use std::sync::Arc;

use crate::component::{FilterFactory, normalizing_fn};
use crate::error::Result;
use crate::registry::{Ctx, FilterBuilder, Provider};
use crate::stream::{Filter, TokenStream};
use crate::token::Tok;
use crate::unicode;

type Hasher = foldhash::fast::RandomState;

/// Lucene's `CharArraySet`.
#[derive(Clone, Debug, Default)]
pub(crate) struct WordSet {
    words: HashSet<Box<str>, Hasher>,
    ignore_case: bool,
}

impl WordSet {
    pub fn new<I, S>(words: I, ignore_case: bool) -> WordSet
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut set = WordSet {
            words: HashSet::with_hasher(Hasher::default()),
            ignore_case,
        };
        for w in words {
            set.add(w.as_ref());
        }
        set
    }

    pub fn add(&mut self, w: &str) {
        if self.ignore_case {
            let mut s = String::with_capacity(w.len());
            unicode::push_lower(&mut s, w);
            self.words.insert(s.into_boxed_str());
        } else {
            self.words.insert(w.into());
        }
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Membership; `scratch` holds the folded form for case-insensitive sets.
    #[inline]
    pub fn contains(&self, s: &str, scratch: &mut String) -> bool {
        if self.words.is_empty() {
            return false;
        }
        if !self.ignore_case {
            return self.words.contains(s);
        }
        if s.is_ascii() && !s.bytes().any(|b| b.is_ascii_uppercase()) {
            return self.words.contains(s);
        }
        scratch.clear();
        unicode::push_lower(scratch, s);
        self.words.contains(scratch.as_str())
    }
}

/// A reusable set of terms for per-document state (`unique`): the terms live in one arena and
/// an open-addressing table indexes them, so clearing keeps every allocation and a warmed-up
/// set inserts without allocating.
#[derive(Default)]
pub(crate) struct TermSet {
    arena: String,
    spans: Vec<(u32, u32)>,
    /// 0 = empty, otherwise span index + 1.
    slots: Vec<u32>,
}

impl TermSet {
    fn hash(s: &str) -> u64 {
        use std::hash::BuildHasher;
        foldhash::fast::FixedState::with_seed(0x5eed).hash_one(s)
    }

    fn term(&self, i: u32) -> &str {
        let (a, b) = self.spans[i as usize];
        &self.arena[a as usize..b as usize]
    }

    pub fn clear(&mut self) {
        if !self.spans.is_empty() {
            self.arena.clear();
            self.spans.clear();
            self.slots.iter_mut().for_each(|x| *x = 0);
        }
    }

    #[cfg(test)]
    pub fn contains(&self, s: &str) -> bool {
        if self.slots.is_empty() {
            return false;
        }
        let mask = self.slots.len() - 1;
        let mut i = Self::hash(s) as usize & mask;
        loop {
            match self.slots[i] {
                0 => return false,
                x if self.term(x - 1) == s => return true,
                _ => i = (i + 1) & mask,
            }
        }
    }

    /// Adds `s`; false when it was already present.
    pub fn insert(&mut self, s: &str) -> bool {
        if (self.spans.len() + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let mask = self.slots.len() - 1;
        let mut i = Self::hash(s) as usize & mask;
        loop {
            match self.slots[i] {
                0 => break,
                x if self.term(x - 1) == s => return false,
                _ => i = (i + 1) & mask,
            }
        }
        let start = self.arena.len() as u32;
        self.arena.push_str(s);
        self.spans.push((start, self.arena.len() as u32));
        self.slots[i] = self.spans.len() as u32;
        true
    }

    fn grow(&mut self) {
        let n = (self.slots.len() * 2).max(16);
        self.slots.clear();
        self.slots.resize(n, 0);
        let mask = n - 1;
        for idx in 0..self.spans.len() {
            let mut i = Self::hash(self.term(idx as u32)) as usize & mask;
            while self.slots[i] != 0 {
                i = (i + 1) & mask;
            }
            self.slots[i] = idx as u32 + 1;
        }
    }
}

/// Removes tokens `keep` rejects, adding their position increments to the next kept token
/// (`FilteringTokenFilter`).
pub(crate) struct Filtering<A> {
    pub keep: A,
    skipped: u32,
}

/// The predicate of a [`Filtering`] filter.
pub(crate) trait Keep: Send {
    fn reset(&mut self) {}
    fn keep(&mut self, t: &Tok) -> bool;
}

impl<A: Keep> Filtering<A> {
    pub fn new(keep: A) -> Self {
        Filtering { keep, skipped: 0 }
    }
}

impl<A: Keep> Filter for Filtering<A> {
    fn reset(&mut self) {
        self.skipped = 0;
        self.keep.reset();
    }

    #[inline]
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        self.skipped = 0;
        while input.next(t) {
            if self.keep.keep(t) {
                t.pos_inc += self.skipped;
                return true;
            }
            self.skipped += t.pos_inc;
        }
        false
    }

    fn end(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        input.end(t);
        t.pos_inc += self.skipped;
    }
}

/// A filter that rewrites each token in place.
pub(crate) struct Map<F>(pub F);

impl<F: FnMut(&mut Tok) + Send> Filter for Map<F> {
    #[inline]
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !input.next(t) {
            return false;
        }
        (self.0)(t);
        true
    }
}

/// A filter whose name Elasticsearch rejects when an analyzer uses it.
fn unusable(msg: &'static str) -> Arc<dyn FilterFactory> {
    Arc::new(crate::component::Unusable(
        crate::error::AnalysisError::illegal(msg),
    ))
}

/// Filter types beyond the basic ones (`registry::filter_provider` falls through to here).
pub(crate) fn extra_provider(ty: &str) -> Option<Provider<FilterBuilder>> {
    let p = |build: FilterBuilder| {
        Some(Provider {
            build,
            requires_settings: false,
        })
    };
    let req = |build: FilterBuilder| {
        Some(Provider {
            build,
            requires_settings: true,
        })
    };
    match ty {
        "asciifolding" => p(|_, _, s| {
            Ok(folding::ascii_folding_factory(
                s.get_bool("preserve_original", false)?,
            ))
        }),
        "scandinavian_folding" => p(|_, _, _| Ok(scandinavian_folding())),
        "scandinavian_normalization" => p(|_, _, _| Ok(scandinavian_normalization())),
        "ngram" => p(|ctx, _, s| ngram::ngram_factory(ctx, s)),
        "edge_ngram" => p(|_, _, s| ngram::edge_ngram_factory(s)),
        "nGram" => p(|ctx, _, s| {
            ngram::ngram_factory(ctx, s)?;
            Ok(unusable(
                "The [nGram] token filter name was deprecated in 6.4 and cannot be used in new indices. Please change the filter name to [ngram] instead.",
            ))
        }),
        "edgeNGram" => p(|_, _, s| {
            ngram::edge_ngram_factory(s)?;
            Ok(unusable(
                "The [edgeNGram] token filter name was deprecated in 6.4 and cannot be used in new indices. Please change the filter name to [edge_ngram] instead.",
            ))
        }),
        "shingle" => p(|ctx, _, s| shingle::shingle_factory(ctx, s)),
        "common_grams" => req(|_, _, s| shingle::common_grams_factory(s)),
        "word_delimiter" => p(|_, _, s| word_delimiter::factory(s, false)),
        "word_delimiter_graph" => p(|_, _, s| word_delimiter::factory(s, true)),
        "synonym" => req(|ctx, name, s| synonym::factory(ctx, name, s, false)),
        "synonym_graph" => req(|ctx, name, s| synonym::factory(ctx, name, s, true)),
        "flatten_graph" => p(|_, _, _| Ok(flatten_graph())),
        "multiplexer" => p(|_, name, s| branch::multiplexer_factory(name, s)),
        "condition" => req(branch::condition_factory),
        "predicate_token_filter" => req(branch::predicate_factory),
        "cjk_width" => p(|_, _, _| Ok(misc::cjk_width_factory())),
        "cjk_bigram" => p(|_, _, s| misc::cjk_bigram_factory(s)),
        "min_hash" => p(|ctx, _, s| misc::min_hash_factory(ctx, s)),
        "pattern_capture" => req(|_, name, s| misc::pattern_capture_factory(name, s)),
        "pattern_replace" => req(|_, name, s| misc::pattern_replace_factory(name, s)),
        "stemmer_override" => req(|_, _, s| misc::stemmer_override_factory(s)),
        "dictionary_decompounder" => req(|_, name, s| misc::decompounder_factory(name, s)),
        "hyphenation_decompounder" => req(|_, name, s| misc::hyphenation_factory(name, s)),
        "hunspell" => req(|_, _, s| misc::hunspell_factory(s)),
        _ => crate::stem::filter_provider(ty),
    }
}

fn flatten_graph() -> Arc<dyn FilterFactory> {
    crate::component::filter_fn(|| Box::new(graph::FlattenGraph::new()))
}

fn scandinavian_folding() -> Arc<dyn FilterFactory> {
    normalizing_fn(|| {
        let mut buf = Vec::new();
        Box::new(Map(move |t: &mut Tok| {
            folding::scandinavian_folding(t, &mut buf)
        }))
    })
}

fn scandinavian_normalization() -> Arc<dyn FilterFactory> {
    normalizing_fn(|| {
        let mut buf = Vec::new();
        Box::new(Map(move |t: &mut Tok| {
            folding::scandinavian_normalization(t, &mut buf)
        }))
    })
}

/// Pre-configured filters beyond the basic ones.
pub(crate) fn extra_prebuilt(ctx: &Ctx, name: &str) -> Option<Result<Arc<dyn FilterFactory>>> {
    Some(Ok(match name {
        "asciifolding" => folding::ascii_folding_factory(false),
        "scandinavian_folding" => scandinavian_folding(),
        "scandinavian_normalization" => scandinavian_normalization(),
        "ngram" => ngram::prebuilt(false),
        "edge_ngram" => ngram::prebuilt(true),
        "shingle" => shingle::shingle_prebuilt(),
        "common_grams" => shingle::common_grams_prebuilt(),
        "word_delimiter" => word_delimiter::prebuilt(false),
        "word_delimiter_graph" => word_delimiter::prebuilt(true),
        "cjk_width" => misc::cjk_width_factory(),
        "cjk_bigram" => misc::cjk_bigram_prebuilt(),
        "type_as_payload" => misc::type_as_payload_factory(),
        _ => return crate::stem::prebuilt(ctx, name),
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::stream::{Chain, Tokenizer};

    /// Runs `filters` over whitespace tokens of `text`: (term, pos_inc, start, end, kind, pos_len).
    pub(crate) fn run_filters(
        filters: Vec<Box<dyn Filter>>,
        text: &str,
    ) -> Vec<(String, u32, u32, u32)> {
        let tk: Box<dyn Tokenizer> = Box::new(crate::tokenizer::CharTokenizer::new(
            std::sync::Arc::new(crate::tokenizer::Whitespace),
            255,
            false,
        ));
        let mut chain = Chain::new(Vec::new(), tk, filters);
        let mut out = Vec::new();
        let n = chain.filters.len();
        chain
            .run(text, n, &mut |_, _| {}, &mut |t| {
                out.push((t.term.clone(), t.pos_inc, t.start, t.end))
            })
            .unwrap();
        out
    }

    pub(crate) fn terms(filters: Vec<Box<dyn Filter>>, text: &str) -> Vec<String> {
        run_filters(filters, text)
            .into_iter()
            .map(|t| t.0)
            .collect()
    }

    #[test]
    fn term_set() {
        let mut t = TermSet::default();
        assert!(t.insert("a") && t.insert("b") && !t.insert("a"));
        for i in 0..100 {
            assert!(t.insert(&i.to_string()));
        }
        assert!(t.contains("57") && !t.contains("100"));
        t.clear();
        assert!(!t.contains("a") && t.insert("a"));
    }

    #[test]
    fn word_set() {
        let mut scratch = String::new();
        let s = WordSet::new(["The", "ÉTÉ"], true);
        assert!(
            s.contains("the", &mut scratch)
                && s.contains("THE", &mut scratch)
                && s.contains("été", &mut scratch)
        );
        let s = WordSet::new(["The"], false);
        assert!(s.contains("The", &mut scratch) && !s.contains("the", &mut scratch));
    }
}
