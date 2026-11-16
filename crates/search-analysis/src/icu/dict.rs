//! ICU's dictionary break engines, ported from ICU4J 77.1 (`com.ibm.icu.impl.breakiter`:
//! `DictionaryBreakEngine`, `ThaiBreakEngine`, `LaoBreakEngine`, `BurmeseBreakEngine`,
//! `KhmerBreakEngine`, `CjkBreakEngine`; `BytesTrie` and `CharsTrie`; Unicode-3.0), over ICU's
//! dictionaries (`thaidict`, `laodict`, `burmesedict`, `khmerdict`, `cjdict`), read in place.
//!
//! Positions are UTF-16 offsets into the text a break iterator runs over, as in Java.

use std::sync::OnceLock;

use super::norm::{Data, Normalizer};
use super::trie::{be16, header};
use super::uset::USet;

/// `CharacterIterator.DONE` and `CharacterIteration.DONE32`.
const DONE: u32 = 0xffff;
const DONE32: u32 = 0x7fffffff;

/// A `CharacterIterator` over a slice (begin 0, end `s.len()`).
#[derive(Clone)]
pub(crate) struct Iter<'a> {
    pub s: &'a [u16],
    pub i: usize,
}

impl Iter<'_> {
    pub fn current(&self) -> u32 {
        self.s.get(self.i).map_or(DONE, |&u| u as u32)
    }
    pub fn next(&mut self) -> u32 {
        if self.i + 1 < self.s.len() {
            self.i += 1;
            self.s[self.i] as u32
        } else {
            self.i = self.s.len();
            DONE
        }
    }
    pub fn previous(&mut self) -> u32 {
        if self.i > 0 {
            self.i -= 1;
            self.s[self.i] as u32
        } else {
            DONE
        }
    }
    /// `CharacterIteration.current32`.
    pub fn current32(&self) -> u32 {
        let c = self.current();
        if (0xd800..0xdc00).contains(&c) && self.i + 1 < self.s.len() {
            let t = self.s[self.i + 1] as u32;
            if (0xdc00..0xe000).contains(&t) {
                return 0x10000 + ((c - 0xd800) << 10) + (t - 0xdc00);
            }
        }
        if c == DONE && self.i >= self.s.len() { DONE32 } else { c }
    }
    /// `CharacterIteration.next32`: past the current code point, returning the next one.
    pub fn next32(&mut self) -> u32 {
        let c = self.current();
        if (0xd800..0xdc00).contains(&c) {
            let t = self.next();
            if !(0xdc00..0xe000).contains(&t) {
                self.previous();
            }
        }
        self.next();
        self.current32()
    }
    /// `UCharacterIterator.nextCodePoint` (on a copy, as the matchers use one).
    fn next_code_point(&mut self) -> Option<u32> {
        if self.i >= self.s.len() {
            return None;
        }
        let c = self.current32();
        self.i += if c > 0xffff { 2 } else { 1 };
        Some(c)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TrieResult {
    NoMatch,
    NoValue,
    Final,
    Intermediate,
}

impl TrieResult {
    fn has_value(self) -> bool {
        matches!(self, TrieResult::Final | TrieResult::Intermediate)
    }
    fn has_next(self) -> bool {
        matches!(self, TrieResult::NoValue | TrieResult::Intermediate)
    }
}

/// `BytesTrie` (the reading side).
struct BytesTrie<'a> {
    b: &'a [u8],
    pos: i64,
    remaining: i64,
}

const B_MAX_BRANCH_LINEAR: usize = 5;
const B_MIN_LINEAR_MATCH: u32 = 0x10;
const B_MIN_VALUE_LEAD: u32 = 0x20;
const B_MIN_ONE_BYTE_VALUE_LEAD: u32 = 0x10;
const B_MIN_TWO_BYTE_VALUE_LEAD: u32 = 0x51;
const B_MIN_THREE_BYTE_VALUE_LEAD: u32 = 0x6c;
const B_FOUR_BYTE_VALUE_LEAD: u32 = 0x7e;
const B_MIN_TWO_BYTE_DELTA_LEAD: u32 = 0xc0;
const B_MIN_THREE_BYTE_DELTA_LEAD: u32 = 0xf0;
const B_FOUR_BYTE_DELTA_LEAD: u32 = 0xfe;

impl<'a> BytesTrie<'a> {
    fn at(&self, p: usize) -> u32 {
        self.b[p] as u32
    }
    fn value_result(node: u32) -> TrieResult {
        if node & 1 != 0 { TrieResult::Final } else { TrieResult::Intermediate }
    }
    fn read_value(&self, pos: usize, lead: u32) -> i32 {
        let b = |i: usize| self.at(pos + i);
        if lead < B_MIN_TWO_BYTE_VALUE_LEAD {
            lead as i32 - B_MIN_ONE_BYTE_VALUE_LEAD as i32
        } else if lead < B_MIN_THREE_BYTE_VALUE_LEAD {
            (((lead - B_MIN_TWO_BYTE_VALUE_LEAD) << 8) | b(0)) as i32
        } else if lead < B_FOUR_BYTE_VALUE_LEAD {
            (((lead - B_MIN_THREE_BYTE_VALUE_LEAD) << 16) | (b(0) << 8) | b(1)) as i32
        } else if lead == B_FOUR_BYTE_VALUE_LEAD {
            ((b(0) << 16) | (b(1) << 8) | b(2)) as i32
        } else {
            ((b(0) << 24) | (b(1) << 16) | (b(2) << 8) | b(3)) as i32
        }
    }
    fn skip_value(pos: usize, lead: u32) -> usize {
        if lead >= (B_MIN_TWO_BYTE_VALUE_LEAD << 1) {
            if lead < (B_MIN_THREE_BYTE_VALUE_LEAD << 1) {
                return pos + 1;
            } else if lead < (B_FOUR_BYTE_VALUE_LEAD << 1) {
                return pos + 2;
            } else {
                return pos + 3 + ((lead >> 1) & 1) as usize;
            }
        }
        pos
    }
    fn skip_value_at(&self, pos: usize) -> usize {
        let lead = self.at(pos);
        Self::skip_value(pos + 1, lead)
    }
    fn jump_by_delta(&self, mut pos: usize) -> usize {
        let mut delta = self.at(pos);
        pos += 1;
        if delta < B_MIN_TWO_BYTE_DELTA_LEAD {
        } else if delta < B_MIN_THREE_BYTE_DELTA_LEAD {
            delta = ((delta - B_MIN_TWO_BYTE_DELTA_LEAD) << 8) | self.at(pos);
            pos += 1;
        } else if delta < B_FOUR_BYTE_DELTA_LEAD {
            delta = ((delta - B_MIN_THREE_BYTE_DELTA_LEAD) << 16) | (self.at(pos) << 8) | self.at(pos + 1);
            pos += 2;
        } else if delta == B_FOUR_BYTE_DELTA_LEAD {
            delta = (self.at(pos) << 16) | (self.at(pos + 1) << 8) | self.at(pos + 2);
            pos += 3;
        } else {
            delta = (self.at(pos) << 24) | (self.at(pos + 1) << 16) | (self.at(pos + 2) << 8) | self.at(pos + 3);
            pos += 4;
        }
        pos + delta as usize
    }
    fn skip_delta(&self, mut pos: usize) -> usize {
        let delta = self.at(pos);
        pos += 1;
        if delta >= B_MIN_TWO_BYTE_DELTA_LEAD {
            if delta < B_MIN_THREE_BYTE_DELTA_LEAD {
                pos += 1;
            } else if delta < B_FOUR_BYTE_DELTA_LEAD {
                pos += 2;
            } else {
                pos += 3 + (delta & 1) as usize;
            }
        }
        pos
    }
    fn stop(&mut self) -> TrieResult {
        self.pos = -1;
        TrieResult::NoMatch
    }
    fn branch_next(&mut self, mut pos: usize, mut length: usize, in_byte: u32) -> TrieResult {
        if length == 0 {
            length = self.at(pos) as usize;
            pos += 1;
        }
        length += 1;
        while length > B_MAX_BRANCH_LINEAR {
            let cmp = self.at(pos);
            pos += 1;
            if in_byte < cmp {
                length >>= 1;
                pos = self.jump_by_delta(pos);
            } else {
                length -= length >> 1;
                pos = self.skip_delta(pos);
            }
        }
        loop {
            let cmp = self.at(pos);
            pos += 1;
            if in_byte == cmp {
                let mut node = self.at(pos);
                let result = if node & 1 != 0 {
                    TrieResult::Final
                } else {
                    pos += 1;
                    node >>= 1;
                    let b = |i: usize| self.at(pos + i);
                    let (delta, n) = if node < B_MIN_TWO_BYTE_VALUE_LEAD {
                        (node - B_MIN_ONE_BYTE_VALUE_LEAD, 0)
                    } else if node < B_MIN_THREE_BYTE_VALUE_LEAD {
                        (((node - B_MIN_TWO_BYTE_VALUE_LEAD) << 8) | b(0), 1)
                    } else if node < B_FOUR_BYTE_VALUE_LEAD {
                        (((node - B_MIN_THREE_BYTE_VALUE_LEAD) << 16) | (b(0) << 8) | b(1), 2)
                    } else if node == B_FOUR_BYTE_VALUE_LEAD {
                        ((b(0) << 16) | (b(1) << 8) | b(2), 3)
                    } else {
                        ((b(0) << 24) | (b(1) << 16) | (b(2) << 8) | b(3), 4)
                    };
                    pos += n + delta as usize;
                    let node = self.at(pos);
                    if node >= B_MIN_VALUE_LEAD { Self::value_result(node) } else { TrieResult::NoValue }
                };
                self.pos = pos as i64;
                return result;
            }
            length -= 1;
            pos = self.skip_value_at(pos);
            if length <= 1 {
                break;
            }
        }
        if in_byte == self.at(pos) {
            pos += 1;
            self.pos = pos as i64;
            let node = self.at(pos);
            if node >= B_MIN_VALUE_LEAD { Self::value_result(node) } else { TrieResult::NoValue }
        } else {
            self.stop()
        }
    }
    fn next_impl(&mut self, mut pos: usize, in_byte: u32) -> TrieResult {
        loop {
            let node = self.at(pos);
            pos += 1;
            if node < B_MIN_LINEAR_MATCH {
                return self.branch_next(pos, node as usize, in_byte);
            } else if node < B_MIN_VALUE_LEAD {
                let mut length = node as i64 - B_MIN_LINEAR_MATCH as i64;
                if in_byte == self.at(pos) {
                    pos += 1;
                    length -= 1;
                    self.remaining = length;
                    self.pos = pos as i64;
                    let n = self.at(pos);
                    return if length < 0 && n >= B_MIN_VALUE_LEAD { Self::value_result(n) } else { TrieResult::NoValue };
                }
                break;
            } else if node & 1 != 0 {
                break;
            } else {
                pos = Self::skip_value(pos, node);
            }
        }
        self.stop()
    }
    fn first(&mut self, in_byte: i32) -> TrieResult {
        self.remaining = -1;
        let in_byte = if in_byte < 0 { in_byte + 0x100 } else { in_byte } as u32;
        self.next_impl(0, in_byte)
    }
    fn next(&mut self, in_byte: i32) -> TrieResult {
        if self.pos < 0 {
            return TrieResult::NoMatch;
        }
        let mut pos = self.pos as usize;
        let in_byte = if in_byte < 0 { in_byte + 0x100 } else { in_byte } as u32;
        let mut length = self.remaining;
        if length >= 0 {
            if in_byte == self.at(pos) {
                pos += 1;
                length -= 1;
                self.remaining = length;
                self.pos = pos as i64;
                let n = self.at(pos);
                return if length < 0 && n >= B_MIN_VALUE_LEAD { Self::value_result(n) } else { TrieResult::NoValue };
            }
            return self.stop();
        }
        self.next_impl(pos, in_byte)
    }
    fn value(&self) -> i32 {
        let pos = self.pos as usize;
        let lead = self.at(pos);
        self.read_value(pos + 1, lead >> 1)
    }
}

/// `CharsTrie` (the reading side), over big-endian UTF-16 units.
struct CharsTrie<'a> {
    c: &'a [u8],
    pos: i64,
    remaining: i64,
}

const C_MAX_BRANCH_LINEAR: usize = 5;
const C_MIN_LINEAR_MATCH: u32 = 0x30;
const C_MIN_VALUE_LEAD: u32 = 0x40;
const C_NODE_TYPE_MASK: u32 = 0x3f;
const C_VALUE_IS_FINAL: u32 = 0x8000;
const C_MIN_TWO_UNIT_VALUE_LEAD: u32 = 0x4000;
const C_THREE_UNIT_VALUE_LEAD: u32 = 0x7fff;
const C_MIN_TWO_UNIT_NODE_VALUE_LEAD: u32 = 0x4040;
const C_THREE_UNIT_NODE_VALUE_LEAD: u32 = 0x7fc0;
const C_MIN_TWO_UNIT_DELTA_LEAD: u32 = 0xfc00;
const C_THREE_UNIT_DELTA_LEAD: u32 = 0xffff;

impl<'a> CharsTrie<'a> {
    fn at(&self, p: usize) -> u32 {
        be16(self.c, p) as u32
    }
    fn value_result(node: u32) -> TrieResult {
        if node & C_VALUE_IS_FINAL != 0 { TrieResult::Final } else { TrieResult::Intermediate }
    }
    fn skip_node_value(pos: usize, lead: u32) -> usize {
        if lead >= C_MIN_TWO_UNIT_NODE_VALUE_LEAD {
            if lead < C_THREE_UNIT_NODE_VALUE_LEAD { pos + 1 } else { pos + 2 }
        } else {
            pos
        }
    }
    fn skip_value_at(&self, pos: usize) -> usize {
        let lead = self.at(pos) & 0x7fff;
        let pos = pos + 1;
        if lead >= C_MIN_TWO_UNIT_VALUE_LEAD {
            if lead < C_THREE_UNIT_VALUE_LEAD { pos + 1 } else { pos + 2 }
        } else {
            pos
        }
    }
    fn jump_by_delta(&self, mut pos: usize) -> usize {
        let mut delta = self.at(pos);
        pos += 1;
        if delta >= C_MIN_TWO_UNIT_DELTA_LEAD {
            if delta == C_THREE_UNIT_DELTA_LEAD {
                delta = (self.at(pos) << 16) | self.at(pos + 1);
                pos += 2;
            } else {
                delta = ((delta - C_MIN_TWO_UNIT_DELTA_LEAD) << 16) | self.at(pos);
                pos += 1;
            }
        }
        pos + delta as usize
    }
    fn skip_delta(&self, mut pos: usize) -> usize {
        let delta = self.at(pos);
        pos += 1;
        if delta >= C_MIN_TWO_UNIT_DELTA_LEAD {
            pos += if delta == C_THREE_UNIT_DELTA_LEAD { 2 } else { 1 };
        }
        pos
    }
    fn stop(&mut self) -> TrieResult {
        self.pos = -1;
        TrieResult::NoMatch
    }
    fn branch_next(&mut self, mut pos: usize, mut length: usize, u: u32) -> TrieResult {
        if length == 0 {
            length = self.at(pos) as usize;
            pos += 1;
        }
        length += 1;
        while length > C_MAX_BRANCH_LINEAR {
            let cmp = self.at(pos);
            pos += 1;
            if u < cmp {
                length >>= 1;
                pos = self.jump_by_delta(pos);
            } else {
                length -= length >> 1;
                pos = self.skip_delta(pos);
            }
        }
        loop {
            let cmp = self.at(pos);
            pos += 1;
            if u == cmp {
                let node = self.at(pos);
                let result = if node & C_VALUE_IS_FINAL != 0 {
                    TrieResult::Final
                } else {
                    pos += 1;
                    let delta = if node < C_MIN_TWO_UNIT_VALUE_LEAD {
                        node
                    } else if node < C_THREE_UNIT_VALUE_LEAD {
                        let d = ((node - C_MIN_TWO_UNIT_VALUE_LEAD) << 16) | self.at(pos);
                        pos += 1;
                        d
                    } else {
                        let d = (self.at(pos) << 16) | self.at(pos + 1);
                        pos += 2;
                        d
                    };
                    pos += delta as usize;
                    let node = self.at(pos);
                    if node >= C_MIN_VALUE_LEAD { Self::value_result(node) } else { TrieResult::NoValue }
                };
                self.pos = pos as i64;
                return result;
            }
            length -= 1;
            pos = self.skip_value_at(pos);
            if length <= 1 {
                break;
            }
        }
        if u == self.at(pos) {
            pos += 1;
            self.pos = pos as i64;
            let node = self.at(pos);
            if node >= C_MIN_VALUE_LEAD { Self::value_result(node) } else { TrieResult::NoValue }
        } else {
            self.stop()
        }
    }
    fn next_impl(&mut self, mut pos: usize, u: u32) -> TrieResult {
        let mut node = self.at(pos);
        pos += 1;
        loop {
            if node < C_MIN_LINEAR_MATCH {
                return self.branch_next(pos, node as usize, u);
            } else if node < C_MIN_VALUE_LEAD {
                let mut length = node as i64 - C_MIN_LINEAR_MATCH as i64;
                if u == self.at(pos) {
                    pos += 1;
                    length -= 1;
                    self.remaining = length;
                    self.pos = pos as i64;
                    let n = self.at(pos);
                    return if length < 0 && n >= C_MIN_VALUE_LEAD { Self::value_result(n) } else { TrieResult::NoValue };
                }
                break;
            } else if node & C_VALUE_IS_FINAL != 0 {
                break;
            } else {
                pos = Self::skip_node_value(pos, node);
                node &= C_NODE_TYPE_MASK;
            }
        }
        self.stop()
    }
    fn first(&mut self, u: u32) -> TrieResult {
        self.remaining = -1;
        self.next_impl(0, u)
    }
    fn next(&mut self, u: u32) -> TrieResult {
        if self.pos < 0 {
            return TrieResult::NoMatch;
        }
        let mut pos = self.pos as usize;
        let mut length = self.remaining;
        if length >= 0 {
            if u == self.at(pos) {
                pos += 1;
                length -= 1;
                self.remaining = length;
                self.pos = pos as i64;
                let n = self.at(pos);
                return if length < 0 && n >= C_MIN_VALUE_LEAD { Self::value_result(n) } else { TrieResult::NoValue };
            }
            return self.stop();
        }
        self.next_impl(pos, u)
    }
    fn split(cp: u32) -> (u32, u32) {
        (0xd800 + ((cp - 0x10000) >> 10), 0xdc00 + ((cp - 0x10000) & 0x3ff))
    }
    fn first_cp(&mut self, cp: u32) -> TrieResult {
        if cp <= 0xffff {
            return self.first(cp);
        }
        let (l, t) = Self::split(cp);
        if self.first(l).has_next() { self.next(t) } else { TrieResult::NoMatch }
    }
    fn next_cp(&mut self, cp: u32) -> TrieResult {
        if cp <= 0xffff {
            return self.next(cp);
        }
        let (l, t) = Self::split(cp);
        if self.next(l).has_next() { self.next(t) } else { TrieResult::NoMatch }
    }
    fn value(&self) -> i32 {
        let pos = self.pos as usize;
        let lead = self.at(pos);
        let p = pos + 1;
        if lead & C_VALUE_IS_FINAL != 0 {
            let lead = lead & 0x7fff;
            if lead < C_MIN_TWO_UNIT_VALUE_LEAD {
                lead as i32
            } else if lead < C_THREE_UNIT_VALUE_LEAD {
                (((lead - C_MIN_TWO_UNIT_VALUE_LEAD) << 16) | self.at(p)) as i32
            } else {
                ((self.at(p) << 16) | self.at(p + 1)) as i32
            }
        } else if lead < C_MIN_TWO_UNIT_NODE_VALUE_LEAD {
            (lead >> 6) as i32 - 1
        } else if lead < C_THREE_UNIT_NODE_VALUE_LEAD {
            ((((lead & 0x7fc0) - C_MIN_TWO_UNIT_NODE_VALUE_LEAD) << 10) | self.at(p)) as i32
        } else {
            ((self.at(p) << 16) | self.at(p + 1)) as i32
        }
    }
}

/// A dictionary (`DictionaryData`): a bytes trie with an offset transform, or a UTF-16 trie.
pub(crate) enum Dictionary {
    Bytes { data: &'static [u8], transform: u32 },
    Chars { data: &'static [u8] },
}

impl Dictionary {
    fn load(bytes: &'static [u8]) -> Dictionary {
        let (format, _, b) = header(bytes).expect("ICU dictionary data");
        assert!(&format == b"Dict");
        let ix = |i: usize| u32::from_be_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        let offset = ix(0) as usize;
        let total = ix(3) as usize;
        let data = &b[offset..total];
        match ix(4) & 7 {
            0 => Dictionary::Bytes { data, transform: ix(5) },
            _ => Dictionary::Chars { data },
        }
    }

    fn transform(&self, c: u32) -> i32 {
        let Dictionary::Bytes { transform, .. } = self else { return -1 };
        if c == 0x200d {
            return 0xff;
        } else if c == 0x200c {
            return 0xfe;
        }
        let delta = c as i64 - (transform & 0x1fffff) as i64;
        if !(0..=0xfd).contains(&delta) { -1 } else { delta as i32 }
    }

    /// `DictionaryMatcher.matches`: the lengths (in code points) of the dictionary words that
    /// start at `text`'s position, and their values; returns how far it looked.
    pub(crate) fn matches(&self, text: &Iter, max_length: usize, lengths: &mut [usize], count: &mut usize, limit: usize, mut values: Option<&mut [i32]>) -> usize {
        let mut it = text.clone();
        *count = 0;
        let Some(c) = it.next_code_point() else { return 0 };
        let mut num = 1;
        match self {
            Dictionary::Bytes { data, .. } => {
                let mut t = BytesTrie { b: data, pos: 0, remaining: -1 };
                let mut r = t.first(self.transform(c));
                loop {
                    if r.has_value() {
                        if *count < limit {
                            if let Some(v) = values.as_deref_mut() {
                                v[*count] = t.value();
                            }
                            lengths[*count] = num;
                            *count += 1;
                        }
                        if r == TrieResult::Final {
                            break;
                        }
                    } else if r == TrieResult::NoMatch {
                        break;
                    }
                    if num >= max_length {
                        break;
                    }
                    let Some(c) = it.next_code_point() else { break };
                    num += 1;
                    r = t.next(self.transform(c));
                }
            }
            Dictionary::Chars { data } => {
                let mut t = CharsTrie { c: data, pos: 0, remaining: -1 };
                let mut r = t.first_cp(c);
                loop {
                    if r.has_value() {
                        if *count < limit {
                            if let Some(v) = values.as_deref_mut() {
                                v[*count] = t.value();
                            }
                            lengths[*count] = num;
                            *count += 1;
                        }
                        if r == TrieResult::Final {
                            break;
                        }
                    } else if r == TrieResult::NoMatch {
                        break;
                    }
                    if num >= max_length {
                        break;
                    }
                    let Some(c) = it.next_code_point() else { break };
                    num += 1;
                    r = t.next_cp(c);
                }
            }
        }
        num
    }
}

/// `DictionaryBreakEngine.DequeI`, kept exactly (an empty deque's `peek` reads a stale slot,
/// which the engines rely on).
#[derive(Clone)]
pub(crate) struct DequeI {
    data: Vec<i32>,
    last: usize,
    first: usize,
}

impl Default for DequeI {
    fn default() -> Self {
        DequeI { data: vec![0; 50], last: 4, first: 4 }
    }
}

impl DequeI {
    pub fn size(&self) -> usize {
        self.first - self.last
    }
    pub fn is_empty(&self) -> bool {
        self.size() == 0
    }
    pub fn offer(&mut self, v: i32) {
        self.last -= 1;
        self.data[self.last] = v;
    }
    pub fn push(&mut self, v: i32) {
        if self.first >= self.data.len() {
            self.data.resize(self.data.len() * 2, 0);
        }
        self.data[self.first] = v;
        self.first += 1;
    }
    pub fn pop(&mut self) -> i32 {
        self.first -= 1;
        self.data[self.first]
    }
    pub fn peek(&self) -> i32 {
        self.data[self.first - 1]
    }
    pub fn element_at(&self, i: usize) -> i32 {
        self.data[self.last + i]
    }
    pub fn clear(&mut self) {
        self.last = 4;
        self.first = 4;
    }
}

/// `DictionaryBreakEngine.PossibleWord`.
struct PossibleWord {
    lengths: [usize; 20],
    count: usize,
    prefix: usize,
    offset: i64,
    mark: usize,
    current: usize,
}

impl PossibleWord {
    fn new() -> PossibleWord {
        PossibleWord { lengths: [0; 20], count: 0, prefix: 0, offset: -1, mark: 0, current: 0 }
    }
    fn candidates(&mut self, it: &mut Iter, dict: &Dictionary, range_end: usize) -> usize {
        let start = it.i;
        if start as i64 != self.offset {
            self.offset = start as i64;
            self.prefix = dict.matches(it, range_end - start, &mut self.lengths, &mut self.count, 20, None);
            if self.count == 0 {
                it.i = start;
            }
        }
        if self.count > 0 {
            it.i = start + self.lengths[self.count - 1];
        }
        self.current = self.count.wrapping_sub(1);
        self.mark = self.current;
        self.count
    }
    fn accept_marked(&self, it: &mut Iter) -> usize {
        it.i = self.offset as usize + self.lengths[self.mark];
        self.lengths[self.mark]
    }
    fn back_up(&mut self, it: &mut Iter) -> bool {
        if self.current > 0 && self.current != usize::MAX {
            self.current -= 1;
            it.i = self.offset as usize + self.lengths[self.current];
            return true;
        }
        false
    }
    fn mark_current(&mut self) {
        self.mark = self.current;
    }
}

/// Which dictionary engine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EngineKind {
    Thai,
    Lao,
    Burmese,
    Khmer,
    Cj,
    Korean,
}

/// One engine's data.
pub(crate) struct Engine {
    pub kind: EngineKind,
    /// `fSet`: the characters `findBreaks` consumes.
    set: USet,
    mark: Option<USet>,
    begin: Option<USet>,
    end: Option<USet>,
    dict: Dictionary,
}

static THAI_DICT: &[u8] = include_bytes!("data/thaidict.dict");
static LAO_DICT: &[u8] = include_bytes!("data/laodict.dict");
static BURMESE_DICT: &[u8] = include_bytes!("data/burmesedict.dict");
static KHMER_DICT: &[u8] = include_bytes!("data/khmerdict.dict");
static CJ_DICT: &[u8] = include_bytes!("data/cjdict.dict");

fn set(p: &str) -> USet {
    USet::parse(p).expect("engine set")
}

impl Engine {
    pub(crate) fn get(kind: EngineKind) -> &'static Engine {
        static E: [OnceLock<Engine>; 6] = [OnceLock::new(), OnceLock::new(), OnceLock::new(), OnceLock::new(), OnceLock::new(), OnceLock::new()];
        E[kind as usize].get_or_init(|| match kind {
            EngineKind::Thai => Engine {
                kind,
                set: set("[[:Thai:]&[:LineBreak=SA:]]"),
                mark: Some(set("[[[:Thai:]&[:LineBreak=SA:]&[:M:]]\\u0020]")),
                begin: Some(set("[\\u0E01-\\u0E2E\\u0E40-\\u0E44]")),
                end: Some(set("[[[:Thai:]&[:LineBreak=SA:]]-[\\u0E31\\u0E40-\\u0E44]]")),
                dict: Dictionary::load(THAI_DICT),
            },
            EngineKind::Lao => Engine {
                kind,
                set: set("[[:Laoo:]&[:LineBreak=SA:]]"),
                mark: Some(set("[[[:Laoo:]&[:LineBreak=SA:]&[:M:]]\\u0020]")),
                begin: Some(set("[\\u0E81-\\u0EAE\\u0EC0-\\u0EC4\\u0EDC-\\u0EDD]")),
                end: Some(set("[[[:Laoo:]&[:LineBreak=SA:]]-[\\u0EC0-\\u0EC4]]")),
                dict: Dictionary::load(LAO_DICT),
            },
            EngineKind::Burmese => Engine {
                kind,
                set: set("[[:Mymr:]&[:LineBreak=SA:]]"),
                mark: Some(set("[[[:Mymr:]&[:LineBreak=SA:]&[:M:]]\\u0020]")),
                begin: Some(set("[\\u1000-\\u102A]")),
                end: Some(set("[[:Mymr:]&[:LineBreak=SA:]]")),
                dict: Dictionary::load(BURMESE_DICT),
            },
            EngineKind::Khmer => Engine {
                kind,
                set: set("[[:Khmr:]&[:LineBreak=SA:]]"),
                mark: Some(set("[[[:Khmr:]&[:LineBreak=SA:]&[:M:]]\\u0020]")),
                begin: Some(set("[\\u1780-\\u17B3]")),
                end: Some(set("[[[:Khmr:]&[:LineBreak=SA:]]-[\\u17D2]]")),
                dict: Dictionary::load(KHMER_DICT),
            },
            EngineKind::Cj => Engine {
                kind,
                set: set("[[:Han:][:Hiragana:][:Katakana:]\\u30fc\\uff70\\uff9e\\uff9f]"),
                mark: None,
                begin: None,
                end: None,
                dict: Dictionary::load(CJ_DICT),
            },
            EngineKind::Korean => Engine {
                kind,
                set: set("[\\uac00-\\ud7a3]"),
                mark: None,
                begin: None,
                end: None,
                dict: Dictionary::load(CJ_DICT),
            },
        })
    }

    /// `LanguageBreakEngine.handles`: Thai, Lao, Burmese and Khmer go by script, the CJK
    /// engines by their sets.
    pub(crate) fn for_char(c: u32) -> Option<&'static Engine> {
        use icu_properties::props::Script;
        let sc = icu_properties::CodePointMapData::<Script>::new().get32(c);
        let kind = match sc {
            Script::Thai => EngineKind::Thai,
            Script::Lao => EngineKind::Lao,
            Script::Myanmar => EngineKind::Burmese,
            Script::Khmer => EngineKind::Khmer,
            _ => {
                if Engine::get(EngineKind::Cj).set.contains(c) {
                    EngineKind::Cj
                } else if (0xac00..=0xd7a3).contains(&c) {
                    EngineKind::Korean
                } else {
                    return None;
                }
            }
        };
        Some(Engine::get(kind))
    }

    /// `DictionaryBreakEngine.findBreaks`.
    pub(crate) fn find_breaks(&self, it: &mut Iter, end_pos: usize, found: &mut DequeI) -> usize {
        let start = it.i;
        let mut c = it.current32();
        let mut current;
        loop {
            current = it.i;
            if current >= end_pos || !self.set.contains(c) {
                break;
            }
            it.next32();
            c = it.current32();
        }
        let r = self.divide(it, start, current, found);
        it.i = current;
        r
    }

    fn divide(&self, it: &mut Iter, start: usize, end: usize, found: &mut DequeI) -> usize {
        match self.kind {
            EngineKind::Cj | EngineKind::Korean => self.divide_cjk(it, start, end, found),
            EngineKind::Thai => self.divide_sea(it, start, end, found, 4, true, true),
            EngineKind::Khmer => self.divide_sea(it, start, end, found, 4, false, false),
            _ => self.divide_sea(it, start, end, found, 2, false, false),
        }
    }

    /// Thai's, Lao's, Burmese's and Khmer's `divideUpDictionaryRange` (they differ in the
    /// minimum span, Thai's suffix handling and how Thai leaves its best-guess loop).
    #[allow(clippy::too_many_arguments)]
    fn divide_sea(&self, it: &mut Iter, range_start: usize, range_end: usize, found: &mut DequeI, min_span: usize, thai_suffixes: bool, thai_loop: bool) -> usize {
        const LOOKAHEAD: usize = 3;
        const ROOT_COMBINE: usize = 3;
        const PREFIX_COMBINE: usize = 3;
        const PAIYANNOI: u32 = 0x0e2f;
        const MAIYAMOK: u32 = 0x0e46;
        if range_end - range_start < min_span {
            return 0;
        }
        let (mark, begin, end) = (self.mark.as_ref().unwrap(), self.begin.as_ref().unwrap(), self.end.as_ref().unwrap());
        let dict = &self.dict;
        let mut words_found: i64 = 0;
        let mut words = [PossibleWord::new(), PossibleWord::new(), PossibleWord::new()];
        let w = |n: i64| (n.rem_euclid(LOOKAHEAD as i64)) as usize;
        it.i = range_start;
        while it.i < range_end {
            let current = it.i;
            let mut word_length = 0usize;
            let candidates = words[w(words_found)].candidates(it, dict, range_end);
            if candidates == 1 {
                word_length = words[w(words_found)].accept_marked(it);
                words_found += 1;
            } else if candidates > 1 {
                if it.i < range_end {
                    let mut found_best = false;
                    'outer: loop {
                        if words[w(words_found + 1)].candidates(it, dict, range_end) > 0 {
                            words[w(words_found)].mark_current();
                            if it.i >= range_end {
                                break 'outer;
                            }
                            loop {
                                if words[w(words_found + 2)].candidates(it, dict, range_end) > 0 {
                                    words[w(words_found)].mark_current();
                                    found_best = true;
                                    break;
                                }
                                if !words[w(words_found + 1)].back_up(it) {
                                    break;
                                }
                            }
                            if found_best && thai_loop {
                                break 'outer;
                            }
                        }
                        // Thai leaves with a labeled break; the others test `backUp` first.
                        if !words[w(words_found)].back_up(it) || found_best {
                            break;
                        }
                    }
                }
                word_length = words[w(words_found)].accept_marked(it);
                words_found += 1;
            }
            if it.i < range_end && word_length < ROOT_COMBINE {
                if words[w(words_found)].candidates(it, dict, range_end) == 0
                    && (word_length == 0 || words[w(words_found)].prefix < PREFIX_COMBINE)
                {
                    let mut remaining = range_end as i64 - (current + word_length) as i64;
                    let mut pc = it.current();
                    let mut chars = 0;
                    loop {
                        it.next();
                        let uc = it.current();
                        chars += 1;
                        remaining -= 1;
                        if remaining <= 0 {
                            break;
                        }
                        if end.contains(pc) && begin.contains(uc) {
                            let candidate = words[w(words_found + 1)].candidates(it, dict, range_end);
                            it.i = current + word_length + chars;
                            if candidate > 0 {
                                break;
                            }
                        }
                        pc = uc;
                    }
                    if word_length == 0 {
                        words_found += 1;
                    }
                    word_length += chars;
                } else {
                    it.i = current + word_length;
                }
            }
            while it.i < range_end && mark.contains(it.current()) {
                let p = it.i;
                it.next();
                word_length += it.i - p;
            }
            if thai_suffixes && it.i < range_end && word_length > 0 {
                let mut uc;
                if words[w(words_found)].candidates(it, dict, range_end) == 0 && {
                    uc = it.current();
                    uc == PAIYANNOI || uc == MAIYAMOK
                } {
                    if uc == PAIYANNOI {
                        let prev = it.previous();
                        if prev != PAIYANNOI && prev != MAIYAMOK {
                            it.next();
                            it.next();
                            word_length += 1;
                            uc = it.current();
                        } else {
                            it.next();
                        }
                    }
                    if uc == MAIYAMOK {
                        if it.previous() != MAIYAMOK {
                            it.next();
                            it.next();
                            word_length += 1;
                        } else {
                            it.next();
                        }
                    }
                } else {
                    it.i = current + word_length;
                }
            }
            if word_length > 0 {
                found.push((current + word_length) as i32);
            }
        }
        if found.peek() >= range_end as i32 {
            found.pop();
            words_found -= 1;
        }
        words_found.max(0) as usize
    }

    /// `CjkBreakEngine.divideUpDictionaryRange` (word breaking, not phrases).
    fn divide_cjk(&self, it: &mut Iter, start_pos: usize, end_pos: usize, found: &mut DequeI) -> usize {
        const MAX_SNLP: i64 = 255;
        const MAX_WORD: usize = 20;
        const MAX_KATAKANA_GROUP: usize = 20;
        if start_pos >= end_pos {
            return 0;
        }
        let pre: Vec<u16> = it.s[start_pos..end_pos].to_vec();
        let nfkc = Normalizer { data: Data::Nfkc, compose: true, filter: None };
        let mut normalized = Vec::new();
        nfkc.normalize_utf16(&pre, &mut normalized);
        let mut char_positions: Vec<usize> = vec![0];
        let text: Vec<u16> = if normalized == pre {
            let mut i = 0;
            while i < pre.len() {
                let c = Iter { s: &pre, i }.current32();
                i += if c > 0xffff { 2 } else { 1 };
                char_positions.push(i);
            }
            pre
        } else {
            // The legacy `Normalizer` iterator: each normalized code point maps to its
            // segment's start, the segment's last one to its end.
            let mut i = 0;
            while i < pre.len() {
                let seg_start = i;
                let c = Iter { s: &pre, i }.current32();
                i += if c > 0xffff { 2 } else { 1 };
                while i < pre.len() {
                    let c = Iter { s: &pre, i }.current32();
                    if nfkc.has_boundary_before(c) {
                        break;
                    }
                    i += if c > 0xffff { 2 } else { 1 };
                }
                let mut seg = Vec::new();
                nfkc.normalize_utf16(&pre[seg_start..i], &mut seg);
                let mut k = 0;
                while k < seg.len() {
                    let c = Iter { s: &seg, i: k }.current32();
                    k += if c > 0xffff { 2 } else { 1 };
                    char_positions.push(if k < seg.len() { seg_start } else { i });
                }
            }
            normalized
        };
        let num = char_positions.len() - 1;
        let mut best: Vec<i64> = vec![i64::from(i32::MAX); num + 1];
        best[0] = 0;
        let mut prev: Vec<i64> = vec![-1; num + 1];
        let mut values = [0i32; MAX_WORD + 1];
        let mut lengths = [0usize; MAX_WORD + 1];
        let mut t = Iter { s: &text, i: 0 };
        let mut ix = 0;
        let mut prev_katakana = false;
        let is_katakana = |v: u32| (0x30a1..=0x30fe).contains(&v) && v != 0x30fb || (0xff66..=0xff9f).contains(&v);
        let katakana_cost = |n: usize| -> i64 { [8192, 984, 408, 240, 204, 252, 300, 372, 480].get(n).copied().unwrap_or(8192) };
        for i in 0..num {
            if i > 0 {
                t.i = ix;
                t.next32();
            }
            ix = t.i;
            if best[i] == i64::from(i32::MAX) {
                continue;
            }
            let max_search = if i + MAX_WORD < num { MAX_WORD } else { num - i };
            let mut count = 0;
            self.dict.matches(&t, max_search, &mut lengths, &mut count, max_search, Some(&mut values));
            t.i = ix;
            let cur = t.current32();
            if (count == 0 || lengths[0] != 1) && cur != DONE32 && !(0xac00..=0xd7a3).contains(&cur) {
                values[count] = MAX_SNLP as i32;
                lengths[count] = 1;
                count += 1;
            }
            for j in 0..count {
                let new = best[i] + values[j] as i64;
                if new < best[lengths[j] + i] {
                    best[lengths[j] + i] = new;
                    prev[lengths[j] + i] = i as i64;
                }
            }
            let katakana = is_katakana(t.current32());
            if !prev_katakana && katakana {
                let mut j = i + 1;
                t.next32();
                while j < num && j - i < MAX_KATAKANA_GROUP && is_katakana(t.current32()) {
                    t.next32();
                    j += 1;
                }
                if j - i < MAX_KATAKANA_GROUP {
                    let new = best[i] + katakana_cost(j - i);
                    if new < best[j] {
                        best[j] = new;
                        prev[j] = i as i64;
                    }
                }
            }
            prev_katakana = katakana;
        }
        let mut boundaries: Vec<usize> = Vec::new();
        if best[num] == i64::from(i32::MAX) {
            boundaries.push(num);
        } else {
            let mut i = num as i64;
            while i > 0 {
                boundaries.push(i as usize);
                i = prev[i as usize];
            }
        }
        if found.is_empty() || found.peek() < start_pos as i32 {
            boundaries.push(0);
        }
        let mut corrected: i64 = 0;
        let mut previous: i64 = -1;
        for &b in boundaries.iter().rev() {
            let pos = (char_positions[b] + start_pos) as i64;
            it.i = pos as usize;
            if pos > previous && pos != start_pos as i64 {
                found.push(pos as i32);
                corrected += 1;
            }
            previous = pos;
        }
        if !found.is_empty() && found.peek() == end_pos as i32 {
            found.pop();
            corrected -= 1;
        }
        if !found.is_empty() {
            it.i = found.peek() as usize;
        }
        corrected.max(0) as usize
    }
}
