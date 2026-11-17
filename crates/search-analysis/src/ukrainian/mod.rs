//! The `analysis-ukrainian` plugin's `ukrainian` analyzer (Elasticsearch's
//! `UkrainianAnalyzerProvider` over Lucene's `UkrainianMorfologikAnalyzer`): apostrophes,
//! stress marks, soft hyphens and `ґ` normalized by a mapping char filter, the standard
//! tokenizer, lower case, stop words, an optional stem exclusion set, then every lemma the
//! dictionary has for a word (`MorfologikFilter`), at the word's position.
//!
//! The dictionary is `ua/net/nlp/ukrainian.dict` of `morfologik-ukrainian-search` 3.7.5 (the
//! version Elasticsearch 9.5.5 ships; based on the dict_uk project and licensed under GPL/LGPL,
//! CC BY-NC-SA 4.0 and Apache-2.0, used here under Apache-2.0): a Morfologik CFSA2 automaton
//! of `form+code+tag` entries with suffix-encoded lemmas. It is read where it lies in the
//! binary (6.6 MB, never copied): an index that never analyzes Ukrainian never touches it.
//! The automaton code ports Morfologik 2.1.1 (`CFSA2`, `FSATraversal`,
//! `ByteSequenceIterator`, `DictionaryLookup`, `TrimSuffixEncoder`; BSD). The stop words are
//! Lucene's `uk/stopwords.txt` (Apache-2.0).

use std::sync::{Arc, OnceLock};

use crate::analyzer::{Analyzer, AnalyzerSpec};
use crate::analyzers::{AnalyzerBuilder, POSITION_INCREMENT_GAP, lowercase, standard_tokenizer, stop_filter};
use crate::component::{FilterFactory, NamedCharFilter, NamedFilter, NamedTokenizer, keyword_aware_fn};
use crate::error::Result;
use crate::filter::WordSet;
use crate::registry::{self, Ctx};
use crate::settings::Settings;
use crate::stream::{Filter, TokenStream};
use crate::token::Tok;
use crate::unicode;

static DICT: &[u8] = include_bytes!("ukrainian.dict");

/// The `fsa.dict.separator` of `ukrainian.info`.
const SEPARATOR: u8 = b'+';

const BIT_TARGET_NEXT: u8 = 1 << 7;
const BIT_LAST_ARC: u8 = 1 << 6;
const BIT_FINAL_ARC: u8 = 1 << 5;
const LABEL_INDEX_MASK: u8 = (1 << 5) - 1;

/// Morfologik's `CFSA2` over the dictionary bytes.
struct Fsa {
    arcs: &'static [u8],
    labels: &'static [u8],
    numbers: bool,
    root: usize,
}

impl Fsa {
    fn get() -> &'static Fsa {
        static F: OnceLock<Fsa> = OnceLock::new();
        F.get_or_init(|| {
            // `\fsa`, version 0xc6, flags (u16), label mapping size and bytes, then the arcs.
            assert!(DICT.starts_with(b"\\fsa") && DICT[4] == 0xc6, "ukrainian.dict is a CFSA2 automaton");
            let flags = u16::from_be_bytes([DICT[5], DICT[6]]);
            let n = DICT[7] as usize;
            let labels = &DICT[8..8 + n];
            let mut fsa = Fsa { arcs: &DICT[8 + n..], labels, numbers: flags & (1 << 8) != 0, root: 0 };
            fsa.root = fsa.destination(fsa.first_arc(0));
            fsa
        })
    }

    fn flag(&self, arc: usize) -> u8 {
        self.arcs[arc]
    }

    fn first_arc(&self, node: usize) -> usize {
        if self.numbers { self.skip_vint(node) } else { node }
    }

    fn is_last(&self, arc: usize) -> bool {
        self.flag(arc) & BIT_LAST_ARC != 0
    }

    fn is_final(&self, arc: usize) -> bool {
        self.flag(arc) & BIT_FINAL_ARC != 0
    }

    fn is_terminal(&self, arc: usize) -> bool {
        self.destination(arc) == 0
    }

    fn next_arc(&self, arc: usize) -> usize {
        if self.is_last(arc) { 0 } else { self.skip_arc(arc) }
    }

    fn label(&self, arc: usize) -> u8 {
        match (self.flag(arc) & LABEL_INDEX_MASK) as usize {
            0 => self.arcs[arc + 1],
            i => self.labels[i],
        }
    }

    fn arc(&self, node: usize, label: u8) -> usize {
        let mut arc = self.first_arc(node);
        while arc != 0 {
            if self.label(arc) == label {
                return arc;
            }
            arc = self.next_arc(arc);
        }
        0
    }

    fn destination(&self, mut arc: usize) -> usize {
        if self.flag(arc) & BIT_TARGET_NEXT != 0 {
            while !self.is_last(arc) {
                arc = self.next_arc(arc);
            }
            self.skip_arc(arc)
        } else {
            let off = if self.flag(arc) & LABEL_INDEX_MASK == 0 { 2 } else { 1 };
            self.read_vint(arc + off)
        }
    }

    fn skip_arc(&self, mut off: usize) -> usize {
        let flag = self.arcs[off];
        off += 1;
        if flag & LABEL_INDEX_MASK == 0 {
            off += 1;
        }
        if flag & BIT_TARGET_NEXT == 0 {
            off = self.skip_vint(off);
        }
        off
    }

    fn read_vint(&self, mut off: usize) -> usize {
        let mut b = self.arcs[off];
        let mut v = (b & 0x7f) as usize;
        let mut shift = 7;
        while b & 0x80 != 0 {
            off += 1;
            b = self.arcs[off];
            v |= ((b & 0x7f) as usize) << shift;
            shift += 7;
        }
        v
    }

    fn skip_vint(&self, mut off: usize) -> usize {
        while self.arcs[off] & 0x80 != 0 {
            off += 1;
        }
        off + 1
    }

    /// `DictionaryLookup.lookup`: the lemmas of a word form, in the automaton's order.
    fn lookup(&self, word: &str, out: &mut Vec<String>) {
        out.clear();
        if word.contains(SEPARATOR as char) {
            return;
        }
        let bytes = word.as_bytes();
        // `FSATraversal.match`: only a word that is a proper prefix of entries has lemmas.
        let mut node = self.root;
        for (i, &b) in bytes.iter().enumerate() {
            let arc = self.arc(node, b);
            if arc == 0 || (self.is_final(arc) && i + 1 == bytes.len()) || self.is_terminal(arc) {
                return;
            }
            node = self.destination(arc);
        }
        let arc = self.arc(node, SEPARATOR);
        if arc == 0 || self.is_final(arc) {
            return;
        }
        // `ByteSequenceIterator` from the separator's node: every final path, depth first.
        let mut stack: Vec<usize> = vec![self.first_arc(self.destination(arc))];
        let mut buf: Vec<u8> = Vec::new();
        while let Some(&arc) = stack.last() {
            let depth = stack.len() - 1;
            if arc == 0 {
                stack.pop();
                continue;
            }
            *stack.last_mut().unwrap() = self.next_arc(arc);
            buf.truncate(depth);
            buf.push(self.label(arc));
            if !self.is_terminal(arc) {
                stack.push(self.first_arc(self.destination(arc)));
            }
            if self.is_final(arc) {
                // `TrimSuffixEncoder.decode` of the bytes before the next separator.
                let code = buf.iter().position(|&b| b == SEPARATOR).map_or(&buf[..], |p| &buf[..p]);
                let Some((&trim, suffix)) = code.split_first() else { continue };
                let trim = trim.wrapping_sub(b'A') as usize;
                let keep = if trim == 255 { 0 } else { bytes.len().saturating_sub(trim) };
                let mut stem = bytes[..keep].to_vec();
                stem.extend_from_slice(suffix);
                out.push(String::from_utf8_lossy(&stem).into_owned());
            }
        }
    }
}

/// `MorfologikFilter`: each lemma of a word that isn't a keyword (looked up as is, then lower
/// cased), the first in the word's place and the rest at the same position.
struct Morfologik {
    lemmas: Vec<String>,
    next: usize,
    state: Tok,
}

impl Filter for Morfologik {
    fn reset(&mut self) {
        self.lemmas.clear();
        self.next = 0;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if self.next < self.lemmas.len() {
            t.clone_from(&self.state);
            t.pos_inc = 0;
            t.term.clone_from(&self.lemmas[self.next]);
            self.next += 1;
            return true;
        }
        if !input.next(t) {
            return false;
        }
        self.lemmas.clear();
        self.next = 0;
        if !t.keyword {
            let fsa = Fsa::get();
            fsa.lookup(&t.term, &mut self.lemmas);
            if self.lemmas.is_empty() {
                let lower: String = t.term.chars().map(unicode::to_lower).collect();
                fsa.lookup(&lower, &mut self.lemmas);
            }
            if !self.lemmas.is_empty() {
                self.state.clone_from(t);
                t.term.clone_from(&self.lemmas[0]);
                self.next = 1;
            }
        }
        true
    }
}

fn default_stop() -> &'static WordSet {
    static S: OnceLock<WordSet> = OnceLock::new();
    // `WordlistLoader.getSnowballWordSet`: `|` starts a comment, words are separated by spaces.
    S.get_or_init(|| {
        let words = include_str!("stopwords.txt")
            .lines()
            .flat_map(|l| l.split('|').next().unwrap_or("").split_whitespace());
        WordSet::new(words, false)
    })
}

/// The `ukrainian` analyzer (`stopwords`, `stopwords_path` and `stem_exclusion`).
pub(crate) fn analyzer_provider(ty: &str) -> Option<AnalyzerBuilder> {
    (ty == "ukrainian").then_some(|_: &Ctx, name: &str, s: &Settings| {
        let stop = registry::parse_stop_words(s, default_stop())?;
        let excl = registry::parse_stem_exclusion(s)?;
        ukrainian(name, stop, excl)
    })
}

fn ukrainian(name: &str, stop: WordSet, excl: WordSet) -> Result<Analyzer> {
    let rules: Vec<(Vec<u16>, Vec<u16>)> = [
        ("\u{2019}", "'"),
        ("\u{2018}", "'"),
        ("\u{02BC}", "'"),
        ("`", "'"),
        ("\u{B4}", "'"),
        ("\u{0301}", ""),
        ("\u{00AD}", ""),
        ("ґ", "г"),
        ("Ґ", "Г"),
    ]
    .iter()
    .map(|(a, b)| (a.encode_utf16().collect(), b.encode_utf16().collect()))
    .collect();
    let mut filters: Vec<Arc<dyn FilterFactory>> = vec![lowercase(), stop_filter(stop)];
    if !excl.is_empty() {
        let words = Arc::new(excl);
        filters.push(keyword_aware_fn(move || {
            Box::new(crate::filter::basic::KeywordMarker::Set(words.clone(), String::new()))
        }));
    }
    filters.push(keyword_aware_fn(|| Box::new(Morfologik { lemmas: Vec::new(), next: 0, state: Tok::default() })));
    AnalyzerSpec {
        name: name.to_string(),
        char_filters: vec![NamedCharFilter { name: name.into(), factory: crate::charfilter::mapping(&rules)? }],
        tokenizer: NamedTokenizer { name: name.into(), factory: standard_tokenizer(255) },
        filters: filters.into_iter().map(|f| NamedFilter { name: name.into(), factory: f }).collect(),
        custom: false,
        pos_gap: POSITION_INCREMENT_GAP,
        offset_gap: 1,
        // `Analyzer.normalize` is the identity: the analyzer overrides neither normalization
        // hook.
        norm_filters: Some(Vec::new()),
    }
    .build()
}

#[cfg(test)]
mod tests {
    #[test]
    fn lemmas_in_dictionary_order() {
        let mut out = Vec::new();
        super::Fsa::get().lookup("колу", &mut out);
        assert_eq!(out, ["кола", "коло", "кіл"]);
    }
}
