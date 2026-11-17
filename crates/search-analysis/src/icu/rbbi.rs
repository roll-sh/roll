//! ICU's rule-based break iterator, forward iteration only, ported from ICU4J 77.1's
//! `RuleBasedBreakIterator` (`handleNext`, the break cache's `populateFollowing` and the
//! `DictionaryCache`; Unicode-3.0) over compiled `.brk` rules (`RBBIDataWrapper`).

use std::sync::OnceLock;

use super::dict::{DequeI, Engine, Iter};
use super::trie::{CpTrie, be16, be32, header};

const ACCEPTING: usize = 0;
const LOOKAHEAD: usize = 1;
const TAGSIDX: usize = 2;
const NEXTSTATES: usize = 3;
const STOP_STATE: usize = 0;
const START_STATE: usize = 1;
const RBBI_BOF_REQUIRED: u32 = 2;
const RBBI_8BITS_ROWS: u32 = 4;

/// Compiled break rules.
pub(crate) struct Rules {
    cat_count: usize,
    table: &'static [u8],
    eight_bit: bool,
    flags: u32,
    dict_start: u32,
    lookahead_size: usize,
    trie: CpTrie<'static>,
    status: Vec<i32>,
}

impl Rules {
    fn load(bytes: &'static [u8]) -> Rules {
        let (format, _, b) = header(bytes).expect("ICU break rules");
        assert!(&format == b"Brk ");
        let h = |i: usize| be32(b, i) as usize;
        assert_eq!(h(0), 0xb1a0);
        let cat_count = h(3);
        let (ft, ft_len) = (h(4), h(5));
        let (trie_off, trie_len) = (h(8), h(9));
        let (st, st_len) = (h(12), h(13));
        let f = &b[ft..ft + ft_len];
        let flags = be32(f, 4);
        let (trie, _) = CpTrie::parse(&b[trie_off..trie_off + trie_len]).expect("break rules trie");
        let status = (0..st_len / 4).map(|i| be32(&b[st..], i) as i32).collect();
        Rules {
            cat_count,
            table: &f[20..],
            eight_bit: flags & RBBI_8BITS_ROWS != 0,
            flags,
            dict_start: be32(f, 2),
            lookahead_size: be32(f, 3) as usize,
            trie,
            status,
        }
    }

    fn cell(&self, i: usize) -> usize {
        if self.eight_bit { self.table[i] as usize } else { be16(self.table, i) as usize }
    }

    fn row(&self, state: usize) -> usize {
        state * (self.cat_count + NEXTSTATES)
    }

    /// `getRuleStatus`: the largest value of a rule status entry.
    pub(crate) fn status_value(&self, idx: usize) -> i32 {
        let n = self.status.get(idx).copied().unwrap_or(0).max(0) as usize;
        self.status.get(idx + n).copied().unwrap_or(0)
    }

    /// Lucene's `Default.brk` (the ICU tokenizer's rules for most scripts).
    pub(crate) fn lucene_default() -> &'static Rules {
        static R: OnceLock<Rules> = OnceLock::new();
        R.get_or_init(|| Rules::load(include_bytes!("data/Default.brk")))
    }

    /// ICU's root word rules (`BreakIterator.getWordInstance(ULocale.ROOT)`).
    pub(crate) fn icu_word() -> &'static Rules {
        static R: OnceLock<Rules> = OnceLock::new();
        R.get_or_init(|| Rules::load(include_bytes!("data/word.brk")))
    }
}

/// A forward break iterator over one text.
pub(crate) struct BreakIter<'a> {
    rules: &'static Rules,
    text: &'a [u16],
    /// The current boundary and its rule status index.
    pos: usize,
    status_idx: usize,
    /// Boundaries computed ahead (`BreakCache`).
    ahead: std::collections::VecDeque<(usize, usize)>,
    dict_chars: usize,
    dict: DictCache,
    lookahead: Vec<i64>,
}

#[derive(Default)]
struct DictCache {
    breaks: DequeI,
    position: i64,
    start: usize,
    limit: usize,
    other_status: usize,
}

impl DictCache {
    fn reset(&mut self) {
        self.position = -1;
        self.start = 0;
        self.limit = 0;
        self.other_status = 0;
        self.breaks.clear();
    }

    /// `DictionaryCache.following`.
    fn following(&mut self, from: usize) -> Option<(usize, usize)> {
        if from >= self.limit || from < self.start {
            self.position = -1;
            return None;
        }
        let n = self.breaks.size() as i64;
        if self.position >= 0 && self.position < n && self.breaks.element_at(self.position as usize) == from as i32 {
            self.position += 1;
            if self.position >= n {
                self.position = -1;
                return None;
            }
            return Some((self.breaks.element_at(self.position as usize) as usize, self.other_status));
        }
        self.position = 0;
        while self.position < n {
            let r = self.breaks.element_at(self.position as usize);
            if r > from as i32 {
                return Some((r as usize, self.other_status));
            }
            self.position += 1;
        }
        self.position = -1;
        None
    }
}

const DONE32: u32 = u32::MAX;

impl<'a> BreakIter<'a> {
    pub(crate) fn new(rules: &'static Rules, text: &'a [u16]) -> BreakIter<'a> {
        let mut d = DictCache::default();
        d.reset();
        BreakIter { rules, text, pos: 0, status_idx: 0, ahead: Default::default(), dict_chars: 0, dict: d, lookahead: vec![0; rules.lookahead_size.max(1)] }
    }

    pub(crate) fn current(&self) -> usize {
        self.pos
    }

    /// `getRuleStatus` of the current boundary.
    pub(crate) fn rule_status(&self) -> i32 {
        self.rules.status_value(self.status_idx)
    }

    fn cp_at(&self, i: usize) -> (u32, usize) {
        if i >= self.text.len() {
            return (DONE32, 0);
        }
        let c = self.text[i] as u32;
        if (0xd800..0xdc00).contains(&c) && i + 1 < self.text.len() {
            let t = self.text[i + 1] as u32;
            if (0xdc00..0xe000).contains(&t) {
                return (0x10000 + ((c - 0xd800) << 10) + (t - 0xdc00), 2);
            }
        }
        (c, 1)
    }

    /// `handleNext`: the next rule boundary after `from`, with its status index; None at the
    /// end.
    fn handle_next(&mut self, from: usize) -> Option<(usize, usize)> {
        let r = self.rules;
        self.dict_chars = 0;
        let mut status_idx = 0;
        let (mut c, mut clen) = self.cp_at(from);
        if c == DONE32 {
            return None;
        }
        let mut p = from;
        let mut state = START_STATE;
        let mut row = r.row(state);
        let mut category: usize = 3;
        const RUN: u8 = 0;
        const START: u8 = 1;
        const END: u8 = 2;
        let mut mode = RUN;
        if r.flags & RBBI_BOF_REQUIRED != 0 {
            category = 2;
            mode = START;
        }
        let mut result = from;
        while state != STOP_STATE {
            if c == DONE32 {
                if mode == END {
                    break;
                }
                mode = END;
                category = 1;
            } else if mode == RUN {
                category = r.trie.get(c) as u16 as usize;
                if category as u32 >= r.dict_start {
                    self.dict_chars += 1;
                }
                p += clen;
                (c, clen) = self.cp_at(p);
            } else {
                mode = RUN;
            }
            state = r.cell(row + NEXTSTATES + category);
            row = r.row(state);
            let accepting = r.cell(row + ACCEPTING);
            if accepting == 1 {
                result = p;
                status_idx = r.cell(row + TAGSIDX);
            } else if accepting > 1 {
                let la = self.lookahead.get(accepting).copied().unwrap_or(-1);
                if la >= 0 {
                    return Some((la as usize, r.cell(row + TAGSIDX)));
                }
            }
            let rule = r.cell(row + LOOKAHEAD);
            if rule != 0 && rule < self.lookahead.len() {
                self.lookahead[rule] = p as i64;
            }
        }
        if result == from {
            let (_, n) = self.cp_at(from);
            result = from + n.max(1);
            status_idx = 0;
        }
        Some((result, status_idx))
    }

    /// `next()`: the next boundary, or None at the end.
    pub(crate) fn next(&mut self) -> Option<usize> {
        if let Some((b, s)) = self.ahead.pop_front() {
            self.pos = b;
            self.status_idx = s;
            return Some(b);
        }
        let from = self.pos;
        let from_status = self.status_idx;
        if let Some((b, s)) = self.dict.following(from) {
            self.pos = b;
            self.status_idx = s;
            return Some(b);
        }
        let (pos, status) = self.handle_next(from)?;
        if self.dict_chars > 0 {
            self.populate_dictionary(from, pos, from_status, status);
            if let Some((b, s)) = self.dict.following(from) {
                self.pos = b;
                self.status_idx = s;
                return Some(b);
            }
        }
        self.pos = pos;
        self.status_idx = status;
        // `populateFollowing` reads up to six more rule boundaries ahead, stopping at one that
        // holds dictionary characters (which it will split when it gets there).
        let mut at = pos;
        for _ in 0..6 {
            let Some((p, s)) = self.handle_next(at) else { break };
            if self.dict_chars > 0 {
                break;
            }
            self.ahead.push_back((p, s));
            at = p;
        }
        Some(pos)
    }

    /// `DictionaryCache.populateDictionary`.
    fn populate_dictionary(&mut self, start: usize, end: usize, _first_status: usize, other_status: usize) {
        if end - start <= 1 {
            return;
        }
        self.dict.reset();
        self.dict.other_status = other_status;
        let mut it = Iter { s: self.text, i: start };
        let mut c = it.current32();
        let r = self.rules;
        let cat = |c: u32| if c == 0x7fffffff { 0 } else { r.trie.get(c) as u16 as u32 };
        let mut category = cat(c);
        let mut found = 0;
        loop {
            while it.i < end && category < r.dict_start {
                c = it.next32();
                category = cat(c);
            }
            if it.i >= end {
                break;
            }
            match Engine::for_char(c) {
                Some(e) => found += e.find_breaks(&mut it, end, &mut self.dict.breaks),
                None => {
                    // `UnhandledBreakEngine`: skips the run of characters of that script.
                    it.next32();
                }
            }
            c = it.current32();
            category = cat(c);
        }
        if found > 0 {
            if (start as i32) < self.dict.breaks.element_at(0) {
                self.dict.breaks.offer(start as i32);
            }
            if (end as i32) > self.dict.breaks.peek() {
                self.dict.breaks.push(end as i32);
            }
            self.dict.position = 0;
            self.dict.start = self.dict.breaks.element_at(0) as usize;
            self.dict.limit = self.dict.breaks.peek() as usize;
        }
    }
}
