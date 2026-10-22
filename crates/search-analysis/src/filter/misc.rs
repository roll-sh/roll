//! Token filters with their own small algorithms: `cjk_width`, `cjk_bigram`, `min_hash`,
//! `pattern_capture`, `pattern_replace`, `stemmer_override`, `dictionary_decompounder`,
//! `hyphenation_decompounder` and `type_as_payload`.
//!
//! Each is a port of the Lucene filter Elasticsearch builds, keeping its observable details
//! (UTF-16 based lengths and offsets, attribute handling, output order); all keep their state in
//! buffers reused across tokens.

use std::collections::HashMap;
use std::sync::Arc;

use super::WordSet;
use crate::component::{FilterFactory, SynonymRole, filter_fn, filter_with, normalizing_fn};
use crate::error::{AnalysisError, Result};
use crate::jregex::{self, Captures, Finder, JavaRegex, Replacement};
use crate::registry::{word_list, word_set};
use crate::settings::Settings;
use crate::stream::{Filter, TokenStream};
use crate::token::{Tok, attr};
use crate::unicode::utf16_len;

// ---------------------------------------------------------------------------------------------
// cjk_width

/// Halfwidth katakana 0xFF65–0xFF9F to their fullwidth forms.
const KANA_NORM: [u16; 59] = [
    0x30fb, 0x30f2, 0x30a1, 0x30a3, 0x30a5, 0x30a7, 0x30a9, 0x30e3, 0x30e5, 0x30e7, 0x30c3, 0x30fc,
    0x30a2, 0x30a4, 0x30a6, 0x30a8, 0x30aa, 0x30ab, 0x30ad, 0x30af, 0x30b1, 0x30b3, 0x30b5, 0x30b7,
    0x30b9, 0x30bb, 0x30bd, 0x30bf, 0x30c1, 0x30c4, 0x30c6, 0x30c8, 0x30ca, 0x30cb, 0x30cc, 0x30cd,
    0x30ce, 0x30cf, 0x30d2, 0x30d5, 0x30d8, 0x30db, 0x30de, 0x30df, 0x30e0, 0x30e1, 0x30e2, 0x30e4,
    0x30e6, 0x30e8, 0x30e9, 0x30ea, 0x30eb, 0x30ec, 0x30ed, 0x30ef, 0x30f3, 0x3099, 0x309A,
];

/// Voiced-mark combinations for 0x30A6–0x30FD.
const KANA_COMBINE_VOICED: [u8; 88] = [
    78, 0, 0, 0, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 0, 1,
    0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 1, 0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 8, 8, 8, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
];

const KANA_COMBINE_HALF_VOICED: [u8; 88] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 2, 0, 0, 2, 0, 0, 2, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// `CJKWidthFilter`: fullwidth ASCII to basic Latin, halfwidth katakana to fullwidth (combining
/// a following (half-)voiced sound mark into the previous kana when possible).
pub(crate) fn cjk_width(t: &mut Tok, units: &mut Vec<u16>) {
    if t.term.is_ascii() {
        return;
    }
    units.clear();
    units.extend(t.term.encode_utf16());
    let mut changed = false;
    let mut i = 0;
    while i < units.len() {
        let ch = units[i];
        if (0xFF01..=0xFF5E).contains(&ch) {
            units[i] = ch - 0xFEE0;
            changed = true;
        } else if (0xFF65..=0xFF9F).contains(&ch) {
            changed = true;
            if (ch == 0xFF9E || ch == 0xFF9F) && i > 0 && combine(units, i, ch) {
                units.remove(i);
                continue;
            }
            units[i] = KANA_NORM[(ch - 0xFF65) as usize];
        }
        i += 1;
    }
    if changed {
        t.term.clear();
        t.term
            .extend(char::decode_utf16(units.iter().copied()).map(|r| r.unwrap_or('\u{FFFD}')));
    }
}

fn combine(units: &mut [u16], pos: usize, ch: u16) -> bool {
    let prev = units[pos - 1];
    if (0x30A6..=0x30FD).contains(&prev) {
        let idx = (prev - 0x30A6) as usize;
        let add = if ch == 0xFF9F {
            KANA_COMBINE_HALF_VOICED[idx]
        } else {
            KANA_COMBINE_VOICED[idx]
        };
        units[pos - 1] = prev + add as u16;
        return units[pos - 1] != prev;
    }
    false
}

pub(crate) fn cjk_width_factory() -> Arc<dyn FilterFactory> {
    normalizing_fn(|| {
        let mut units = Vec::new();
        Box::new(super::Map(move |t: &mut Tok| cjk_width(t, &mut units)))
    })
}

// ---------------------------------------------------------------------------------------------
// cjk_bigram

pub(crate) const HAN: u32 = 1;
pub(crate) const HIRAGANA: u32 = 2;
pub(crate) const KATAKANA: u32 = 4;
pub(crate) const HANGUL: u32 = 8;

/// `CJKBigramFilter`.
pub(crate) struct CjkBigram {
    flags: u32,
    unigrams: bool,
    ngram_state: bool,
    buffer: Vec<char>,
    starts: Vec<u32>,
    ends: Vec<u32>,
    index: usize,
    last_end: u32,
    exhausted: bool,
    lone: Tok,
    has_lone: bool,
}

impl CjkBigram {
    pub fn new(flags: u32, unigrams: bool) -> Self {
        CjkBigram {
            flags,
            unigrams,
            ngram_state: false,
            buffer: Vec::new(),
            starts: Vec::new(),
            ends: Vec::new(),
            index: 0,
            last_end: 0,
            exhausted: false,
            lone: Tok::default(),
            has_lone: false,
        }
    }

    fn is_cjk(&self, kind: &str) -> bool {
        use crate::tokenizer::standard::{
            HANGUL as T_HANGUL, HIRAGANA as T_HIRAGANA, IDEOGRAPHIC, KATAKANA as T_KATAKANA,
        };
        (self.flags & HAN != 0 && kind == IDEOGRAPHIC)
            || (self.flags & HIRAGANA != 0 && kind == T_HIRAGANA)
            || (self.flags & KATAKANA != 0 && kind == T_KATAKANA)
            || (self.flags & HANGUL != 0 && kind == T_HANGUL)
    }

    fn has_bigram(&self) -> bool {
        self.buffer.len() > self.index + 1
    }

    fn has_unigram(&self) -> bool {
        if self.unigrams {
            self.buffer.len() == self.index + 1
        } else {
            self.buffer.len() == 1 && self.index == 0
        }
    }

    fn do_next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if self.has_lone {
            t.clone_from(&self.lone);
            self.has_lone = false;
            return true;
        }
        if self.exhausted {
            return false;
        }
        if input.next(t) {
            return true;
        }
        self.exhausted = true;
        false
    }

    fn refill(&mut self, t: &Tok) {
        if self.buffer.len() > 64 {
            let last = self.buffer.len() - 1;
            self.buffer.swap(0, last);
            self.starts.swap(0, last);
            self.ends.swap(0, last);
            self.buffer.truncate(1);
            self.starts.truncate(1);
            self.ends.truncate(1);
            self.index -= last;
        }
        let len = utf16_len(&t.term) as i64;
        let mut start = t.start;
        let end = t.end;
        self.last_end = end;
        if end as i64 - start as i64 != len {
            for c in t.term.chars() {
                self.buffer.push(c);
                self.starts.push(start);
                self.ends.push(end);
            }
        } else {
            for c in t.term.chars() {
                self.buffer.push(c);
                self.starts.push(start);
                start += c.len_utf16() as u32;
                self.ends.push(start);
            }
        }
    }

    fn flush_bigram(&mut self, t: &mut Tok) {
        t.clear();
        t.term.push(self.buffer[self.index]);
        t.term.push(self.buffer[self.index + 1]);
        t.start = self.starts[self.index];
        t.end = self.ends[self.index + 1];
        t.kind = "<DOUBLE>";
        if self.unigrams {
            t.pos_inc = 0;
            t.pos_len = 2;
        }
        self.index += 1;
    }

    fn flush_unigram(&mut self, t: &mut Tok) {
        t.clear();
        t.term.push(self.buffer[self.index]);
        t.start = self.starts[self.index];
        t.end = self.ends[self.index];
        t.kind = "<SINGLE>";
        self.index += 1;
    }

    fn clear_buffer(&mut self) {
        self.index = 0;
        self.buffer.clear();
        self.starts.clear();
        self.ends.clear();
    }
}

impl Filter for CjkBigram {
    fn reset(&mut self) {
        self.clear_buffer();
        self.last_end = 0;
        self.has_lone = false;
        self.exhausted = false;
        self.ngram_state = false;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        loop {
            if self.has_bigram() {
                if self.unigrams {
                    if self.ngram_state {
                        self.flush_bigram(t);
                    } else {
                        self.flush_unigram(t);
                        self.index -= 1;
                    }
                    self.ngram_state = !self.ngram_state;
                } else {
                    self.flush_bigram(t);
                }
                return true;
            } else if self.do_next(input, t) {
                if self.is_cjk(t.kind) {
                    if t.start != self.last_end {
                        if self.has_unigram() {
                            self.lone.clone_from(t);
                            self.has_lone = true;
                            self.flush_unigram(t);
                            return true;
                        }
                        self.clear_buffer();
                    }
                    self.refill(t);
                } else {
                    if self.has_unigram() {
                        self.lone.clone_from(t);
                        self.has_lone = true;
                        self.flush_unigram(t);
                        return true;
                    }
                    return true;
                }
            } else {
                if self.has_unigram() {
                    self.flush_unigram(t);
                    return true;
                }
                return false;
            }
        }
    }
}

struct CjkBigramFactory {
    flags: u32,
    unigrams: bool,
}

impl FilterFactory for CjkBigramFactory {
    fn create(&self) -> Box<dyn Filter> {
        Box::new(CjkBigram::new(self.flags, self.unigrams))
    }
    fn synonym_role(&self) -> SynonymRole {
        if self.unigrams {
            SynonymRole::Forbidden
        } else {
            SynonymRole::Same
        }
    }
}

pub(crate) fn cjk_bigram_factory(s: &Settings) -> Result<Arc<dyn FilterFactory>> {
    let unigrams = s.get_bool("output_unigrams", false)?;
    let ignored = s.list_or_empty("ignored_scripts");
    let mut flags = 0;
    for (name, f) in [
        ("han", HAN),
        ("hiragana", HIRAGANA),
        ("katakana", KATAKANA),
        ("hangul", HANGUL),
    ] {
        if !ignored.iter().any(|i| i == name) {
            flags |= f;
        }
    }
    Ok(Arc::new(CjkBigramFactory { flags, unigrams }))
}

pub(crate) fn cjk_bigram_prebuilt() -> Arc<dyn FilterFactory> {
    Arc::new(CjkBigramFactory {
        flags: HAN | HIRAGANA | KATAKANA | HANGUL,
        unigrams: false,
    })
}

// ---------------------------------------------------------------------------------------------
// min_hash

/// Lucene's `murmurhash3_x64_128` (seed 0): (h1, h2).
pub(crate) fn murmur3_x64_128(key: &[u8], seed: u32) -> (u64, u64) {
    let c1: u64 = 0x87c37b91114253d5;
    let c2: u64 = 0x4cf5ad432745937f;
    let mut h1 = seed as u64;
    let mut h2 = seed as u64;
    let len = key.len();
    let rounded = len & !15;
    let le = |b: &[u8]| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
    let mut i = 0;
    while i < rounded {
        let mut k1 = le(&key[i..]);
        let mut k2 = le(&key[i + 8..]);
        k1 = k1.wrapping_mul(c1).rotate_left(31).wrapping_mul(c2);
        h1 ^= k1;
        h1 = h1
            .rotate_left(27)
            .wrapping_add(h2)
            .wrapping_mul(5)
            .wrapping_add(0x52dce729);
        k2 = k2.wrapping_mul(c2).rotate_left(33).wrapping_mul(c1);
        h2 ^= k2;
        h2 = h2
            .rotate_left(31)
            .wrapping_add(h1)
            .wrapping_mul(5)
            .wrapping_add(0x38495ab5);
        i += 16;
    }
    let tail = &key[rounded..];
    let mut k1: u64 = 0;
    let mut k2: u64 = 0;
    let n = len & 15;
    if n > 8 {
        for j in (8..n).rev() {
            k2 |= (tail[j] as u64) << ((j - 8) * 8);
        }
        k2 = k2.wrapping_mul(c2).rotate_left(33).wrapping_mul(c1);
        h2 ^= k2;
    }
    if n > 0 {
        for j in (0..n.min(8)).rev() {
            k1 |= (tail[j] as u64) << (j * 8);
        }
        k1 = k1.wrapping_mul(c1).rotate_left(31).wrapping_mul(c2);
        h1 ^= k1;
    }
    h1 ^= len as u64;
    h2 ^= len as u64;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    h1 = fmix64(h1);
    h2 = fmix64(h2);
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    (h1, h2)
}

fn fmix64(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51afd7ed558ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ceb9fe1a85ec53);
    k ^= k >> 33;
    k
}

/// `MinHashFilter`: the smallest `hash_set_size` hashes per bucket and hash function, as tokens.
pub(crate) struct MinHash {
    hash_count: usize,
    bucket_count: usize,
    set_size: usize,
    rotation: bool,
    bucket_size: u64,
    /// Per (hash, bucket): the smallest hashes, sorted ascending as (val2, val1).
    sets: Vec<Vec<(u64, u64)>>,
    int_hashes: Vec<(u64, u64)>,
    started: bool,
    hash_pos: i64,
    bucket_pos: i64,
    end_offset: u32,
    end_state: Tok,
    first_returned: bool,
    bytes: Vec<u8>,
    units: Vec<u16>,
}

impl MinHash {
    pub fn new(hash_count: usize, bucket_count: usize, set_size: usize, rotation: bool) -> Self {
        let mut bucket_size = (1u64 << 32) / bucket_count as u64;
        if !(1u64 << 32).is_multiple_of(bucket_count as u64) {
            bucket_size += 1;
        }
        let int_hashes = (0..hash_count)
            .map(|i| murmur3_x64_128(&(i as u32).to_be_bytes(), 0))
            .collect();
        MinHash {
            hash_count,
            bucket_count,
            set_size,
            rotation,
            bucket_size,
            sets: (0..hash_count * bucket_count).map(|_| Vec::new()).collect(),
            int_hashes,
            started: false,
            hash_pos: -1,
            bucket_pos: -1,
            end_offset: 0,
            end_state: Tok::default(),
            first_returned: false,
            bytes: Vec::new(),
            units: Vec::new(),
        }
    }

    /// `FixedSizeTreeSet.add` with Lucene's unsigned (val2, val1) order.
    fn add(&mut self, set: usize, v: (u64, u64)) {
        let s = &mut self.sets[set];
        match s.binary_search(&v) {
            Ok(_) => {}
            Err(i) => {
                if s.len() >= self.set_size {
                    if i >= s.len() {
                        return;
                    }
                    s.pop();
                }
                s.insert(i, v);
            }
        }
    }

    fn push_char(term: &mut String, units: &mut Vec<u16>, u: u16) {
        let _ = term;
        units.push(u);
    }
}

impl Filter for MinHash {
    fn reset(&mut self) {
        for s in &mut self.sets {
            s.clear();
        }
        self.started = false;
        self.hash_pos = -1;
        self.bucket_pos = -1;
        self.first_returned = false;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        let mut pos_inc = 0;
        if !self.started {
            self.started = true;
            let mut found = false;
            while input.next(t) {
                found = true;
                self.bytes.clear();
                for u in t.term.encode_utf16() {
                    self.bytes.extend_from_slice(&u.to_le_bytes());
                }
                let (v1, v2) = murmur3_x64_128(&self.bytes, 0);
                for i in 0..self.hash_count {
                    let (i1, i2) = self.int_hashes[i];
                    // combineOrdered(hash, intHash): ((0 * 37 + v) * 37 + i).
                    let r1 = v1.wrapping_mul(37).wrapping_add(i1);
                    let r2 = v2.wrapping_mul(37).wrapping_add(i2);
                    let bucket = ((r2 >> 32) / self.bucket_size) as usize;
                    self.add(i * self.bucket_count + bucket, (r2, r1));
                }
                self.end_offset = t.end;
            }
            t.clear_for_end();
            input.end(t);
            self.end_state.clone_from(t);
            if !found {
                return false;
            }
            pos_inc = 1;
            if self.rotation && self.set_size == 1 {
                for h in 0..self.hash_count {
                    for b in 0..self.bucket_count {
                        if self.sets[h * self.bucket_count + b].is_empty() {
                            for off in 1..self.bucket_count {
                                let other = h * self.bucket_count + (b + off) % self.bucket_count;
                                if let Some(&first) = self.sets[other].first() {
                                    self.add(h * self.bucket_count + b, first);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        } else if self.first_returned {
            pos_inc = 0;
        }
        t.clear();
        while self.hash_pos < self.hash_count as i64 {
            if self.hash_pos == -1 {
                self.hash_pos += 1;
                continue;
            }
            while self.bucket_pos < self.bucket_count as i64 {
                if self.bucket_pos == -1 {
                    self.bucket_pos += 1;
                    continue;
                }
                let set = self.hash_pos as usize * self.bucket_count + self.bucket_pos as usize;
                if self.sets[set].is_empty() {
                    self.bucket_pos += 1;
                    continue;
                }
                let (high, low) = self.sets[set].remove(0);
                self.units.clear();
                let mut term = std::mem::take(&mut t.term);
                if self.hash_count > 1 {
                    let hp = self.hash_pos as u32;
                    Self::push_char(&mut term, &mut self.units, (hp >> 16) as u16);
                    Self::push_char(&mut term, &mut self.units, hp as u16);
                }
                for shift in [48, 32, 16, 0] {
                    self.units.push((high >> shift) as u16);
                }
                self.units.push((low >> 48) as u16);
                self.units.push((low >> 32) as u16);
                if self.hash_count == 1 {
                    self.units.push((low >> 16) as u16);
                    self.units.push(low as u16);
                }
                term.clear();
                // Lone surrogates index as U+FFFD (`UnicodeUtil.UTF16toUTF8`).
                term.extend(
                    char::decode_utf16(self.units.iter().copied()).map(|r| r.unwrap_or('\u{FFFD}')),
                );
                t.term = term;
                t.pos_inc = pos_inc;
                t.start = 0;
                t.end = self.end_offset;
                t.kind = "MIN_HASH";
                t.pos_len = 1;
                self.first_returned = true;
                return true;
            }
            self.bucket_pos = -1;
            self.hash_pos += 1;
        }
        false
    }

    fn end(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        if !self.started {
            input.end(t);
            self.end_state.clone_from(t);
        }
        t.clone_from(&self.end_state);
    }
}

struct MinHashFactory {
    hash_count: i32,
    bucket_count: i32,
    set_size: i32,
    rotation: bool,
}

impl FilterFactory for MinHashFactory {
    fn create(&self) -> Box<dyn Filter> {
        Box::new(MinHash::new(
            self.hash_count.max(1) as usize,
            self.bucket_count.max(1) as usize,
            self.set_size.max(1) as usize,
            self.rotation,
        ))
    }
    fn check(&self) -> Result<()> {
        if self.hash_count <= 0 {
            return Err(AnalysisError::illegal(
                "hashCount must be greater than zero",
            ));
        }
        if self.bucket_count <= 0 {
            return Err(AnalysisError::illegal(
                "bucketCount must be greater than zero",
            ));
        }
        if self.set_size <= 0 {
            return Err(AnalysisError::illegal(
                "hashSetSize must be greater than zero",
            ));
        }
        Ok(())
    }
}

/// `min_hash` (`MinHashTokenFilterFactory`).
pub(crate) fn min_hash_factory(
    ctx: &crate::registry::Ctx,
    s: &Settings,
) -> Result<Arc<dyn FilterFactory>> {
    let hash_count = s.get_int("hash_count", 1)?;
    let bucket_count = s.get_int("bucket_count", 512)?;
    let set_size = s.get_int("hash_set_size", 1)?;
    for (k, v) in [
        ("hash_count", hash_count),
        ("bucket_count", bucket_count),
        ("hash_set_size", set_size),
    ] {
        if s.contains(k) && v > 10_000 {
            return Err(AnalysisError::illegal(format!(
                "[{k}] must not exceed [10000] but was [{v}]"
            )));
        }
    }
    if s.contains("hash_count") || s.contains("bucket_count") || s.contains("hash_set_size") {
        let max = ctx.opts.max_token_count as i64;
        let product = hash_count as i64 * bucket_count as i64 * set_size as i64;
        if product > max {
            return Err(AnalysisError::illegal(format!(
                "The product of hash_count [{hash_count}], bucket_count [{bucket_count}], and hash_set_size [{set_size}] is [{product}], which exceeds the maximum token count limit [{max}]. This limit can be set by changing the [index.analyze.max_token_count] index level setting."
            )));
        }
    }
    // Elasticsearch hands the setting to Lucene's `MinHashFilterFactory`, which reads it with
    // `Boolean.parseBoolean`.
    let rotation = s.get_java_bool("with_rotation", bucket_count > 1);
    Ok(Arc::new(MinHashFactory {
        hash_count,
        bucket_count,
        set_size,
        rotation,
    }))
}

// ---------------------------------------------------------------------------------------------
// pattern_capture / pattern_replace

/// `PatternCaptureGroupTokenFilter`: every non-empty capture group of every match of every
/// pattern, stacked on the token (optionally with the original first).
pub(crate) struct PatternCapture {
    patterns: Arc<Vec<JavaRegex>>,
    preserve: bool,
    caps: Vec<Captures>,
    finders: Vec<Finder>,
    current_group: Vec<i64>,
    current_matcher: i64,
    spare: String,
    state: Tok,
}

impl PatternCapture {
    pub fn new(patterns: Arc<Vec<JavaRegex>>, preserve: bool) -> Self {
        let caps = patterns.iter().map(|p| p.captures()).collect();
        let n = patterns.len();
        PatternCapture {
            patterns,
            preserve,
            caps,
            finders: (0..n).map(|_| Finder::new()).collect(),
            current_group: vec![-1; n],
            current_matcher: -1,
            spare: String::new(),
            state: Tok::default(),
        }
    }

    fn group_count(&self, i: usize) -> i64 {
        self.patterns[i].groups as i64
    }

    fn next_capture(&mut self) -> bool {
        let mut min_offset = usize::MAX;
        self.current_matcher = -1;
        let mut i = 0usize;
        while i < self.patterns.len() {
            if self.current_group[i] == -1 {
                let found = self.finders[i]
                    .next(&self.patterns[i], &self.spare, &mut self.caps[i], true)
                    .is_some();
                self.current_group[i] = if found { 1 } else { 0 };
            }
            let mut retry = false;
            if self.current_group[i] != 0 {
                while self.current_group[i] < self.group_count(i) + 1 {
                    let (start, end) = self.caps[i]
                        .get(self.current_group[i] as usize)
                        .unwrap_or((usize::MAX, usize::MAX));
                    if start == end || (self.preserve && start == 0 && self.spare.len() == end) {
                        self.current_group[i] += 1;
                        continue;
                    }
                    if start < min_offset {
                        min_offset = start;
                        self.current_matcher = i as i64;
                    }
                    break;
                }
                if self.current_group[i] == self.group_count(i) + 1 {
                    self.current_group[i] = -1;
                    retry = true;
                }
            }
            if !retry {
                i += 1;
            }
        }
        self.current_matcher != -1
    }

    fn current_range(&self) -> (usize, usize) {
        let m = self.current_matcher as usize;
        self.caps[m]
            .get(self.current_group[m] as usize)
            .unwrap_or((0, 0))
    }
}

impl Filter for PatternCapture {
    fn reset(&mut self) {
        self.current_matcher = -1;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if self.current_matcher != -1 && self.next_capture() {
            t.clone_from(&self.state);
            let (s, e) = self.current_range();
            t.pos_inc = 0;
            t.term.clear();
            t.term.push_str(&self.spare[s..e]);
            let m = self.current_matcher as usize;
            self.current_group[m] += 1;
            return true;
        }
        if !input.next(t) {
            return false;
        }
        self.spare.clone_from(&t.term);
        self.state.clone_from(t);
        for i in 0..self.patterns.len() {
            self.finders[i].reset();
            self.current_group[i] = -1;
        }
        if self.preserve {
            self.current_matcher = 0;
        } else if self.next_capture() {
            let (s, e) = self.current_range();
            t.term.clear();
            t.term.push_str(&self.spare[s..e]);
            let m = self.current_matcher as usize;
            self.current_group[m] += 1;
        }
        true
    }
}

pub(crate) fn pattern_capture_factory(name: &str, s: &Settings) -> Result<Arc<dyn FilterFactory>> {
    let Some(regexes) = s.get_list_unsplit("patterns") else {
        return Err(AnalysisError::illegal(format!(
            "required setting 'patterns' is missing for token filter [{name}]"
        )));
    };
    let patterns = Arc::new(
        regexes
            .iter()
            .map(|r| jregex::compile(r, 0))
            .collect::<Result<Vec<_>>>()?,
    );
    let preserve = s.get_bool("preserve_original", true)?;
    Ok(filter_fn(move || {
        Box::new(PatternCapture::new(patterns.clone(), preserve))
    }))
}

/// `PatternReplaceFilter`: replaces matches in the term when there is one.
pub(crate) struct PatternReplace {
    re: Arc<JavaRegex>,
    rep: Arc<Replacement>,
    all: bool,
    caps: Captures,
    out: String,
}

impl Filter for PatternReplace {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !input.next(t) {
            return false;
        }
        self.out.clear();
        match self
            .rep
            .replace(&self.re, &t.term, self.all, &mut self.caps, &mut self.out)
        {
            Ok(true) => std::mem::swap(&mut t.term, &mut self.out),
            Ok(false) => {}
            Err(e) => {
                t.error = Some(Box::new(e));
                return false;
            }
        }
        true
    }
}

pub(crate) fn pattern_replace_factory(name: &str, s: &Settings) -> Result<Arc<dyn FilterFactory>> {
    let Some(pattern) = s.get("pattern") else {
        return Err(AnalysisError::illegal(format!(
            "pattern is missing for [{name}] token filter of type 'pattern_replace'"
        )));
    };
    let flags = match s.get("flags") {
        Some(f) => jregex::flags_from_string(&f)?,
        None => 0,
    };
    let re = jregex::compile(&pattern, flags)?;
    let rep = Arc::new(Replacement::parse(
        &s.get("replacement").unwrap_or_default(),
        &re,
    ));
    let re = Arc::new(re);
    let all = s.get_bool("all", true)?;
    Ok(normalizing_fn(move || {
        Box::new(PatternReplace {
            re: re.clone(),
            rep: rep.clone(),
            all,
            caps: re.captures(),
            out: String::new(),
        })
    }))
}

// ---------------------------------------------------------------------------------------------
// stemmer_override

/// `StemmerOverrideFilter`: replaces terms found in the map and marks them as keywords.
pub(crate) struct StemmerOverride {
    map: Arc<HashMap<String, String, foldhash::fast::RandomState>>,
}

impl Filter for StemmerOverride {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !input.next(t) {
            return false;
        }
        if !t.keyword
            && let Some(stem) = self.map.get(t.term.as_str())
        {
            t.term.clear();
            t.term.push_str(stem);
            t.keyword = true;
        }
        true
    }
}

/// `StemmerOverrideTokenFilterFactory.parseRules`.
pub(crate) fn parse_override_rules(rules: &[String], sep: &str) -> Result<Vec<(String, String)>> {
    let invalid = |rule: &str| {
        AnalysisError::other(
            "runtime_exception",
            500,
            format!("Invalid Keyword override Rule:{rule}"),
        )
    };
    let mut out = Vec::new();
    for rule in rules {
        let sides: Vec<&str> = rule.split(sep).collect();
        if sides.len() != 2 {
            return Err(invalid(rule));
        }
        let over = sides[1].trim_matches(|c: char| c <= ' ');
        if over.is_empty() || over.contains(',') {
            return Err(invalid(rule));
        }
        for key in sides[0].split(',') {
            let k = key.trim_matches(|c: char| c <= ' ');
            if k.is_empty() {
                return Err(invalid(rule));
            }
            out.push((k.to_string(), over.to_string()));
        }
    }
    Ok(out)
}

pub(crate) fn stemmer_override_factory(s: &Settings) -> Result<Arc<dyn FilterFactory>> {
    let Some(rules) = word_list(s, "rules")? else {
        return Err(AnalysisError::illegal(
            "stemmer override filter requires either `rules` or `rules_path` to be configured",
        ));
    };
    let mut map: HashMap<String, String, foldhash::fast::RandomState> = HashMap::default();
    for (k, v) in parse_override_rules(&rules, "=>")? {
        // The first rule for a key wins.
        map.entry(k).or_insert(v);
    }
    let map = Arc::new(map);
    Ok(filter_with(
        false,
        attr::KEYWORD,
        SynonymRole::Same,
        move || Box::new(StemmerOverride { map: map.clone() }),
    ))
}

// ---------------------------------------------------------------------------------------------
// Compound words

/// `DictionaryCompoundWordTokenFilter` as Elasticsearch configures it (longest matches only,
/// skipping their subwords): dictionary subwords stacked on the word.
pub(crate) struct Decompound {
    dict: Arc<WordSet>,
    min_word: usize,
    min_sub: usize,
    max_sub: usize,
    units: Vec<u16>,
    parts: Vec<String>,
    next_part: usize,
    current: Tok,
    scratch: String,
    piece: String,
}

impl Filter for Decompound {
    fn reset(&mut self) {
        self.parts.clear();
        self.next_part = 0;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if self.next_part < self.parts.len() {
            t.clone_from(&self.current);
            t.term.clear();
            t.term.push_str(&self.parts[self.next_part]);
            t.pos_inc = 0;
            self.next_part += 1;
            return true;
        }
        self.parts.clear();
        self.next_part = 0;
        if !input.next(t) {
            return false;
        }
        self.units.clear();
        self.units.extend(t.term.encode_utf16());
        let len = self.units.len();
        if len >= self.min_word {
            let mut i = 0;
            while len >= self.min_sub && i <= len - self.min_sub {
                let mut longest: Option<usize> = None;
                for j in self.min_sub..=self.max_sub {
                    if i + j > len {
                        break;
                    }
                    self.piece.clear();
                    self.piece.extend(
                        char::decode_utf16(self.units[i..i + j].iter().copied())
                            .map(|r| r.unwrap_or('\u{FFFD}')),
                    );
                    if self.dict.contains(&self.piece, &mut self.scratch)
                        && longest.is_none_or(|l| l < j)
                    {
                        longest = Some(j);
                    }
                }
                if let Some(j) = longest {
                    let s: String = char::decode_utf16(self.units[i..i + j].iter().copied())
                        .map(|r| r.unwrap_or('\u{FFFD}'))
                        .collect();
                    self.parts.push(s);
                    i += j - 1;
                }
                i += 1;
            }
            if !self.parts.is_empty() {
                self.current.clone_from(t);
            }
        }
        true
    }
}

/// The settings of the compound word filters. The outer `Result` holds the errors raised while
/// the factory is configured; the inner one the size checks Lucene makes when the filter is
/// created (reported once an analyzer uses it).
#[allow(clippy::type_complexity)]
fn compound_settings(
    name: &str,
    s: &Settings,
) -> Result<(Arc<WordSet>, Result<(usize, usize, usize)>)> {
    let min_word = s.get_int("min_word_size", 5)?;
    let min_sub = s.get_int("min_subword_size", 2)?;
    let max_sub = s.get_int("max_subword_size", 15)?;
    s.get_bool("only_longest_match", false)?;
    let Some(words) = word_set(s, "word_list")? else {
        return Err(AnalysisError::illegal(format!(
            "word_list must be provided for [{name}], either as a path to a file, or directly"
        )));
    };
    let sizes = if min_word < 0 {
        Err(AnalysisError::illegal("minWordSize cannot be negative"))
    } else if min_sub < 0 {
        Err(AnalysisError::illegal("minSubwordSize cannot be negative"))
    } else if max_sub < 0 {
        Err(AnalysisError::illegal("maxSubwordSize cannot be negative"))
    } else {
        Ok((min_word as usize, min_sub as usize, max_sub as usize))
    };
    Ok((Arc::new(words), sizes))
}

pub(crate) fn decompounder_factory(name: &str, s: &Settings) -> Result<Arc<dyn FilterFactory>> {
    let (dict, sizes) = compound_settings(name, s)?;
    let (min_word, min_sub, max_sub) = match sizes {
        Ok(sizes) => sizes,
        Err(e) => return Ok(crate::component::lazy_filter_error(e, false)),
    };
    Ok(filter_with(false, 0, SynonymRole::Identity, move || {
        Box::new(Decompound {
            dict: dict.clone(),
            min_word,
            min_sub,
            max_sub,
            units: Vec::new(),
            parts: Vec::new(),
            next_part: 0,
            current: Tok::default(),
            scratch: String::new(),
            piece: String::new(),
        })
    }))
}

/// `hyphenation_decompounder`: its patterns file never exists here, which fails as on a node
/// without the file.
pub(crate) fn hyphenation_factory(name: &str, s: &Settings) -> Result<Arc<dyn FilterFactory>> {
    // The size checks would only come at creation; the patterns file fails first.
    let _ = compound_settings(name, s)?;
    let Some(path) = s.get("hyphenation_patterns_path") else {
        return Err(AnalysisError::illegal(
            "hyphenation_patterns_path is a required setting.",
        ));
    };
    let cause = match crate::registry::file_failure(&path, crate::registry::MODULE_READABLE) {
        crate::registry::FileFailure::Denied(full) => crate::registry::not_entitled(
            "analysis-common",
            "org.elasticsearch.analysis.common",
            "org.elasticsearch.analysis.common.HyphenationCompoundWordTokenFilterFactory",
            &full,
        ),
        crate::registry::FileFailure::Directory(_) => {
            AnalysisError::other("i_o_exception", 500, "Is a directory")
        }
        crate::registry::FileFailure::Missing(full) => {
            AnalysisError::other("no_such_file_exception", 400, full)
        }
    };
    Err(
        AnalysisError::illegal("Exception while reading hyphenation_patterns_path.")
            .with_cause(cause),
    )
}

// ---------------------------------------------------------------------------------------------
// hunspell

/// `HunspellTokenFilterFactory`. Dictionaries live in the node's `config/hunspell/<locale>/`
/// directory, and roll has none, so every locale fails the way a node without that dictionary
/// fails (the locale is required first).
pub(crate) fn hunspell_factory(s: &Settings) -> Result<Arc<dyn FilterFactory>> {
    let locale = s
        .get("locale")
        .or_else(|| s.get("language"))
        .or_else(|| s.get("lang"));
    let Some(locale) = locale else {
        return Err(AnalysisError::illegal(
            "missing [locale | language | lang] configuration for hunspell token filter",
        ));
    };
    Err(AnalysisError::illegal(format!(
        "failed to load hunspell dictionary for locale: {locale}"
    ))
    .with_cause(AnalysisError::other(
        "exception",
        500,
        format!("Could not find hunspell dictionary [{locale}]"),
    )))
}

// ---------------------------------------------------------------------------------------------
// type_as_payload

/// `TypeAsPayloadTokenFilter`: the token type's UTF-8 bytes as payload.
pub(crate) fn type_as_payload_factory() -> Arc<dyn FilterFactory> {
    filter_with(false, attr::PAYLOAD, SynonymRole::Same, || {
        Box::new(super::Map(|t: &mut Tok| {
            t.payload.clear();
            t.payload.extend_from_slice(t.kind.as_bytes());
            t.has_payload = true;
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::tests::{run_filters, terms};

    #[test]
    fn width_and_kana() {
        let mut t = Tok::default();
        let mut u = Vec::new();
        t.term.push_str("ＦＵＬＬ１２３ ｶﾀｶﾅ ｶﾞ");
        cjk_width(&mut t, &mut u);
        assert_eq!(t.term, "FULL123 カタカナ ガ");
    }

    #[test]
    fn murmur_matches_reference() {
        // Reference values of MurmurHash3_x64_128 (seed 0).
        let (h1, h2) = murmur3_x64_128(b"", 0);
        assert_eq!((h1, h2), (0, 0));
        let (h1, _) = murmur3_x64_128(b"hello", 0);
        assert_eq!(h1, 0xcbd8a7b341bd9b02);
    }

    #[test]
    fn capture_and_override() {
        let pats = Arc::new(vec![jregex::compile("(\\w+)@(\\w+)", 0).unwrap()]);
        let out = terms(
            vec![Box::new(PatternCapture::new(pats, true))],
            "joe@example x",
        );
        assert_eq!(out, vec!["joe@example", "joe", "example", "x"]);
        let rules = parse_override_rules(&["running, runs => run".into()], "=>").unwrap();
        assert_eq!(
            rules,
            vec![
                ("running".into(), "run".into()),
                ("runs".into(), "run".into())
            ]
        );
        assert_eq!(
            parse_override_rules(&["a => ".into()], "=>")
                .unwrap_err()
                .reason,
            "Invalid Keyword override Rule:a => "
        );
        let out = run_filters(
            vec![
                decompounder_factory("d", &{
                    let mut s = Settings::new();
                    s.put_list(
                        "word_list",
                        vec![
                            "dampf".into(),
                            "schiff".into(),
                            "dampfschiff".into(),
                            "fahrt".into(),
                        ],
                    );
                    s
                })
                .unwrap()
                .create(),
            ],
            "donaudampfschifffahrt",
        );
        let t: Vec<_> = out.iter().map(|t| t.0.as_str()).collect();
        assert_eq!(t, vec!["donaudampfschifffahrt", "dampfschiff", "fahrt"]);
    }
}
