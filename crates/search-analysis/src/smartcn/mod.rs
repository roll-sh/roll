//! Elasticsearch's `analysis-smartcn` plugin: Lucene's `SmartChineseAnalyzer`
//! (`org.apache.lucene.analysis.cn.smart`, Lucene 10.5.1): a hidden Markov model over word and
//! word-bigram frequencies (Lucene's `coredict` and `bigramdict`, derived from ICTCLAS),
//! choosing the most probable segmentation of each sentence.
//!
//! - tokenizer `smartcn_tokenizer` (`HMMChineseTokenizer`)
//! - analyzer `smartcn`
//! - filter `smartcn_stop`
//!
//! Text is cut into 1024-unit chunks and sentences exactly as Lucene's `SegmentingTokenizerBase`
//! cuts it (the JDK's sentence iterator, shared with the `thai` tokenizer), then each sentence
//! goes through `HHMMSegmenter`: a graph of candidate words (dictionary words of Han characters
//! 4E00-9FA5, runs of Latin letters or digits, single delimiters), weighted by the bigram model,
//! and its shortest path.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::filter::WordSet;
use crate::stream::Tokenizer;
use crate::token::Tok;
use crate::tokenizer::thai::{is_safe_end, sentence_next};

static PACK: &[u8] = include_bytes!("hhmm.dict");

/// `SegmentingTokenizerBase.BUFFERMAX`.
const BUFFER: usize = 1024;

const MAX_FREQUENCE: i32 = 2079997 + 80000;
const STRING_CHAR_ARRAY: &str = "未##串";
const NUMBER_CHAR_ARRAY: &str = "未##数";
const START_CHAR_ARRAY: &str = "始##始";
const END_CHAR_ARRAY: &str = "末##末";
const WORD_SEGMENT_CHAR: u16 = b'@' as u16;

/// `CharType`
#[derive(Clone, Copy, PartialEq, Eq)]
enum CharType {
    Delimiter,
    Letter,
    Digit,
    Hanzi,
    SpaceLike,
    FullwidthLetter,
    FullwidthDigit,
    Other,
    Surrogate,
}

/// `Utility.getCharType`.
fn char_type(ch: u16) -> CharType {
    if (0xD800..=0xDFFF).contains(&ch) {
        return CharType::Surrogate;
    }
    if (0x4E00..=0x9FA5).contains(&ch) {
        return CharType::Hanzi;
    }
    if (0x41..=0x5A).contains(&ch) || (0x61..=0x7A).contains(&ch) {
        return CharType::Letter;
    }
    if (0x30..=0x39).contains(&ch) {
        return CharType::Digit;
    }
    if matches!(ch, 0x20 | 0x09 | 0x0D | 0x0A | 0x3000) {
        return CharType::SpaceLike;
    }
    if (0x21..=0xBB).contains(&ch) || (0x2010..=0x2642).contains(&ch) || (0x3001..=0x301E).contains(&ch) {
        return CharType::Delimiter;
    }
    if (0xFF21..=0xFF3A).contains(&ch) || (0xFF41..=0xFF5A).contains(&ch) {
        return CharType::FullwidthLetter;
    }
    if (0xFF10..=0xFF19).contains(&ch) {
        return CharType::FullwidthDigit;
    }
    if (0xFE30..=0xFF63).contains(&ch) {
        return CharType::Delimiter;
    }
    CharType::Other
}

/// `WordType`
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WordType {
    SentenceBegin,
    SentenceEnd,
    ChineseWord,
    String,
    Number,
    Delimiter,
    FullwidthString,
    FullwidthNumber,
}

/// A word of a table: its characters after the first (`None` for the first alone) and its
/// frequency.
type Item = (Option<Vec<u16>>, i32);

/// The word and bigram dictionaries.
struct Dict {
    /// `charIndexTable`/`wordIndexTable`: a first character's word table + 1 (0: none).
    tables: Vec<u16>,
    /// Per table: the rest of each word (`None` for the character alone) and its frequency,
    /// sorted as `sortEachItems` sorts them.
    items: Vec<Vec<Item>>,
    /// `bigramHashTable`/`frequencyTable`: frequency by `hash1` of `w1@w2`, sorted by hash.
    bigrams: Vec<(i64, i32)>,
}

fn dict() -> &'static Dict {
    static D: OnceLock<Dict> = OnceLock::new();
    D.get_or_init(|| {
        let words = crate::morph::section(PACK, "words");
        let mut p = 0;
        let i32_at = |p: &mut usize| {
            let v = i32::from_le_bytes(words[*p..*p + 4].try_into().unwrap());
            *p += 4;
            v
        };
        let u16_at = |p: &mut usize| {
            let v = u16::from_le_bytes(words[*p..*p + 2].try_into().unwrap());
            *p += 2;
            v
        };
        let nc = i32_at(&mut p) as usize;
        let mut chars = Vec::with_capacity(nc);
        for _ in 0..nc {
            let c = u16_at(&mut p);
            let id = i32_at(&mut p);
            chars.push((c, id));
        }
        let nt = i32_at(&mut p) as usize;
        let mut ids = HashMap::new();
        let mut items = Vec::with_capacity(nt);
        for k in 0..nt {
            let id = i32_at(&mut p);
            let n = i32_at(&mut p) as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                let len = u16_at(&mut p);
                let word = (len != 0xFFFF).then(|| (0..len).map(|_| u16_at(&mut p)).collect::<Vec<u16>>());
                let f = i32_at(&mut p);
                v.push((word, f));
            }
            ids.insert(id, k);
            items.push(v);
        }
        let mut tables = vec![0u16; 0x10000];
        for (c, id) in chars {
            if let Some(&k) = ids.get(&id) {
                tables[c as usize] = k as u16 + 1;
            }
        }
        let b = crate::morph::section(PACK, "bigrams");
        let mut input = crate::morph::Input { b: &b, p: 0 };
        let vlong = |i: &mut crate::morph::Input| {
            let mut v = 0u64;
            let mut shift = 0;
            loop {
                let x = i.byte();
                v |= ((x & 0x7f) as u64) << shift;
                if x & 0x80 == 0 {
                    return v;
                }
                shift += 7;
            }
        };
        let n = vlong(&mut input) as usize;
        let mut bigrams = Vec::with_capacity(n);
        let mut prev = i64::MIN;
        for _ in 0..n {
            let h = prev.wrapping_add(vlong(&mut input) as i64);
            let f = vlong(&mut input) as u32 as i32;
            bigrams.push((h, f));
            prev = h;
        }
        Dict { tables, items, bigrams }
    })
}

/// `Utility.compareArray(larray, lstart, rarray, rstart)`.
fn compare_array(l: Option<&[u16]>, ls: usize, r: Option<&[u16]>, rs: usize) -> i32 {
    let (l, r) = match (l, r) {
        (None, r) => return if r.is_none_or(|r| rs >= r.len()) { 0 } else { -1 },
        (Some(l), None) => return if ls >= l.len() { 0 } else { 1 },
        (Some(l), Some(r)) => (l, r),
    };
    let (mut li, mut ri) = (ls, rs);
    while li < l.len() && ri < r.len() && l[li] == r[ri] {
        li += 1;
        ri += 1;
    }
    if li == l.len() {
        if ri == r.len() { 0 } else { -1 }
    } else if ri == r.len() || l[li] > r[ri] {
        1
    } else {
        -1
    }
}

/// `Utility.compareArrayByPrefix(shortArray, shortIndex, longArray, longIndex)`.
fn compare_by_prefix(short: &[u16], si: usize, long: Option<&[u16]>, li: usize) -> i32 {
    let Some(long) = long else { return if si < short.len() { 1 } else { 0 } };
    let (mut si, mut li) = (si, li);
    while si < short.len() && li < long.len() && short[si] == long[li] {
        si += 1;
        li += 1;
    }
    if si == short.len() {
        0
    } else if li == long.len() || short[si] > long[li] {
        1
    } else {
        -1
    }
}

impl Dict {
    fn table(&self, c: u16) -> Option<&[Item]> {
        match self.tables[c as usize] {
            0 => None,
            k => Some(self.items[k as usize - 1].as_slice()),
        }
    }

    /// `WordDictionary.getPrefixMatch(charArray, knownStart)`.
    fn prefix_match(&self, w: &[u16], known_start: i32) -> i32 {
        let Some(items) = self.table(w[0]) else { return -1 };
        let (mut start, mut end) = (known_start, items.len() as i32 - 1);
        let mut mid = (start + end) / 2;
        while start <= end {
            let cmp = compare_by_prefix(w, 1, items[mid as usize].0.as_deref(), 0);
            if cmp == 0 {
                while mid >= 0 && compare_by_prefix(w, 1, items[mid as usize].0.as_deref(), 0) == 0 {
                    mid -= 1;
                }
                return mid + 1;
            } else if cmp < 0 {
                end = mid - 1;
            } else {
                start = mid + 1;
            }
            mid = (start + end) / 2;
        }
        -1
    }

    /// `WordDictionary.getFrequency`.
    fn frequency(&self, w: &[u16]) -> i32 {
        let Some(items) = self.table(w[0]) else { return 0 };
        let (mut start, mut end) = (0i32, items.len() as i32 - 1);
        let mut mid = (start + end) / 2;
        while start <= end {
            let cmp = compare_array(items[mid as usize].0.as_deref(), 0, Some(w), 1);
            if cmp == 0 {
                return items[mid as usize].1;
            } else if cmp < 0 {
                start = mid + 1;
            } else {
                end = mid - 1;
            }
            mid = (start + end) / 2;
        }
        0
    }

    /// `WordDictionary.isEqual`.
    fn is_equal(&self, w: &[u16], index: i32) -> bool {
        let Some(items) = self.table(w[0]) else { return false };
        compare_array(Some(w), 1, items[index as usize].0.as_deref(), 0) == 0
    }

    /// `BigramDictionary.getFrequency`.
    fn bigram_frequency(&self, w: &[u16]) -> i32 {
        const P: i64 = 1099511628211;
        let mut h: i64 = 0xcbf29ce484222325u64 as i64;
        for &d in w {
            h = (h ^ (d & 0xFF) as i64).wrapping_mul(P);
            h = (h ^ (d >> 8) as i64).wrapping_mul(P);
        }
        match self.bigrams.binary_search_by(|e| e.0.cmp(&h)) {
            Ok(i) => self.bigrams[i].1,
            Err(_) => 0,
        }
    }
}

/// A candidate word: a span of the sentence, the characters the models see (in the arena), its
/// type and frequency.
#[derive(Clone, Copy, Debug)]
struct SegToken {
    chars: (u32, u32),
    start: i32,
    end: i32,
    word_type: WordType,
    weight: i32,
}

/// A word of the best path, offsets absolute.
#[derive(Clone, Debug)]
struct Word {
    chars: Vec<u16>,
    start: i32,
    end: i32,
}

/// `HHMMSegmenter` with its buffers, reused across sentences.
#[derive(Default)]
struct Segmenter {
    /// Characters of the candidates (the placeholder words first).
    arena: Vec<u16>,
    types: Vec<CharType>,
    tokens: Vec<SegToken>,
    /// Candidates by start offset + 1 (index 0 is the sentence start), in insertion order.
    by_start: Vec<Vec<u32>>,
    /// `makeIndex`: the graph index of each candidate, and the candidates in index order.
    index: Vec<u32>,
    order: Vec<u32>,
    /// Edges into each index: (from index, weight), in generation order.
    edges: Vec<Vec<(u32, f64)>>,
    id: Vec<u16>,
    word: Vec<u16>,
    path: Vec<(f64, u32)>,
}

const PH_STRING: (u32, u32) = (0, 4);
const PH_NUMBER: (u32, u32) = (4, 4);
const PH_START: (u32, u32) = (8, 4);
const PH_END: (u32, u32) = (12, 4);

impl Segmenter {
    fn chars(&self, c: (u32, u32)) -> &[u16] {
        &self.arena[c.0 as usize..(c.0 + c.1) as usize]
    }

    fn add(&mut self, t: SegToken) {
        let slot = (t.start + 1) as usize;
        if self.by_start.len() <= slot {
            self.by_start.resize_with(slot + 1, Vec::new);
        }
        self.by_start[slot].push(self.tokens.len() as u32);
        self.tokens.push(t);
    }

    fn placeholder(&mut self, d: &Dict, c: (u32, u32), start: i32, end: i32, word_type: WordType) {
        let weight = d.frequency(self.chars(c));
        self.add(SegToken { chars: c, start, end, word_type, weight });
    }

    /// `createSegGraph`.
    fn graph(&mut self, d: &Dict, s: &[u16]) {
        self.arena.clear();
        for w in [STRING_CHAR_ARRAY, NUMBER_CHAR_ARRAY, START_CHAR_ARRAY, END_CHAR_ARRAY] {
            self.arena.extend(w.encode_utf16());
        }
        self.tokens.clear();
        for l in self.by_start.iter_mut() {
            l.clear();
        }
        self.types.clear();
        self.types.extend(s.iter().map(|&c| char_type(c)));
        let length = s.len();
        let mut i = 0usize;
        while i < length {
            let mut full = false;
            match self.types[i] {
                CharType::SpaceLike => i += 1,
                CharType::Surrogate => {
                    let count = if (0xD800..=0xDBFF).contains(&s[i])
                        && i + 1 < length
                        && (0xDC00..=0xDFFF).contains(&s[i + 1])
                    {
                        2
                    } else {
                        1
                    };
                    let off = self.arena.len() as u32;
                    self.arena.extend_from_slice(&s[i..i + count]);
                    self.add(SegToken {
                        chars: (off, count as u32),
                        start: i as i32,
                        end: (i + count) as i32,
                        word_type: WordType::ChineseWord,
                        weight: 0,
                    });
                    i += count;
                }
                CharType::Hanzi => {
                    let mut j = i + 1;
                    let mut word = std::mem::take(&mut self.word);
                    word.clear();
                    word.push(s[i]);
                    let off = self.arena.len() as u32;
                    self.arena.push(s[i]);
                    self.add(SegToken {
                        chars: (off, 1),
                        start: i as i32,
                        end: j as i32,
                        word_type: WordType::ChineseWord,
                        weight: d.frequency(&word),
                    });
                    let mut found = d.prefix_match(&word, 0);
                    while j <= length && found != -1 {
                        if word.len() > 1 && d.is_equal(&word, found) {
                            let off = self.arena.len() as u32;
                            self.arena.extend_from_slice(&word);
                            self.add(SegToken {
                                chars: (off, word.len() as u32),
                                start: i as i32,
                                end: j as i32,
                                word_type: WordType::ChineseWord,
                                weight: d.frequency(&word),
                            });
                        }
                        while j < length && self.types[j] == CharType::SpaceLike {
                            j += 1;
                        }
                        if j < length && self.types[j] == CharType::Hanzi {
                            word.push(s[j]);
                            found = d.prefix_match(&word, found);
                            j += 1;
                        } else {
                            break;
                        }
                    }
                    self.word = word;
                    i += 1;
                }
                t @ (CharType::FullwidthLetter | CharType::Letter) => {
                    full |= t == CharType::FullwidthLetter;
                    let mut j = i + 1;
                    while j < length && matches!(self.types[j], CharType::Letter | CharType::FullwidthLetter) {
                        full |= self.types[j] == CharType::FullwidthLetter;
                        j += 1;
                    }
                    let ty = if full { WordType::FullwidthString } else { WordType::String };
                    self.placeholder(d, PH_STRING, i as i32, j as i32, ty);
                    i = j;
                }
                t @ (CharType::FullwidthDigit | CharType::Digit) => {
                    full |= t == CharType::FullwidthDigit;
                    let mut j = i + 1;
                    while j < length && matches!(self.types[j], CharType::Digit | CharType::FullwidthDigit) {
                        full |= self.types[j] == CharType::FullwidthDigit;
                        j += 1;
                    }
                    let ty = if full { WordType::FullwidthNumber } else { WordType::Number };
                    self.placeholder(d, PH_NUMBER, i as i32, j as i32, ty);
                    i = j;
                }
                CharType::Delimiter => {
                    let off = self.arena.len() as u32;
                    self.arena.push(s[i]);
                    self.add(SegToken {
                        chars: (off, 1),
                        start: i as i32,
                        end: i as i32 + 1,
                        word_type: WordType::Delimiter,
                        weight: MAX_FREQUENCE,
                    });
                    i += 1;
                }
                CharType::Other => {
                    self.placeholder(d, PH_STRING, i as i32, i as i32 + 1, WordType::String);
                    i += 1;
                }
            }
        }
        self.placeholder(d, PH_START, -1, 0, WordType::SentenceBegin);
        self.placeholder(d, PH_END, length as i32, length as i32 + 1, WordType::SentenceEnd);
    }

    /// `process`: the shortest path through the bigram graph (`BiSegGraph`), as token ids.
    fn shortest_path(&mut self, d: &Dict) -> Vec<u32> {
        // makeIndex: by start, then insertion order.
        self.order.clear();
        self.index.clear();
        self.index.resize(self.tokens.len(), 0);
        for l in &self.by_start {
            for &t in l {
                self.index[t as usize] = self.order.len() as u32;
                self.order.push(t);
            }
        }
        let n = self.order.len();
        if self.edges.len() < n {
            self.edges.resize_with(n, Vec::new);
        }
        for e in self.edges[..n].iter_mut() {
            e.clear();
        }
        // generateBiSegGraph
        let smooth = 0.1f64;
        let tiny = 1.0 / MAX_FREQUENCE as f64;
        let max_slot = self.by_start.iter().rposition(|l| !l.is_empty()).unwrap_or(0);
        let mut id = std::mem::take(&mut self.id);
        for key in 0..max_slot {
            for k1 in 0..self.by_start[key].len() {
                let t1 = self.tokens[self.by_start[key][k1] as usize];
                let one = t1.weight as f64;
                let mut next = (t1.end + 1) as usize;
                while next <= max_slot && self.by_start[next].is_empty() {
                    next += 1;
                }
                if next > max_slot {
                    break;
                }
                let from = self.index[self.by_start[key][k1] as usize];
                for k2 in 0..self.by_start[next].len() {
                    let t2id = self.by_start[next][k2] as usize;
                    let t2 = self.tokens[t2id];
                    id.clear();
                    id.extend_from_slice(self.chars(t1.chars));
                    id.push(WORD_SEGMENT_CHAR);
                    id.extend_from_slice(self.chars(t2.chars));
                    let pair = d.bigram_frequency(&id) as f64;
                    let weight = -(smooth * (1.0 + one) / (MAX_FREQUENCE as f64 + 0.0)
                        + (1.0 - smooth) * ((1.0 - tiny) * pair / (1.0 + one) + tiny))
                        .ln();
                    let to = self.index[t2id] as usize;
                    self.edges[to].push((from, weight));
                }
            }
        }
        self.id = id;
        // getShortPath: nodes 1..=count of indices with incoming edges.
        let node_count = self.edges[..n].iter().filter(|e| !e.is_empty()).count();
        self.path.clear();
        self.path.push((0.0, 0));
        for current in 1..=node_count {
            let mut min = f64::MAX;
            let mut pre = 0;
            for &(from, w) in &self.edges[current] {
                let pw = self.path[from as usize].0;
                if pw + w < min {
                    min = pw + w;
                    pre = from;
                }
            }
            self.path.push((min, pre));
        }
        let mut rpath = vec![(self.path.len() - 1) as u32];
        let mut current = self.path.len() - 1;
        while current != 0 {
            current = self.path[current].1 as usize;
            rpath.push(current as u32);
        }
        rpath.iter().rev().map(|&i| self.order[i as usize]).collect()
    }

    /// `WordSegmenter.segmentSentence`: the sentence's words (without the sentence start and
    /// end), offsets from `base`.
    fn segment_sentence(&mut self, d: &Dict, s: &[u16], base: i32, out: &mut std::collections::VecDeque<Word>) {
        self.graph(d, s);
        let path = self.shortest_path(d);
        if path.len() <= 2 {
            return;
        }
        for &t in &path[1..path.len() - 1] {
            let st = self.tokens[t as usize];
            let mut chars: Vec<u16> = match st.word_type {
                WordType::String | WordType::Number | WordType::FullwidthNumber | WordType::FullwidthString => {
                    s[st.start as usize..st.end as usize].to_vec()
                }
                WordType::Delimiter => vec![b',' as u16],
                _ => self.chars(st.chars).to_vec(),
            };
            match st.word_type {
                WordType::FullwidthNumber | WordType::FullwidthString => {
                    for c in chars.iter_mut() {
                        if *c >= 0xFF10 {
                            *c -= 0xFEE0;
                        }
                        if (0x41..=0x5A).contains(c) {
                            *c += 0x20;
                        }
                    }
                }
                WordType::String => {
                    for c in chars.iter_mut() {
                        if (0x41..=0x5A).contains(c) {
                            *c += 0x20;
                        }
                    }
                }
                _ => {}
            }
            out.push_back(Word {
                chars,
                start: st.start + base,
                end: st.end + base,
            });
        }
    }
}

/// The `smartcn_tokenizer` (`HMMChineseTokenizer` over `SegmentingTokenizerBase`).
pub(crate) struct ChineseTokenizer {
    buf: Vec<u16>,
    offset: usize,
    length: usize,
    usable: usize,
    sentence_pos: usize,
    words: std::collections::VecDeque<Word>,
    seg: Segmenter,
}

impl ChineseTokenizer {
    pub fn new() -> ChineseTokenizer {
        ChineseTokenizer {
            buf: Vec::new(),
            offset: 0,
            length: 0,
            usable: 0,
            sentence_pos: 0,
            words: Default::default(),
            seg: Segmenter::default(),
        }
    }

    /// `SegmentingTokenizerBase.refill`.
    fn refill(&mut self) {
        self.offset += self.usable;
        let leftover = self.length - self.usable;
        let requested = BUFFER - leftover;
        let available = self.buf.len() - (self.offset + leftover);
        let returned = requested.min(available);
        self.length = leftover + returned;
        self.usable = if returned < requested {
            self.length
        } else {
            let chunk = &self.buf[self.offset..self.offset + self.length];
            chunk.iter().rposition(|&u| is_safe_end(u)).map_or(self.length, |i| i + 1)
        };
        self.sentence_pos = 0;
    }

    /// `incrementSentence`.
    fn increment_sentence(&mut self) -> bool {
        if self.length == 0 {
            return false;
        }
        let d = dict();
        loop {
            let start = self.sentence_pos;
            if start >= self.usable {
                return false;
            }
            let chunk = &self.buf[self.offset..self.offset + self.usable];
            let end = sentence_next(chunk, start);
            self.sentence_pos = end;
            self.seg.segment_sentence(d, &chunk[start..end], (self.offset + start) as i32, &mut self.words);
            if !self.words.is_empty() {
                return true;
            }
        }
    }
}

impl Tokenizer for ChineseTokenizer {
    fn reset(&mut self, text: &str) {
        self.buf.clear();
        self.buf.extend(text.encode_utf16());
        self.offset = 0;
        self.length = 0;
        self.usable = 0;
        self.sentence_pos = 0;
        self.words.clear();
    }

    fn next(&mut self, _text: &str, t: &mut Tok) -> bool {
        if self.length == 0 || self.words.is_empty() {
            while !self.increment_sentence() {
                self.refill();
                if self.length == 0 {
                    return false;
                }
            }
        }
        let w = self.words.pop_front().expect("a word");
        crate::morph::push_utf16(&mut t.term, &w.chars);
        t.start = w.start as u32;
        t.end = w.end as u32;
        true
    }

    fn end(&mut self, _text: &str, t: &mut Tok) {
        let end = (self.offset + self.length) as u32;
        t.start = end;
        t.end = end;
    }
}

/// `SmartChineseAnalyzer`'s default stop words (`//` comments).
pub(crate) fn stop_word_list() -> Vec<&'static str> {
    include_str!("stopwords.txt")
        .lines()
        .filter(|l| !l.starts_with("//"))
        .map(|l| l.trim_matches(|c: char| c <= ' '))
        .collect()
}

pub(crate) fn default_stop_words() -> &'static WordSet {
    static S: OnceLock<WordSet> = OnceLock::new();
    S.get_or_init(|| WordSet::new(stop_word_list(), false))
}
