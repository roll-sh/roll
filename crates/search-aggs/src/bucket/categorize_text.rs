//! `categorize_text` (x-pack ml): groups the text of documents into categories of similar
//! messages, the way Elasticsearch's port of ML's `CTokenListDataCategorizerBase` does.
//!
//! Each value of the field (read from `_source` through [`TextTerms::source_strings`]) is
//! analyzed — by default `first_line_with_letters` + the `ml_standard` tokenizer + a stop
//! filter of day/month names + a 100-token limit ([`standard_tokens`]) — and every token gets a
//! weight from a part-of-speech dictionary (verbs weigh most, dictionary words more than other
//! tokens, runs of three or more dictionary words more again; see [`PosDictionary`]). A string
//! joins the most similar existing category (weighted edit distance over token ids, with quick
//! rejections on total weight and on common-token loss) when the similarity reaches
//! `similarity_threshold`, or starts a new one. Categories keep the tokens common to all their
//! strings, in order; the key is those tokens, the regex matches them in order.
//!
//! Shards return their top `shard_size` categories with their full token lists; the reduce
//! re-categorizes the shard categories with a fresh categorizer (weights kept), adds their
//! counts and sub-aggregations, and orders by count, then key.
//!
//! Text is processed as Java does: UTF-16 code units, `Character.isLetter` classes
//! ([`crate::jchar`]), lengths in code units, `float` similarity arithmetic. Token ids are
//! assigned in first-seen order, which decides tie orders.
//!
//! The part-of-speech dictionary (Elasticsearch's `ml-en.dict`: SCOWL words cross-referenced with
//! the Moby part-of-speech list) is data the embedder installs with
//! [`set_part_of_speech_dictionary`]; without one every token weighs 1 (categories then differ from
//! Elasticsearch's when verbs or dictionary words decide them).

use std::cmp::Ordering;
use std::sync::{Arc, RwLock};

use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch, BindCtx, Subs};
use crate::error::{AggError, Result};
use crate::field::TextTerms;
use crate::internal::{AggValue, Aggs, Internal, MultiBuckets, Prop, ReduceCtx, RenderCtx, multi_bucket_property};
use crate::jchar;
use crate::json::Json;
use crate::memory::MemTracker;
use crate::request::{AggDef, PathElement};
use crate::values::DocCounter;
use crate::xparser::{Field, Token, Tokens, XParser};

/// `TokenListCategorizer.MAX_TOKENS` / `CategorizationAnalyzerConfig.MAX_TOKEN_COUNT`.
const MAX_TOKENS: usize = 100;
/// `SerializableTokenListCategory.KEY_BUDGET`.
const KEY_BUDGET: usize = 10000;
const EPSILON: f32 = 0.000001;

/// Stop words of the standard categorization analyzer (`addDateWordsTokenFilter`).
const DATE_WORDS: [&str; 40] = [
    "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun", "January", "February",
    "March", "April", "May", "June", "July", "August", "September", "October", "November", "December", "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul",
    "Aug", "Sep", "Oct", "Nov", "Dec", "GMT", "UTC",
];

/// The analyzer to use (`CategorizationAnalyzerConfig`).
#[derive(Clone, Debug, PartialEq)]
pub enum AnalyzerConfig {
    /// The standard categorization analyzer.
    Standard,
    /// The standard analyzer with `categorization_filters` as `pattern_replace` char filters
    /// (built by the index, see [`AnalyzerConfig::to_json`]).
    StandardWithFilters(Vec<String>),
    /// `categorization_analyzer`: a global analyzer name or a custom definition.
    Custom(Json),
}

impl AnalyzerConfig {
    /// The `categorization_analyzer` definition Elasticsearch builds (what
    /// [`FieldLookup::categorization_analyzer`](crate::FieldLookup::categorization_analyzer) receives).
    pub fn to_json(&self) -> Json {
        match self {
            AnalyzerConfig::Custom(j) => j.clone(),
            AnalyzerConfig::Standard | AnalyzerConfig::StandardWithFilters(_) => {
                let filters: &[String] = match self {
                    AnalyzerConfig::StandardWithFilters(f) => f,
                    _ => &[],
                };
                let mut chars = vec![Json::str("first_line_with_letters")];
                for f in filters {
                    chars.push(Json::obj().with("type", Json::str("pattern_replace")).with("pattern", Json::str(f)));
                }
                let stop = Json::obj().with("type", Json::str("stop")).with("stopwords", Json::Arr(DATE_WORDS.iter().map(|&w| Json::str(w)).collect()));
                let limit = Json::obj().with("type", Json::str("limit")).with("max_token_count", Json::Int(MAX_TOKENS as i64));
                Json::obj().with("char_filter", Json::Arr(chars)).with("tokenizer", Json::str("ml_standard")).with("filter", Json::Arr(vec![stop, limit]))
            }
        }
    }
}

/// An analyzer for categorization: the token strings of one text.
pub trait CategorizationAnalyzer: Send + Sync {
    fn analyze(&self, text: &str, out: &mut Vec<String>) -> Result<()>;
}

/// The standard categorization analyzer, built in.
pub struct StandardAnalyzer;

impl CategorizationAnalyzer for StandardAnalyzer {
    fn analyze(&self, text: &str, out: &mut Vec<String>) -> Result<()> {
        standard_tokens(text, out);
        Ok(())
    }
}

/// `first_line_with_letters` + `ml_standard` + the date-words stop filter + `limit` 100.
pub fn standard_tokens(text: &str, out: &mut Vec<String>) {
    let units: Vec<u16> = text.encode_utf16().collect();
    let line = first_line_with_letters(&units);
    let mut tok = MlStandardTokenizer::new(line);
    let mut buf = Vec::new();
    let mut kept = 0;
    while kept < MAX_TOKENS && tok.next(&mut buf) {
        let s = String::from_utf16_lossy(&buf);
        if DATE_WORDS.contains(&s.as_str()) {
            continue;
        }
        kept += 1;
        out.push(s);
    }
}

/// `FirstLineWithLettersCharFilter`: the first line holding a letter (without its line break),
/// or nothing.
pub fn first_line_with_letters(text: &[u16]) -> &[u16] {
    let mut seen_letter = false;
    let mut prev_newline: Option<usize> = None;
    let mut end = None;
    for (i, &c) in text.iter().enumerate() {
        if c == b'\n' as u16 {
            if seen_letter {
                end = Some(if i > 0 && text[i - 1] == b'\r' as u16 { i - 1 } else { i });
                break;
            }
            prev_newline = Some(i);
        } else {
            seen_letter = seen_letter || jchar::is_letter(c);
        }
    }
    if !seen_letter {
        return &[];
    }
    let start = prev_newline.map_or(0, |p| p + 1);
    &text[start..end.unwrap_or(text.len())]
}

/// The `ml_standard` tokenizer (`MlStandardTokenizer`): runs of letters and digits, with `_ . -
/// @` inside tokens, `/` and Windows/URL-like `\` paths, and `scheme:` prefixes; tokens that are
/// hex numbers, start with a digit or are only slashes are dropped; `_ . - @ :` are trimmed from
/// the end. A dropped token at the very end (or after a put-back `:` split) ends the stream, as in
/// Elasticsearch.
pub struct MlStandardTokenizer<'a> {
    input: &'a [u16],
    pos: usize,
    put_back: Option<u16>,
    done: bool,
}

impl<'a> MlStandardTokenizer<'a> {
    pub fn new(input: &'a [u16]) -> MlStandardTokenizer<'a> {
        MlStandardTokenizer { input, pos: 0, put_back: None, done: false }
    }

    fn next_char(&mut self) -> Option<u16> {
        if let Some(c) = self.put_back.take() {
            return Some(c);
        }
        let c = self.input.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// The next token (in UTF-16 units) into `term`; `false` at the end of the stream.
    pub fn next(&mut self, term: &mut Vec<u16>) -> bool {
        if self.done {
            return false;
        }
        let ok = self.increment(term);
        if !ok {
            self.done = true;
        }
        ok
    }

    fn increment(&mut self, term: &mut Vec<u16>) -> bool {
        const US: u16 = b'_' as u16;
        const DOT: u16 = b'.' as u16;
        const DASH: u16 = b'-' as u16;
        const AT: u16 = b'@' as u16;
        const COLON: u16 = b':' as u16;
        const SLASH: u16 = b'/' as u16;
        const BACKSLASH: u16 = b'\\' as u16;
        term.clear();
        let mut length = 0usize;
        let mut have_non_hex = false;
        let mut letters_before_colon = 0usize;
        let mut have_colon = false;
        let mut first_backslash: Option<usize> = None;
        let mut first_slash: Option<usize> = None;
        let mut slash_count = 0usize;
        while let Some(c) = self.next_char() {
            let accept = jchar::is_letter_or_digit(c)
                || (length > 0 && (c == US || c == DOT || c == DASH || c == AT || (c == COLON && letters_before_colon == length)))
                || c == SLASH
                || (c == BACKSLASH && (length == 0 || (have_colon && letters_before_colon == 1) || first_backslash == Some(0)));
            if accept {
                term.push(c);
                length += 1;
                if c == COLON {
                    have_colon = true;
                } else if c == SLASH {
                    slash_count += 1;
                    if first_slash.is_none() {
                        first_slash = Some(length - 1);
                    }
                } else if c == BACKSLASH {
                    slash_count += 1;
                    if first_backslash.is_none() {
                        first_backslash = Some(length - 1);
                    }
                } else if have_colon {
                    // Only `x:\…` / `x:/…` continue past a colon: anything else splits before
                    // the colon and re-reads this char.
                    if first_backslash != Some(letters_before_colon + 1) && first_slash != Some(letters_before_colon + 1) {
                        length -= 2;
                        self.put_back = Some(c);
                        break;
                    }
                } else if jchar::is_letter(c) {
                    letters_before_colon += 1;
                }
                have_non_hex = have_non_hex || (!jchar::is_hex_digit(c) && c != DOT && c != DASH && c != AT && c != COLON);
            } else if length > 0 {
                if have_non_hex && !jchar::is_digit(term[0]) && length > slash_count {
                    break;
                }
                length = 0;
                term.clear();
                have_non_hex = false;
                letters_before_colon = 0;
                have_colon = false;
                first_backslash = None;
                first_slash = None;
                slash_count = 0;
            }
        }
        if length == 0 {
            return false;
        }
        if !have_non_hex || jchar::is_digit(term[0]) || length == slash_count {
            return false;
        }
        while matches!(term[length - 1], US | DOT | DASH | AT | COLON) {
            length -= 1;
        }
        term.truncate(length);
        true
    }
}

/// What a word is to the categorizer (`CategorizationPartOfSpeechDictionary.PartOfSpeech`, as
/// far as weights care).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartOfSpeech {
    NotInDictionary,
    Verb,
    Other,
}

/// A part-of-speech dictionary: lower-case words and whether each is a verb.
#[derive(Debug, Default)]
pub struct PosDictionary {
    words: hashbrown::HashMap<String, PartOfSpeech>,
    /// In UTF-16 units, like `String.length()`.
    max_len: usize,
}

impl PosDictionary {
    /// Parses `ml-en.dict`'s format: one `word@codes` per line, the first code letter being the
    /// part of speech (Moby codes: `V`, `t` and `i` are verbs; `N p h o A v C P ! r D I ?`
    /// other words).
    pub fn parse(text: &str) -> Result<PosDictionary> {
        let mut d = PosDictionary::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut split = line.split('@');
            let (Some(word), Some(code), None) = (split.next(), split.next(), split.next()) else {
                return Err(AggError::illegal_argument(format!("Unexpected format in line [{line}]: expected one [@] separator")));
            };
            if word.is_empty() || code.is_empty() {
                return Err(AggError::illegal_argument(format!("Unexpected format in line [{line}]")));
            }
            let pos = match code.chars().next().expect("non-empty") {
                'V' | 't' | 'i' => PartOfSpeech::Verb,
                'N' | 'p' | 'h' | 'o' | 'A' | 'v' | 'C' | 'P' | '!' | 'r' | 'D' | 'I' | '?' => PartOfSpeech::Other,
                c => return Err(AggError::illegal_argument(format!("Unknown part-of-speech code [{c}]"))),
            };
            let lower = word.to_lowercase();
            d.max_len = d.max_len.max(lower.encode_utf16().count());
            d.words.insert(lower, pos);
        }
        Ok(d)
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// `getPartOfSpeech`.
    pub fn part_of_speech(&self, word: &str) -> PartOfSpeech {
        if self.words.is_empty() || word.encode_utf16().count() > self.max_len {
            return PartOfSpeech::NotInDictionary;
        }
        self.words.get(&word.to_lowercase()).copied().unwrap_or(PartOfSpeech::NotInDictionary)
    }
}

static DICTIONARY: RwLock<Option<Arc<PosDictionary>>> = RwLock::new(None);

/// Installs the part-of-speech dictionary every later `categorize_text` uses.
pub fn set_part_of_speech_dictionary(dict: PosDictionary) {
    *DICTIONARY.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(dict));
}

fn dictionary() -> Arc<PosDictionary> {
    DICTIONARY.read().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
}

/// `TokenListCategorizer.WeightCalculator` (ML's `TWeightVerbs5Other2AdjacentBoost6`).
struct WeightCalculator<'a> {
    dict: &'a PosDictionary,
    consecutive: u32,
}

impl WeightCalculator<'_> {
    const MIN_DICTIONARY_LENGTH: usize = 2;
    const CONSECUTIVE_FOR_EXTRA: u32 = 3;
    const EXTRA_VERB_WEIGHT: u32 = 5;
    const EXTRA_OTHER_WEIGHT: u32 = 2;
    const ADJACENCY_BOOST: u32 = 6;

    fn weight(&mut self, term: &str) -> u32 {
        if term.encode_utf16().count() < Self::MIN_DICTIONARY_LENGTH {
            self.consecutive = 0;
            return 1;
        }
        let pos = self.dict.part_of_speech(term);
        if pos == PartOfSpeech::NotInDictionary {
            self.consecutive = 0;
            return 1;
        }
        let pos_weight = if pos == PartOfSpeech::Verb { Self::EXTRA_VERB_WEIGHT } else { Self::EXTRA_OTHER_WEIGHT };
        self.consecutive += 1;
        let boost = if self.consecutive >= Self::CONSECUTIVE_FOR_EXTRA { Self::ADJACENCY_BOOST } else { 1 };
        1 + pos_weight * boost
    }

    fn min_matching_weight(weight: i32) -> i32 {
        if weight <= Self::ADJACENCY_BOOST as i32 { weight } else { 1 + (weight - 1) / Self::ADJACENCY_BOOST as i32 }
    }

    fn max_matching_weight(weight: i32) -> i32 {
        let (lo, hi) = (Self::EXTRA_VERB_WEIGHT.min(Self::EXTRA_OTHER_WEIGHT) as i32, (Self::EXTRA_VERB_WEIGHT.max(Self::EXTRA_OTHER_WEIGHT) + 1) as i32);
        if weight <= lo || weight > hi { weight } else { 1 + (weight - 1) * Self::ADJACENCY_BOOST as i32 }
    }
}

/// A token id and its weight (`TokenAndWeight`; ordered by id only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tw {
    id: u32,
    weight: u32,
}

/// Token bytes ↔ ids in first-seen order (`CategorizationBytesRefHash`).
#[derive(Default, Clone, Debug)]
struct TokenIds {
    ids: hashbrown::HashMap<Box<[u8]>, u32>,
    tokens: Vec<Box<[u8]>>,
}

impl TokenIds {
    fn put(&mut self, t: &[u8]) -> u32 {
        if let Some(&id) = self.ids.get(t) {
            return id;
        }
        let id = self.tokens.len() as u32;
        self.ids.insert(t.into(), id);
        self.tokens.push(t.into());
        id
    }
}

/// `TokenListCategory`.
#[derive(Clone, Debug)]
struct Category {
    base: Vec<Tw>,
    base_weight: i32,
    base_unfiltered_len: i32,
    max_unfiltered_len: i32,
    ord_begin: usize,
    ord_end: usize,
    /// Sorted by token id.
    common: Vec<Tw>,
    common_weight: i32,
    orig_unique_weight: i32,
    num_matches: u64,
    bucket_ord: Option<u32>,
    sub_aggs: Vec<Aggs>,
}

fn weight_sum(list: &[Tw]) -> i32 {
    list.iter().map(|t| t.weight as i32).sum()
}

impl Category {
    fn new(unfiltered_len: i32, base: &[Tw], unique: &[Tw], max_unfiltered_len: i32, num_matches: u64) -> Category {
        let common_weight = weight_sum(unique);
        Category {
            base: base.to_vec(),
            base_weight: weight_sum(base),
            base_unfiltered_len: unfiltered_len,
            max_unfiltered_len,
            ord_begin: 0,
            ord_end: base.len(),
            common: unique.to_vec(),
            common_weight,
            orig_unique_weight: common_weight,
            num_matches,
            bucket_ord: None,
            sub_aggs: Vec::new(),
        }
    }

    /// The category of a shard's result (`TokenListCategory(id, serializable, hash)`).
    fn from_wire(w: &WireCategory, ids: &mut TokenIds) -> Category {
        let base: Vec<Tw> = w.base_tokens.iter().zip(&w.base_weights).map(|(t, &weight)| Tw { id: ids.put(t), weight }).collect();
        let mut common: Vec<Tw> = w.common_indexes.iter().zip(&w.common_weights).map(|(&i, &weight)| Tw { id: base[i as usize].id, weight }).collect();
        common.sort_by_key(|t| t.id);
        Category {
            base_weight: weight_sum(&base),
            base,
            base_unfiltered_len: w.base_unfiltered_len,
            max_unfiltered_len: w.max_unfiltered_len,
            ord_begin: w.ord_begin,
            ord_end: w.ord_end,
            common_weight: weight_sum(&common),
            common,
            orig_unique_weight: w.orig_unique_weight,
            num_matches: w.num_matches,
            bucket_ord: None,
            sub_aggs: Vec::new(),
        }
    }

    fn is_common(&self, t: Tw) -> bool {
        self.common.binary_search_by_key(&t.id, |c| c.id).is_ok()
    }

    fn max_matching_len(&self) -> i32 {
        max_matching_string_len(self.base_unfiltered_len, self.max_unfiltered_len, self.common.len())
    }

    fn add_string(&mut self, unfiltered_len: i32, tokens: &[Tw], unique: &[Tw], num_matches: u64) {
        self.update_common(unique);
        self.update_ordered_common(tokens, 0, tokens.len());
        if unfiltered_len > self.max_unfiltered_len {
            self.max_unfiltered_len = unfiltered_len;
        }
        self.num_matches += num_matches;
    }

    /// `updateCommonUniqueTokenIds`: keeps the common tokens `unique` also has.
    fn update_common(&mut self, unique: &[Tw]) {
        self.common_weight = 0;
        let initial = self.common.len();
        let (mut ci, mut ni, mut out) = (0, 0, 0);
        while ci < initial {
            if ni >= unique.len() {
                ci += 1;
                continue;
            }
            let c = self.common[ci];
            match c.id.cmp(&unique[ni].id) {
                Ordering::Less => {
                    ci += 1;
                    continue;
                }
                Ordering::Equal => {
                    self.common[out] = c;
                    out += 1;
                    self.common_weight += c.weight as i32;
                    ci += 1;
                }
                Ordering::Greater => {}
            }
            ni += 1;
        }
        self.common.truncate(out);
    }

    /// `updateOrderedCommonTokenIds`: the heaviest run of base tokens (common ones) that
    /// appears in order in the new tokens.
    fn update_ordered_common(&mut self, new: &[Tw], new_begin: usize, new_end: usize) {
        while self.ord_end > self.ord_begin && !self.is_common(self.base[self.ord_end - 1]) {
            self.ord_end -= 1;
        }
        while self.ord_begin < self.ord_end && !self.is_common(self.base[self.ord_begin]) {
            self.ord_begin += 1;
        }
        let mut best_begin = self.ord_end;
        let mut best_end = self.ord_end;
        let mut best_weight = 0u32;
        for try_begin in self.ord_begin..self.ord_end {
            let mut ni = new_begin;
            let mut try_weight = 0u32;
            for ci in try_begin..self.ord_end {
                if !self.is_common(self.base[ci]) {
                    continue;
                }
                while ni < new_end {
                    let b = self.base[ci];
                    if new[ni].id != b.id {
                        ni += 1;
                    } else {
                        try_weight += b.weight;
                        break;
                    }
                }
                if ni >= new_end {
                    if try_weight > best_weight {
                        best_weight = try_weight;
                        best_begin = try_begin;
                        best_end = ci;
                    }
                    break;
                }
            }
            if ni < new_end {
                if try_weight > best_weight {
                    best_begin = try_begin;
                    best_end = self.ord_end;
                }
                break;
            }
        }
        self.ord_begin = best_begin;
        self.ord_end = best_end;
    }

    fn missing_common_weight(&self, unique: &[Tw]) -> i32 {
        let (mut present, mut ci, mut ti) = (0i32, 0, 0);
        while ci < self.common.len() && ti < unique.len() {
            let c = self.common[ci];
            match c.id.cmp(&unique[ti].id) {
                Ordering::Less => {
                    ci += 1;
                    continue;
                }
                Ordering::Equal => {
                    present += c.weight as i32;
                    ci += 1;
                }
                Ordering::Greater => {}
            }
            ti += 1;
        }
        self.common_weight - present
    }

    fn is_missing_common_weight_zero(&self, unique: &[Tw]) -> bool {
        let mut ti = 0;
        for c in &self.common {
            if ti >= unique.len() {
                return false;
            }
            while unique[ti].id < c.id {
                ti += 1;
                if ti >= unique.len() {
                    return false;
                }
            }
            if unique[ti].id != c.id {
                return false;
            }
            ti += 1;
        }
        true
    }

    fn contains_common_in_order(&self, tokens: &[Tw]) -> bool {
        let mut ti = 0;
        for i in self.ord_begin..self.ord_end {
            let b = self.base[i];
            if !self.is_common(b) {
                continue;
            }
            loop {
                if ti >= tokens.len() {
                    return false;
                }
                let t = tokens[ti];
                ti += 1;
                if t.id == b.id {
                    break;
                }
            }
        }
        true
    }

    fn matches_search(&self, other_weight: i32, other_unfiltered_len: i32, other_unique: &[Tw], other_tokens: &[Tw]) -> bool {
        (self.base_weight == 0) == (other_weight == 0)
            && self.max_matching_len() >= other_unfiltered_len
            && self.is_missing_common_weight_zero(other_unique)
            && self.contains_common_in_order(other_tokens)
    }

    fn ram_bytes(&self) -> usize {
        96 + (self.base.len() + self.common.len()) * 24
    }
}

/// `TokenListCategory.maxMatchingStringLen`.
fn max_matching_string_len(base_unfiltered_len: i32, max_unfiltered_len: i32, common: usize) -> i32 {
    let extended = ((max_unfiltered_len * 11) / 10).min((base_unfiltered_len as f32 * (common as f32 / 1.5f32).max(2.0f32)) as i32);
    max_unfiltered_len.max(extended)
}

/// `TokenListSimilarityTester.weightedEditDistance`.
fn weighted_edit_distance(first: &[Tw], second: &[Tw], cur: &mut Vec<i32>, prev: &mut Vec<i32>) -> i32 {
    if first.is_empty() {
        return weight_sum(second);
    }
    if second.is_empty() {
        return weight_sum(first);
    }
    let n = second.len();
    cur.clear();
    cur.resize(n + 1, 0);
    prev.clear();
    prev.resize(n + 1, 0);
    for d in 0..n {
        cur[d + 1] = cur[d] + second[d].weight as i32;
    }
    for f in first {
        std::mem::swap(cur, prev);
        let fc = f.weight as i32;
        cur[0] = prev[0] + fc;
        for d in 0..n {
            let s = second[d];
            let sc = s.weight as i32;
            let o1 = prev[d + 1] + fc;
            let o2 = cur[d] + sc;
            let o3 = prev[d] + if f.id == s.id { 0 } else { fc.max(sc) };
            cur[d + 1] = o1.min(o2).min(o3);
        }
    }
    cur[n]
}

/// `TokenListCategorizer`.
#[derive(Clone, Debug)]
struct Categorizer {
    lower: f32,
    upper: f32,
    /// By id (creation order).
    cats: Vec<Category>,
    /// Ids, most matches first (`categoriesByNumMatches`).
    by_num: Vec<usize>,
    cur: Vec<i32>,
    prev: Vec<i32>,
}

impl Categorizer {
    fn new(threshold: f32) -> Categorizer {
        Categorizer { lower: threshold, upper: (1.0f32 + threshold) / 2.0f32, cats: Vec::new(), by_num: Vec::new(), cur: Vec::new(), prev: Vec::new() }
    }

    fn min_weight(weight: i32, threshold: f32) -> i32 {
        if weight == 0 {
            return 0;
        }
        ((weight as f32 * threshold + EPSILON) as f64).floor() as i32 + 1
    }

    fn max_weight(weight: i32, threshold: f32) -> i32 {
        if weight == 0 {
            return 0;
        }
        ((weight as f32 / threshold - EPSILON) as f64).ceil() as i32 - 1
    }

    fn similarity(&mut self, left: &[Tw], left_weight: i32, right: &[Tw], right_weight: i32) -> f32 {
        let max = left_weight.max(right_weight);
        if max > 0 { 1.0f32 - weighted_edit_distance(left, right, &mut self.cur, &mut self.prev) as f32 / max as f32 } else { 1.0f32 }
    }

    /// `computeCategory(List<TokenAndWeight>, int, long)`: the category of a weighted token list.
    fn categorize_tokens(&mut self, tokens: &[Tw], unfiltered_len: i32, num_docs: u64) -> usize {
        let mut work_weight = 0i32;
        let (mut min_rw, mut max_rw) = (0i32, 0i32);
        let mut grouping: std::collections::BTreeMap<u32, u32> = std::collections::BTreeMap::new();
        for t in tokens {
            work_weight += t.weight as i32;
            min_rw += WeightCalculator::min_matching_weight(t.weight as i32);
            max_rw += WeightCalculator::max_matching_weight(t.weight as i32);
            *grouping.entry(t.id).or_insert(0) += t.weight;
        }
        let unique: Vec<Tw> = grouping.into_iter().map(|(id, weight)| Tw { id, weight }).collect();
        self.compute(tokens, &unique, work_weight, min_rw, max_rw, unfiltered_len, unfiltered_len, num_docs)
    }

    /// `mergeWireCategory`.
    fn merge_wire(&mut self, w: &WireCategory, ids: &mut TokenIds) -> usize {
        let foreign = Category::from_wire(w, ids);
        self.compute(
            &foreign.base,
            &foreign.common,
            foreign.base_weight,
            WeightCalculator::min_matching_weight(foreign.base_weight),
            WeightCalculator::max_matching_weight(foreign.base_weight),
            foreign.base_unfiltered_len,
            foreign.max_unfiltered_len,
            foreign.num_matches,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn compute(&mut self, tokens: &[Tw], unique: &[Tw], work_weight: i32, min_rw: i32, max_rw: i32, unfiltered_len: i32, max_unfiltered_len: i32, num_docs: u64) -> usize {
        let mut min_weight = Self::min_weight(min_rw, self.lower);
        let mut max_weight = Self::max_weight(max_rw, self.lower);
        let mut best: Option<usize> = None;
        let mut best_similarity = self.lower;
        for index in 0..self.by_num.len() {
            let cid = self.by_num[index];
            let c = &self.cats[cid];
            let base_weight = c.base_weight;
            let matches_search = c.matches_search(work_weight, max_unfiltered_len, unique, tokens);
            if !matches_search {
                if base_weight < min_weight || base_weight > max_weight {
                    continue;
                }
                let missing = c.missing_common_weight(unique);
                if missing > 0 {
                    let proportion = (c.common_weight - missing) as f32 / c.orig_unique_weight as f32;
                    if proportion < self.lower {
                        continue;
                    }
                }
            }
            let base = std::mem::take(&mut self.cats[cid].base);
            let similarity = self.similarity(tokens, work_weight, &base, base_weight);
            self.cats[cid].base = base;
            if matches_search || similarity > self.upper {
                return self.add_match(max_unfiltered_len, tokens, unique, num_docs, index);
            }
            if similarity > best_similarity {
                best = Some(index);
                best_similarity = similarity;
                min_weight = Self::min_weight(min_rw, similarity);
                max_weight = Self::max_weight(max_rw, similarity);
            }
        }
        if let Some(index) = best {
            return self.add_match(max_unfiltered_len, tokens, unique, num_docs, index);
        }
        let id = self.cats.len();
        self.cats.push(Category::new(unfiltered_len, tokens, unique, max_unfiltered_len, num_docs));
        self.by_num.push(id);
        self.reposition(id, self.by_num.len() - 1);
        id
    }

    fn add_match(&mut self, unfiltered_len: i32, tokens: &[Tw], unique: &[Tw], num_docs: u64, index: usize) -> usize {
        let cid = self.by_num[index];
        self.cats[cid].add_string(unfiltered_len, tokens, unique, num_docs);
        if num_docs == 1 {
            self.reposition(cid, index);
        } else {
            let cats = &self.cats;
            self.by_num.sort_by(|&a, &b| cats[b].num_matches.cmp(&cats[a].num_matches));
        }
        cid
    }

    /// `repositionCategory`: moves the category up past those with fewer matches (one swap).
    fn reposition(&mut self, cid: usize, current: usize) {
        let n = self.cats[cid].num_matches;
        let mut swap = current;
        while swap > 0 {
            swap -= 1;
            if n <= self.cats[self.by_num[swap]].num_matches {
                swap += 1;
                break;
            }
        }
        if swap != current {
            self.by_num.swap(current, swap);
        }
    }

    fn ram_bytes(&self) -> usize {
        64 + self.cats.iter().map(Category::ram_bytes).sum::<usize>()
    }
}

/// `SerializableTokenListCategory`: a category with its token bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct WireCategory {
    pub base_tokens: Vec<Box<[u8]>>,
    pub base_weights: Vec<u32>,
    pub base_unfiltered_len: i32,
    pub max_unfiltered_len: i32,
    pub ord_begin: usize,
    pub ord_end: usize,
    pub common_indexes: Vec<u32>,
    pub common_weights: Vec<u32>,
    pub key_indexes: Vec<u32>,
    pub orig_unique_weight: i32,
    pub num_matches: u64,
}

impl WireCategory {
    fn of(c: &Category, ids: &TokenIds) -> WireCategory {
        let base_tokens: Vec<Box<[u8]>> = c.base.iter().map(|t| ids.tokens[t.id as usize].clone()).collect();
        let first_index = |id: u32| c.base.iter().position(|t| t.id == id).expect("base token") as u32;
        let mut key_indexes = Vec::new();
        let mut budget = KEY_BUDGET as i64 + 1;
        for t in &c.base {
            if !c.is_common(*t) {
                continue;
            }
            let index = first_index(t.id);
            budget -= 1;
            let len = base_tokens[index as usize].len() as i64;
            if len > budget {
                break;
            }
            budget -= len;
            key_indexes.push(index);
        }
        WireCategory {
            base_weights: c.base.iter().map(|t| t.weight).collect(),
            base_unfiltered_len: c.base_unfiltered_len,
            max_unfiltered_len: c.max_unfiltered_len,
            ord_begin: c.ord_begin,
            ord_end: c.ord_end,
            common_indexes: c.common.iter().map(|t| first_index(t.id)).collect(),
            common_weights: c.common.iter().map(|t| t.weight).collect(),
            key_indexes,
            orig_unique_weight: c.orig_unique_weight,
            num_matches: c.num_matches,
            base_tokens,
        }
    }

    fn key_tokens(&self) -> impl Iterator<Item = &[u8]> {
        self.key_indexes.iter().map(|&i| &*self.base_tokens[i as usize])
    }

    /// The bucket key: key tokens joined by spaces.
    pub fn key(&self) -> String {
        self.key_tokens().map(|t| String::from_utf8_lossy(t).into_owned()).collect::<Vec<_>>().join(" ")
    }

    /// `getRegex`.
    pub fn regex(&self) -> String {
        if self.key_indexes.is_empty() || self.ord_begin == self.ord_end {
            return ".*".to_string();
        }
        let parts: Vec<String> = self
            .key_indexes
            .iter()
            .filter(|&&i| (i as usize) >= self.ord_begin && (i as usize) < self.ord_end)
            .map(|&i| {
                let mut s = String::new();
                for c in String::from_utf8_lossy(&self.base_tokens[i as usize]).chars() {
                    if "\\|()[]{}^$.+*?".contains(c) {
                        s.push('\\');
                    }
                    s.push(c);
                }
                s
            })
            .collect();
        format!(".*?{}.*?", parts.join(".+?"))
    }

    pub fn max_matching_length(&self) -> i32 {
        max_matching_string_len(self.base_unfiltered_len, self.max_unfiltered_len, self.common_indexes.len())
    }

    fn key_cmp(&self, other: &WireCategory) -> Ordering {
        self.key_tokens().cmp(other.key_tokens())
    }
}

/// A parsed `categorize_text`.
#[derive(Clone, Debug)]
pub struct CategorizeTextSpec {
    pub field: Option<String>,
    pub similarity_threshold: i32,
    pub analyzer: AnalyzerConfig,
    pub size: usize,
    /// `None`: `BucketUtils.suggestShardSideQueueSize(size)`.
    pub shard_size: Option<usize>,
    pub min_doc_count: u64,
    pub shard_min_doc_count: u64,
}

fn status(msg: impl Into<String>) -> AggError {
    AggError::new(400, "status_exception", msg)
}

/// `CategorizeTextAggregationBuilder.PARSER` (setters validate as fields arrive).
pub fn parse(p: &mut XParser<'_>, name: &str, regex_ok: &dyn Fn(&str) -> bool) -> Result<CategorizeTextSpec> {
    let fields = [
        Field::new("field", Tokens::STRING),
        Field::new("similarity_threshold", Tokens::INT),
        Field::new("max_unique_tokens", Tokens::INT),
        Field::new("max_matched_tokens", Tokens::INT),
        Field::new("categorization_analyzer", Tokens::OBJECT_OR_STRING),
        Field::new("categorization_filters", Tokens::STRING_ARRAY),
        Field::new("shard_size", Tokens::INT),
        Field::new("min_doc_count", Tokens::LONG),
        Field::new("shard_min_doc_count", Tokens::LONG),
        Field::new("size", Tokens::INT),
    ];
    let mut spec = CategorizeTextSpec {
        field: None,
        similarity_threshold: 70,
        analyzer: AnalyzerConfig::Standard,
        size: 10,
        shard_size: None,
        min_doc_count: 1,
        shard_min_doc_count: 0,
    };
    let mut analyzer_set = false;
    let both = || status("[categorization_filters] cannot be used with [categorization_analyzer] - instead specify them as pattern_replace char_filters in the analyzer");
    p.object("categorize_text", &fields, |p, i| {
        match i {
            0 => spec.field = Some(p.text()?),
            1 => {
                let v = p.int_value()?;
                if !(1..=100).contains(&v) {
                    return Err(status(format!("[similarity_threshold] must be in the range [1, 100]. Found [{v}] in [{name}]")));
                }
                spec.similarity_threshold = v;
            }
            2 | 3 => {
                p.int_value()?;
                let (l, c) = p.token_location();
                let field = if i == 2 { "max_unique_tokens" } else { "max_matched_tokens" };
                p.warnings.push(format!("[categorize_text][{l}:{c}] Deprecated field [{field}] used, this field is unused and will be removed entirely"));
            }
            4 => {
                let config = if p.token() == Token::String {
                    Json::str(p.text()?)
                } else {
                    let v = p.take_value();
                    if v.get("analyzer").is_none() && v.get("tokenizer").is_none() {
                        return Err(AggError::illegal_argument("categorization_analyzer that is not a global analyzer must specify a [tokenizer] field"));
                    }
                    v
                };
                if analyzer_set {
                    return Err(both());
                }
                analyzer_set = true;
                spec.analyzer = AnalyzerConfig::Custom(config);
            }
            5 => {
                let filters: Vec<String> = if p.token() == Token::String {
                    vec![p.text()?]
                } else {
                    p.array_items(|p| p.text())?
                };
                if filters.is_empty() {
                    return Ok(());
                }
                if analyzer_set {
                    return Err(both());
                }
                let distinct = filters.iter().collect::<hashbrown::HashSet<&String>>().len();
                if distinct != filters.len() {
                    return Err(status("categorization_filters contain duplicates"));
                }
                if filters.iter().any(String::is_empty) {
                    return Err(status("categorization_filters are not allowed to contain empty strings"));
                }
                if let Some(bad) = filters.iter().find(|f| !regex_ok(f)) {
                    return Err(status(format!("categorization_filters contains invalid regular expression '{bad}'")));
                }
                analyzer_set = true;
                spec.analyzer = AnalyzerConfig::StandardWithFilters(filters);
            }
            6 => {
                let v = p.int_value()?;
                if v <= 0 {
                    return Err(status(format!("[shard_size] must be greater than 0. Found [{v}] in [{name}]")));
                }
                spec.shard_size = Some(v as usize);
            }
            7 => {
                let v = p.long_value()?;
                if v < 0 {
                    return Err(status(format!("[min_doc_count] must be greater than or equal to 0. Found [{v}] in [{name}]")));
                }
                spec.min_doc_count = v as u64;
            }
            8 => {
                let v = p.long_value()?;
                if v < 0 {
                    return Err(status(format!("[shard_min_doc_count] must be greater than or equal to 0. Found [{v}] in [{name}]")));
                }
                spec.shard_min_doc_count = v as u64;
            }
            _ => {
                let v = p.int_value()?;
                if v <= 0 {
                    return Err(status(format!("[size] must be greater than 0. Found [{v}] in [{name}]")));
                }
                spec.size = v as usize;
            }
        }
        Ok(())
    })?;
    Ok(spec)
}

/// A light syntax check of a Java regex (balanced groups and classes, no dangling
/// quantifier), used when the embedder gives no Java regex validator.
pub fn basic_regex_ok(re: &str) -> bool {
    let mut depth = 0i32;
    let mut chars = re.chars().peekable();
    let mut in_class = false;
    let mut prev_atom = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if chars.next().is_none() {
                    return false;
                }
                prev_atom = true;
            }
            '[' if !in_class => {
                in_class = true;
                if chars.peek() == Some(&'^') {
                    chars.next();
                }
                if chars.peek() == Some(&']') {
                    chars.next();
                }
            }
            ']' if in_class => {
                in_class = false;
                prev_atom = true;
            }
            _ if in_class => {}
            '(' => {
                depth += 1;
                prev_atom = false;
                if chars.peek() == Some(&'?') {
                    chars.next();
                }
            }
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
                prev_atom = true;
            }
            '*' | '+' | '?' => {
                if !prev_atom {
                    return false;
                }
                if matches!(chars.peek(), Some('?') | Some('+')) {
                    chars.next();
                }
                prev_atom = false;
            }
            '|' => prev_atom = false,
            _ => prev_atom = true,
        }
    }
    depth == 0 && !in_class
}

/// Mapped field types `categorize_text` accepts (a `TextSearchInfo` with terms).
fn analyzable(type_name: &str) -> bool {
    matches!(type_name, "text" | "keyword" | "match_only_text" | "wildcard" | "constant_keyword" | "search_as_you_type" | "annotated_text" | "flattened" | "version")
}

/// `fieldType.getClass().getSimpleName()` of refused types.
fn field_type_class(type_name: &str) -> String {
    match type_name {
        "long" | "integer" | "short" | "byte" | "double" | "float" | "half_float" => "NumberFieldType".to_string(),
        "scaled_float" => "ScaledFloatFieldType".to_string(),
        "unsigned_long" => "UnsignedLongFieldType".to_string(),
        "date" | "date_nanos" => "DateFieldType".to_string(),
        "boolean" => "BooleanFieldType".to_string(),
        "ip" => "IpFieldType".to_string(),
        "geo_point" => "GeoPointFieldType".to_string(),
        "geo_shape" => "GeoShapeWithDocValuesFieldType".to_string(),
        "binary" => "BinaryFieldType".to_string(),
        t => {
            let camel: String = t.split('_').map(|w| w[..1].to_uppercase() + &w[1..]).collect();
            format!("{camel}FieldType")
        }
    }
}

struct CategorizeAgg {
    field: String,
    analyzer: Arc<dyn CategorizationAnalyzer>,
    source: Arc<dyn TextTerms>,
    dict: Arc<PosDictionary>,
    threshold: i32,
    size: usize,
    shard_size: usize,
    min_doc_count: u64,
    ids: TokenIds,
    categorizers: Vec<Option<Categorizer>>,
    /// `(owning bucket, category id)` → bucket ordinal.
    ords: hashbrown::HashMap<(u32, u32), u32>,
    subs: Subs,
    doc_count_field: Option<u32>,
    doc_counter: DocCounter,
    seg: Option<SegmentContext>,
    tokens: Vec<String>,
    weighted: Vec<Tw>,
    tracker: MemTracker,
}

/// `CategorizeTextAggregatorFactory.createInternal` + `CategorizeTextAggregator`.
pub fn create(def: &AggDef, spec: &CategorizeTextSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let Some(field) = &spec.field else {
        return Err(AggError::new(500, "null_pointer_exception", "Cannot invoke \"Object.hashCode()\" because \"pk\" is null"));
    };
    let empty = || -> Box<dyn Aggregator> {
        Box::new(EmptyCategorize { proto: CategorizeText { buckets: Vec::new(), required_size: spec.size, min_doc_count: spec.min_doc_count, threshold: spec.similarity_threshold } })
    };
    let Some(f) = ctx.lookup.field(field)? else { return Ok(empty()) };
    if !analyzable(&f.type_name) {
        return Err(AggError::illegal_argument(format!(
            "categorize_text agg [{}] only works on text and keyword fields. Cannot aggregate field type [{}] via [{}]",
            def.name,
            f.name,
            field_type_class(&f.type_name)
        )));
    }
    let analyzer: Arc<dyn CategorizationAnalyzer> = match &spec.analyzer {
        AnalyzerConfig::Standard => Arc::new(StandardAnalyzer),
        other => ctx.lookup.categorization_analyzer(&other.to_json())?,
    };
    let source = ctx.lookup.text_terms().ok_or_else(|| AggError::illegal_argument("[categorize_text] needs the index's _source"))?;
    let mut shard_size = spec.shard_size.unwrap_or_else(|| crate::bucket::terms::suggest_shard_size(spec.size));
    if shard_size < spec.size {
        shard_size = spec.size;
    }
    let subs = Subs::bind(def, ctx)?;
    Ok(Box::new(CategorizeAgg {
        field: f.name.clone(),
        analyzer,
        source,
        dict: dictionary(),
        threshold: spec.similarity_threshold,
        size: spec.size,
        shard_size,
        min_doc_count: spec.min_doc_count,
        ids: TokenIds::default(),
        categorizers: Vec::new(),
        ords: hashbrown::HashMap::new(),
        subs,
        doc_count_field: ctx.doc_count_field,
        doc_counter: DocCounter::none(),
        seg: None,
        tokens: Vec::new(),
        weighted: Vec::new(),
        tracker: ctx.tracker(def),
    }))
}

impl CategorizeAgg {
    fn categorizer(&mut self, owning: u32) -> &mut Categorizer {
        let o = owning as usize;
        if self.categorizers.len() <= o {
            self.categorizers.resize_with(o + 1, || None);
        }
        let t = self.threshold as f32 / 100.0f32;
        self.categorizers[o].get_or_insert_with(|| Categorizer::new(t))
    }

    fn bytes(&self) -> usize {
        self.ids.tokens.iter().map(|t| t.len() + 48).sum::<usize>() + self.categorizers.iter().flatten().map(Categorizer::ram_bytes).sum::<usize>() + self.ords.len() * 16
    }
}

impl Aggregator for CategorizeAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.doc_counter = DocCounter::open(seg, self.doc_count_field);
        self.seg = Some(seg.clone());
        self.subs.set_segment(seg)
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let Some(seg) = self.seg.clone() else { return Ok(()) };
        for i in 0..b.len() {
            let (doc, owning, score) = (b.docs[i], b.ords[i], b.score(i));
            self.categorizer(owning);
            let num_docs = self.doc_counter.count(doc);
            for text in self.source.source_strings(&seg, doc, &self.field)? {
                if num_docs == 0 {
                    continue;
                }
                self.tokens.clear();
                self.analyzer.analyze(&text, &mut self.tokens)?;
                self.weighted.clear();
                let mut wc = WeightCalculator { dict: &self.dict, consecutive: 0 };
                for t in &self.tokens {
                    if self.weighted.len() >= MAX_TOKENS {
                        break;
                    }
                    if t.is_empty() {
                        continue;
                    }
                    let weight = wc.weight(t);
                    self.weighted.push(Tw { id: self.ids.put(t.as_bytes()), weight });
                }
                if self.weighted.is_empty() {
                    continue;
                }
                let unfiltered = text.encode_utf16().count() as i32;
                let weighted = std::mem::take(&mut self.weighted);
                let cat = self.categorizer(owning).categorize_tokens(&weighted, unfiltered, num_docs) as u32;
                self.weighted = weighted;
                let next = self.ords.len() as u32;
                let ord = *self.ords.entry((owning, cat)).or_insert(next);
                if ord == next {
                    self.categorizer(owning).cats[cat as usize].bucket_ord = Some(ord);
                }
                self.subs.push(doc, ord, score)?;
            }
        }
        let bytes = self.bytes();
        self.tracker.ensure(bytes)
    }

    fn finish_segment(&mut self) -> Result<()> {
        self.subs.finish_segment()
    }

    fn post_collection(&mut self) -> Result<()> {
        self.subs.post_collection()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let mut per: Vec<Vec<(WireCategory, u32)>> = Vec::with_capacity(owning.len());
        let mut all = Vec::new();
        for &o in owning {
            let list: Vec<(WireCategory, u32)> = match self.categorizers.get(o as usize).and_then(|c| c.as_ref()) {
                None => Vec::new(),
                Some(c) => c
                    .by_num
                    .iter()
                    .take(self.shard_size.min(c.cats.len()))
                    .map(|&cid| (WireCategory::of(&c.cats[cid], &self.ids), c.cats[cid].bucket_ord.expect("collected")))
                    .collect(),
            };
            all.extend(list.iter().map(|(_, ord)| *ord));
            per.push(list);
        }
        let mut sub_aggs = self.subs.build(&all)?.into_iter();
        Ok(per
            .into_iter()
            .map(|list| {
                let buckets = list.into_iter().map(|(cat, _)| CatBucket { cat, aggs: sub_aggs.next().unwrap_or_default() }).collect();
                AggValue::CategorizeText(Box::new(CategorizeText { buckets, required_size: self.size, min_doc_count: self.min_doc_count, threshold: self.threshold }))
            })
            .collect())
    }

    fn build_empty(&self) -> AggValue {
        AggValue::CategorizeText(Box::new(CategorizeText { buckets: Vec::new(), required_size: self.size, min_doc_count: self.min_doc_count, threshold: self.threshold }))
    }

    fn needs_scores(&self) -> bool {
        self.subs.needs_scores()
    }

    fn validate_sort(&self, name: &str, _key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        Err(crate::collect::unsortable(name, rest))
    }
}

/// An unmapped field (`UnmappedCategorizationAggregation`).
struct EmptyCategorize {
    proto: CategorizeText,
}

impl Aggregator for EmptyCategorize {
    fn set_segment(&mut self, _seg: &SegmentContext) -> Result<()> {
        Ok(())
    }

    fn collect(&mut self, _b: Batch<'_>) -> Result<()> {
        Ok(())
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        Ok(owning.iter().map(|_| self.build_empty()).collect())
    }

    fn build_empty(&self) -> AggValue {
        AggValue::CategorizeText(Box::new(self.proto.clone()))
    }

    fn needs_scores(&self) -> bool {
        false
    }

    fn validate_sort(&self, name: &str, _key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        Err(crate::collect::unsortable(name, rest))
    }
}

/// One category bucket.
#[derive(Clone, Debug)]
pub struct CatBucket {
    pub cat: WireCategory,
    pub aggs: Aggs,
}

/// `InternalCategorizationAggregation`.
#[derive(Clone, Debug)]
pub struct CategorizeText {
    pub buckets: Vec<CatBucket>,
    pub required_size: usize,
    pub min_doc_count: u64,
    pub threshold: i32,
}

impl Internal for CategorizeText {
    fn type_name(&self) -> &'static str {
        "categorize_text"
    }

    fn class_name(&self) -> &'static str {
        "InternalCategorizationAggregation"
    }

    fn must_reduce_single(&self) -> bool {
        true
    }

    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let first = &list[0];
        let (required_size, min_doc_count, threshold) = (first.required_size, first.min_doc_count, first.threshold);
        let mut ids = TokenIds::default();
        let mut categorizer = Categorizer::new(threshold as f32 / 100.0f32);
        for part in list {
            for b in part.buckets {
                let cid = categorizer.merge_wire(&b.cat, &mut ids);
                categorizer.cats[cid].sub_aggs.push(b.aggs);
            }
        }
        let is_final = ctx.is_final();
        let size = if is_final { required_size.min(categorizer.cats.len()) } else { categorizer.cats.len() };
        let min = if is_final { min_doc_count } else { 0 };
        let mut buckets = Vec::new();
        for &cid in categorizer.by_num.iter().take(size) {
            if categorizer.cats[cid].num_matches < min {
                break;
            }
            let subs = std::mem::take(&mut categorizer.cats[cid].sub_aggs);
            let aggs = if subs.is_empty() { Aggs::default() } else { Aggs::reduce_lists(subs, ctx)? };
            buckets.push(CatBucket { cat: WireCategory::of(&categorizer.cats[cid], &ids), aggs });
        }
        ctx.consume_buckets(buckets.len())?;
        if is_final {
            buckets.sort_by(|a, b| b.cat.num_matches.cmp(&a.cat.num_matches).then_with(|| a.cat.key_cmp(&b.cat)));
        }
        Ok(CategorizeText { buckets, required_size, min_doc_count, threshold })
    }

    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        let buckets = self
            .buckets
            .iter()
            .map(|b| {
                let mut o = Json::obj()
                    .with("doc_count", Json::from(b.cat.num_matches))
                    .with("key", Json::Str(b.cat.key()))
                    .with("regex", Json::Str(b.cat.regex()))
                    .with("max_matching_length", Json::Int(b.cat.max_matching_length() as i64));
                b.aggs.render_into(&mut o, rc);
                o
            })
            .collect();
        out.push("buckets", Json::Arr(buckets));
    }

    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_bucket_property(self, name, path)
    }

    fn ram_bytes(&self) -> usize {
        64 + self.buckets.iter().map(|b| 128 + b.cat.base_tokens.iter().map(|t| t.len() + 24).sum::<usize>() + b.aggs.ram_bytes()).sum::<usize>()
    }
}

impl MultiBuckets for CategorizeText {
    fn len(&self) -> usize {
        self.buckets.len()
    }
    fn doc_count(&self, i: usize) -> u64 {
        self.buckets[i].cat.num_matches
    }
    fn key(&self, i: usize) -> Json {
        Json::Str(self.buckets[i].cat.key())
    }
    fn key_string(&self, i: usize) -> String {
        self.buckets[i].cat.key()
    }
    fn aggs(&self, i: usize) -> &Aggs {
        &self.buckets[i].aggs
    }
    fn aggs_mut(&mut self, i: usize) -> &mut Aggs {
        &mut self.buckets[i].aggs
    }
    fn retain_buckets(&mut self, keep: &[bool]) {
        let mut i = 0;
        self.buckets.retain(|_| {
            let k = keep[i];
            i += 1;
            k
        });
    }
    fn reorder(&mut self, order: &[usize]) {
        let mut slots: Vec<Option<CatBucket>> = self.buckets.drain(..).map(Some).collect();
        self.buckets = order.iter().map(|&i| slots[i].take().expect("bucket")).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        standard_tokens(s, &mut out);
        out
    }

    /// Token lists from Elasticsearch 9.5.5's `_analyze` with `first_line_with_letters`,
    /// `ml_standard` and a stop filter.
    #[test]
    fn ml_standard_tokenizer_matches_elasticsearch() {
        let cases: [(&str, &[&str]); 18] = [
            ("Node 2 stopping", &["Node", "stopping"]),
            ("deadbeef 0x1F 12:30 Monday May dog", &["dog"]),
            ("a.b-c@d_ e.f. --g", &["a.b-c@d", "g"]),
            ("C:\\Windows\\system32 /usr/local/bin http://x.y/z", &["C:\\Windows\\system32", "/usr/local/bin", "http://x.y/z"]),
            ("  \n\n  first line\nsecond", &["first", "line"]),
            ("1234 \n 5678", &[]),
            ("ab:cd ef", &[]),
            ("12:ab foo", &["foo"]),
            ("x:\\y z", &["x:\\y", "z"]),
            ("user@host.com:8080/path", &["user@host.com"]),
            ("//", &[]),
            ("/a/b/", &["/a/b/"]),
            ("\\\\server\\share", &["\\\\server\\share"]),
            ("über naïve café 中文 العربية", &["über", "naïve", "café", "中文", "العربية"]),
            ("abc\u{93e} def", &[]),
            ("emoji 😀 here", &["emoji", "here"]),
            ("_lead trail_ -x", &["lead", "trail", "x"]),
            ("a:b:c d", &[]),
        ];
        for (text, want) in cases {
            assert_eq!(toks(text), want, "{text:?}");
        }
    }

    #[test]
    fn first_line_with_letters_keeps_one_line() {
        let u = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        assert_eq!(first_line_with_letters(&u("12\n34\r\nab c\r\nd")), u("ab c").as_slice());
        assert_eq!(first_line_with_letters(&u("123")), &[] as &[u16]);
        assert_eq!(first_line_with_letters(&u("x")), u("x").as_slice());
    }

    #[test]
    fn edit_distance_and_lengths() {
        let tw = |id, weight| Tw { id, weight };
        let (mut a, mut b) = (Vec::new(), Vec::new());
        assert_eq!(weighted_edit_distance(&[tw(1, 3), tw(2, 1)], &[tw(1, 3), tw(3, 2)], &mut a, &mut b), 2);
        assert_eq!(weighted_edit_distance(&[], &[tw(1, 3)], &mut a, &mut b), 3);
        assert_eq!(max_matching_string_len(15, 15, 2), 16);
        assert_eq!(Categorizer::min_weight(10, 0.7), 8);
        assert_eq!(Categorizer::max_weight(10, 0.7), 14);
    }

    #[test]
    fn regex_escapes_and_keys() {
        let w = WireCategory {
            base_tokens: vec![b"a.b".to_vec().into(), b"x".to_vec().into(), b"c(d)".to_vec().into()],
            base_weights: vec![1, 1, 1],
            base_unfiltered_len: 10,
            max_unfiltered_len: 12,
            ord_begin: 0,
            ord_end: 3,
            common_indexes: vec![0, 2],
            common_weights: vec![1, 1],
            key_indexes: vec![0, 2],
            orig_unique_weight: 3,
            num_matches: 2,
        };
        assert_eq!(w.key(), "a.b c(d)");
        assert_eq!(w.regex(), ".*?a\\.b.+?c\\(d\\).*?");
    }

    /// Elasticsearch 9.5.5's deprecation warnings (response `Warning` headers).
    #[test]
    fn deprecated_fields_warn() {
        let body = r#"{"aggs":{"f":{"categorize_text":{"field":"m","max_unique_tokens":3,"max_matched_tokens":2}},"g":{"frequent_items":{"fields":[{"field":"c"}]}}}}"#;
        let json = Json::parse(body).unwrap();
        let opts = crate::ParseOptions { src: Some(body.as_bytes()), base: vec![crate::xparser::Seg::Key("aggs".into())], ..Default::default() };
        let req = crate::parse(json.get("aggs").unwrap(), &opts).unwrap();
        assert_eq!(
            req.warnings,
            [
                "[categorize_text][1:66] Deprecated field [max_unique_tokens] used, this field is unused and will be removed entirely",
                "[categorize_text][1:89] Deprecated field [max_matched_tokens] used, this field is unused and will be removed entirely",
                "Deprecated field [frequent_items] used, expected [frequent_item_sets] instead",
            ]
        );
    }

    #[test]
    fn regex_check() {
        assert!(basic_regex_ok("foo[0-9]+(bar)?"));
        assert!(!basic_regex_ok("(unclosed"));
        assert!(!basic_regex_ok("*x"));
        assert!(!basic_regex_ok("[abc"));
    }
}
