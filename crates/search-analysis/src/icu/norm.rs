//! ICU normalization (`Normalizer2`) over ICU's own data files, as Lucene's ICU components run
//! it: a port of ICU4J 77.1's `Normalizer2Impl` (decomposition, composition and the boundary
//! tests; Unicode-3.0) reading the `.nrm` files ICU4J ships (`nfc`, `nfkc`, `nfkc_cf`,
//! `nfkc_scf`, `uts46`) and Lucene's `utr30.nrm` (`icu_folding`), plus `FilteredNormalizer2`.
//!
//! Text is processed as UTF-16, as in Java. The data is read where it lies in the binary.

use std::sync::OnceLock;

use super::trie::{CpTrie, be16, header};
use super::uset::USet;

const JAMO_L_BASE: u32 = 0x1100;
const JAMO_V_BASE: u32 = 0x1161;
const JAMO_T_BASE: u32 = 0x11a7;
const HANGUL_BASE: u32 = 0xac00;
const JAMO_L_COUNT: u32 = 19;
const JAMO_V_COUNT: u32 = 21;
const JAMO_T_COUNT: u32 = 28;
const JAMO_VT_COUNT: u32 = JAMO_V_COUNT * JAMO_T_COUNT;
const HANGUL_COUNT: u32 = JAMO_L_COUNT * JAMO_VT_COUNT;

const MIN_YES_YES_WITH_CC: u32 = 0xfe02;
const JAMO_VT: u32 = 0xfe00;
const MIN_NORMAL_MAYBE_YES: u32 = 0xfc00;
const JAMO_L: u32 = 2;
const INERT: u32 = 1;
const HAS_COMP_BOUNDARY_AFTER: u32 = 1;
const OFFSET_SHIFT: u32 = 1;
const DELTA_TCCC_1: u32 = 2;
const DELTA_TCCC_MASK: u32 = 6;
const DELTA_SHIFT: u32 = 3;
const MAX_DELTA: i64 = 0x40;
const MAPPING_HAS_CCC_LCCC_WORD: u32 = 0x80;
const MAPPING_LENGTH_MASK: u32 = 0x1f;
const COMP_1_LAST_TUPLE: u32 = 0x8000;
const COMP_1_TRIPLE: u32 = 1;
const COMP_1_TRAIL_LIMIT: u32 = 0x3400;
const COMP_1_TRAIL_MASK: u32 = 0x7ffe;
const COMP_1_TRAIL_SHIFT: u32 = 9;
const COMP_2_TRAIL_SHIFT: u32 = 6;
const COMP_2_TRAIL_MASK: u32 = 0xffc0;

fn is_lead(c: u32) -> bool {
    (c & 0xfffffc00) == 0xd800
}

fn is_trail(c: u32) -> bool {
    (c & 0xfffffc00) == 0xdc00
}

/// `Character.codePointAt` over UTF-16.
fn cp_at(s: &[u16], i: usize) -> u32 {
    let c = s[i] as u32;
    if is_lead(c) && i + 1 < s.len() && is_trail(s[i + 1] as u32) {
        0x10000 + ((c - 0xd800) << 10) + (s[i + 1] as u32 - 0xdc00)
    } else {
        c
    }
}

/// `Character.codePointBefore`.
fn cp_before(s: &[u16], i: usize) -> u32 {
    let c = s[i - 1] as u32;
    if is_trail(c) && i >= 2 && is_lead(s[i - 2] as u32) {
        0x10000 + ((s[i - 2] as u32 - 0xd800) << 10) + (c - 0xdc00)
    } else {
        c
    }
}

fn char_count(c: u32) -> usize {
    if c > 0xffff { 2 } else { 1 }
}

fn push_cp(s: &mut Vec<u16>, c: u32) {
    if c > 0xffff {
        s.push((0xd800 + ((c - 0x10000) >> 10)) as u16);
        s.push((0xdc00 + ((c - 0x10000) & 0x3ff)) as u16);
    } else {
        s.push(c as u16);
    }
}

/// `Normalizer2Impl`: one `.nrm` data set.
pub(crate) struct Impl {
    trie: CpTrie<'static>,
    extra: &'static [u8],
    small_fcd: &'static [u8],
    min_decomp_no_cp: u32,
    min_comp_no_maybe_cp: u32,
    min_lccc_cp: u32,
    min_yes_no: u32,
    min_yes_no_mappings_only: u32,
    min_no_no: u32,
    min_no_no_comp_boundary_before: u32,
    min_no_no_comp_no_maybe_cc: u32,
    min_no_no_empty: u32,
    limit_no_no: u32,
    min_maybe_no: u32,
    min_maybe_yes: u32,
    center_no_no_delta: i64,
}

/// The UTF-16 buffer a normalization appends to, reordering combining marks
/// (`ReorderingBuffer`).
struct Buf<'a> {
    imp: &'a Impl,
    s: Vec<u16>,
    reorder_start: usize,
    last_cc: u32,
    cp_start: usize,
    cp_limit: usize,
}

impl<'a> Buf<'a> {
    fn new(imp: &'a Impl, s: Vec<u16>) -> Buf<'a> {
        let mut b = Buf { imp, s, reorder_start: 0, last_cc: 0, cp_start: 0, cp_limit: 0 };
        if !b.s.is_empty() {
            b.cp_start = b.s.len();
            b.last_cc = b.previous_cc();
            if b.last_cc > 1 {
                while b.previous_cc() > 1 {}
            }
            b.reorder_start = b.cp_limit;
        }
        b
    }

    fn len(&self) -> usize {
        self.s.len()
    }

    fn append_cc(&mut self, c: u32, cc: u32) {
        if self.last_cc <= cc || cc == 0 {
            push_cp(&mut self.s, c);
            self.last_cc = cc;
            if cc <= 1 {
                self.reorder_start = self.s.len();
            }
        } else {
            self.insert(c, cc);
        }
    }

    fn append_mapping(&mut self, m: &[u16], is_nfd: bool, lead_cc: u32, trail_cc: u32) {
        if m.is_empty() {
            return;
        }
        if self.last_cc <= lead_cc || lead_cc == 0 {
            if trail_cc <= 1 {
                self.reorder_start = self.s.len() + m.len();
            } else if lead_cc <= 1 {
                self.reorder_start = self.s.len() + 1;
            }
            self.s.extend_from_slice(m);
            self.last_cc = trail_cc;
        } else {
            let mut i = 0;
            let c = cp_at(m, i);
            i += char_count(c);
            self.insert(c, lead_cc);
            while i < m.len() {
                let c = cp_at(m, i);
                i += char_count(c);
                let cc = if i < m.len() {
                    let n16 = self.imp.norm16(c);
                    if is_nfd { cc_from_yes_or_maybe_yes(n16) } else { self.imp.cc(n16) }
                } else {
                    trail_cc
                };
                self.append_cc(c, cc);
            }
        }
    }

    /// `append(CharSequence, start, limit)`: zero-cc text.
    fn append_zero_cc(&mut self, s: &[u16]) {
        if !s.is_empty() {
            self.s.extend_from_slice(s);
            self.last_cc = 0;
            self.reorder_start = self.s.len();
        }
    }

    fn flush(&mut self) {
        self.reorder_start = self.s.len();
        self.last_cc = 0;
    }

    fn insert(&mut self, c: u32, cc: u32) {
        self.cp_start = self.s.len();
        self.skip_previous();
        while self.previous_cc() > cc {}
        let at = self.cp_limit;
        if c <= 0xffff {
            self.s.insert(at, c as u16);
            if cc <= 1 {
                self.reorder_start = at + 1;
            }
        } else {
            let mut pair = Vec::with_capacity(2);
            push_cp(&mut pair, c);
            self.s.splice(at..at, pair);
            if cc <= 1 {
                self.reorder_start = at + 2;
            }
        }
    }

    fn skip_previous(&mut self) {
        self.cp_limit = self.cp_start;
        let c = cp_before(&self.s, self.cp_start);
        self.cp_start -= char_count(c);
    }

    fn previous_cc(&mut self) -> u32 {
        self.cp_limit = self.cp_start;
        if self.reorder_start >= self.cp_start {
            return 0;
        }
        let c = cp_before(&self.s, self.cp_start);
        self.cp_start -= char_count(c);
        self.imp.cc_from_yes_or_maybe_yes_cp(c)
    }
}

fn cc_from_normal_yes_or_maybe(n16: u32) -> u32 {
    (n16 >> OFFSET_SHIFT) & 0xff
}

fn cc_from_yes_or_maybe_yes(n16: u32) -> u32 {
    if n16 >= MIN_NORMAL_MAYBE_YES { cc_from_normal_yes_or_maybe(n16) } else { 0 }
}

impl Impl {
    fn load(bytes: &'static [u8]) -> Impl {
        let (format, version, b) = header(bytes).expect("ICU normalization data");
        assert!(&format == b"Nrm2" && version == 5, "Nrm2 format version 5");
        let ix = |i: usize| u32::from_be_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        let trie_offset = ix(0) as usize;
        let extra_offset = ix(1) as usize;
        let small_fcd_offset = ix(2) as usize;
        let (trie, _) = CpTrie::parse(&b[trie_offset..extra_offset]).expect("normalization trie");
        let min_maybe_no = ix(20);
        Impl {
            trie,
            extra: &b[extra_offset..small_fcd_offset],
            small_fcd: &b[small_fcd_offset..small_fcd_offset + 0x100],
            min_decomp_no_cp: ix(8),
            min_comp_no_maybe_cp: ix(9),
            min_lccc_cp: ix(18),
            min_yes_no: ix(10),
            min_yes_no_mappings_only: ix(14),
            min_no_no: ix(11),
            min_no_no_comp_boundary_before: ix(15),
            min_no_no_comp_no_maybe_cc: ix(16),
            min_no_no_empty: ix(17),
            limit_no_no: ix(12),
            min_maybe_no,
            min_maybe_yes: ix(13),
            center_no_no_delta: ((min_maybe_no >> DELTA_SHIFT) as i64) - MAX_DELTA - 1,
        }
    }

    /// A unit of the extra data (`extraData.charAt`).
    fn x(&self, i: usize) -> u32 {
        be16(self.extra, i) as u32
    }

    fn x_slice(&self, start: usize, len: usize) -> Vec<u16> {
        (start..start + len).map(|i| self.x(i) as u16).collect()
    }

    fn norm16(&self, c: u32) -> u32 {
        if is_lead(c) { INERT } else { self.trie.get(c) }
    }

    fn raw_norm16(&self, c: u32) -> u32 {
        self.trie.get(c)
    }

    fn cc(&self, n16: u32) -> u32 {
        if n16 >= MIN_NORMAL_MAYBE_YES {
            return cc_from_normal_yes_or_maybe(n16);
        }
        if n16 < self.min_no_no || self.limit_no_no <= n16 {
            return 0;
        }
        let mapping = self.data_for_yes_or_no(n16);
        if self.x(mapping) & MAPPING_HAS_CCC_LCCC_WORD != 0 { self.x(mapping - 1) & 0xff } else { 0 }
    }

    fn cc_from_yes_or_maybe_yes_cp(&self, c: u32) -> u32 {
        if c < self.min_comp_no_maybe_cp {
            return 0;
        }
        cc_from_yes_or_maybe_yes(self.norm16(c))
    }

    fn single_lead_might_have_non_zero_fcd16(&self, lead: u32) -> bool {
        let bits = self.small_fcd[(lead >> 8) as usize];
        bits != 0 && (bits >> ((lead >> 5) & 7)) & 1 != 0
    }

    fn is_algorithmic_no_no(&self, n16: u32) -> bool {
        self.limit_no_no <= n16 && n16 < self.min_maybe_no
    }
    fn is_decomp_yes(&self, n16: u32) -> bool {
        n16 < self.min_yes_no || self.min_maybe_yes <= n16
    }
    fn is_maybe(&self, n16: u32) -> bool {
        self.min_maybe_no <= n16 && n16 <= JAMO_VT
    }
    fn is_maybe_yes_or_non_zero_cc(&self, n16: u32) -> bool {
        n16 >= self.min_maybe_yes
    }
    fn hangul_lvt(&self) -> u32 {
        self.min_yes_no_mappings_only | HAS_COMP_BOUNDARY_AFTER
    }
    fn is_hangul_lv(&self, n16: u32) -> bool {
        n16 == self.min_yes_no
    }
    fn is_hangul_lvt(&self, n16: u32) -> bool {
        n16 == self.hangul_lvt()
    }
    fn is_comp_yes_and_zero_cc(&self, n16: u32) -> bool {
        n16 < self.min_no_no
    }
    fn is_most_decomp_yes_and_zero_cc(&self, n16: u32) -> bool {
        n16 < self.min_yes_no || n16 == MIN_NORMAL_MAYBE_YES || n16 == JAMO_VT
    }
    fn is_decomp_no_algorithmic(&self, n16: u32) -> bool {
        self.limit_no_no <= n16 && n16 < self.min_maybe_no
    }
    fn map_algorithmic(&self, c: u32, n16: u32) -> u32 {
        (c as i64 + (n16 >> DELTA_SHIFT) as i64 - self.center_no_no_delta) as u32
    }
    fn data_for_yes_or_no(&self, n16: u32) -> usize {
        (n16 >> OFFSET_SHIFT) as usize
    }
    fn data_for_maybe(&self, n16: u32) -> usize {
        ((n16 - self.min_maybe_no + self.limit_no_no) >> OFFSET_SHIFT) as usize
    }
    fn data(&self, mut n16: u32) -> usize {
        if n16 >= self.min_maybe_no {
            n16 = n16 - self.min_maybe_no + self.limit_no_no;
        }
        (n16 >> OFFSET_SHIFT) as usize
    }
    fn compositions_list_for_decomp_yes(&self, n16: u32) -> Option<usize> {
        if !(JAMO_L..MIN_NORMAL_MAYBE_YES).contains(&n16) { None } else { Some(self.data(n16)) }
    }
    fn compositions_list_for_composite(&self, n16: u32) -> usize {
        let list = self.data(n16);
        list + 1 + (self.x(list) & MAPPING_LENGTH_MASK) as usize
    }

    fn norm16_has_comp_boundary_before(&self, n16: u32) -> bool {
        n16 < self.min_no_no_comp_no_maybe_cc || self.is_algorithmic_no_no(n16)
    }
    fn norm16_has_comp_boundary_after(&self, n16: u32) -> bool {
        n16 & HAS_COMP_BOUNDARY_AFTER != 0
    }
    fn has_comp_boundary_before_cp(&self, c: u32) -> bool {
        c < self.min_comp_no_maybe_cp || self.norm16_has_comp_boundary_before(self.norm16(c))
    }
    fn has_comp_boundary_before_at(&self, s: &[u16], src: usize) -> bool {
        src == s.len() || self.has_comp_boundary_before_cp(cp_at(s, src))
    }
    fn has_comp_boundary_after_before(&self, s: &[u16], start: usize, p: usize) -> bool {
        start == p || self.norm16_has_comp_boundary_after(self.norm16(cp_before(s, p)))
    }

    fn norm16_has_decomp_boundary_before(&self, n16: u32) -> bool {
        if n16 < self.min_no_no_comp_no_maybe_cc {
            return true;
        }
        if n16 >= self.limit_no_no {
            return n16 <= MIN_NORMAL_MAYBE_YES || n16 == JAMO_VT;
        }
        let mapping = self.data_for_yes_or_no(n16);
        self.x(mapping) & MAPPING_HAS_CCC_LCCC_WORD == 0 || self.x(mapping - 1) & 0xff00 == 0
    }

    fn norm16_has_decomp_boundary_after(&self, n16: u32) -> bool {
        if n16 <= self.min_yes_no || self.is_hangul_lvt(n16) {
            return true;
        }
        if n16 >= self.limit_no_no {
            if self.is_maybe_yes_or_non_zero_cc(n16) {
                return n16 <= MIN_NORMAL_MAYBE_YES || n16 == JAMO_VT;
            } else if n16 < self.min_maybe_no {
                return n16 & DELTA_TCCC_MASK <= DELTA_TCCC_1;
            }
        }
        let mapping = self.data(n16);
        let first = self.x(mapping);
        if first > 0x1ff {
            return false;
        }
        if first <= 0xff {
            return true;
        }
        first & MAPPING_HAS_CCC_LCCC_WORD == 0 || self.x(mapping - 1) & 0xff00 == 0
    }

    pub(crate) fn has_boundary_before(&self, c: u32, compose: bool) -> bool {
        if compose {
            self.has_comp_boundary_before_cp(c)
        } else {
            c < self.min_lccc_cp
                || (c <= 0xffff && !self.single_lead_might_have_non_zero_fcd16(c))
                || self.norm16_has_decomp_boundary_before(self.norm16(c))
        }
    }

    pub(crate) fn has_boundary_after(&self, c: u32, compose: bool) -> bool {
        if compose {
            self.norm16_has_comp_boundary_after(self.norm16(c))
        } else if c < self.min_decomp_no_cp || (c <= 0xffff && !self.single_lead_might_have_non_zero_fcd16(c)) {
            true
        } else {
            self.norm16_has_decomp_boundary_after(self.norm16(c))
        }
    }

    fn fcd16_from_maybe_or_non_zero_cc(&self, n16: u32) -> u32 {
        if n16 >= MIN_NORMAL_MAYBE_YES {
            let cc = cc_from_normal_yes_or_maybe(n16);
            return cc | (cc << 8);
        } else if n16 >= self.min_maybe_yes {
            return 0;
        }
        self.x(self.data_for_maybe(n16)) >> 8
    }

    /// `composeQuickCheck(s, 0, len, onlyContiguous=false, doSpan=true) >>> 1`.
    fn compose_span_yes(&self, s: &[u16]) -> usize {
        let limit = s.len();
        let mut src = 0;
        let mut prev_boundary = 0;
        loop {
            let mut prev_src;
            let mut c;
            let mut n16: u32;
            loop {
                if src == limit {
                    return src;
                }
                c = s[src] as u32;
                if c < self.min_comp_no_maybe_cp || {
                    n16 = self.trie.get(c);
                    self.is_comp_yes_and_zero_cc(n16)
                } {
                    src += 1;
                } else {
                    prev_src = src;
                    src += 1;
                    if !is_lead(c) {
                        break;
                    }
                    if src != limit && is_trail(s[src] as u32) {
                        src += 1;
                        c = cp_at(s, prev_src);
                        n16 = self.trie.get(c);
                        if !self.is_comp_yes_and_zero_cc(n16) {
                            break;
                        }
                    }
                }
            }
            if prev_boundary != prev_src {
                prev_boundary = prev_src;
                if !self.norm16_has_comp_boundary_before(n16) {
                    let cp = cp_before(s, prev_src);
                    if !self.norm16_has_comp_boundary_after(self.norm16(cp)) {
                        prev_boundary -= char_count(cp);
                    }
                }
            }
            if n16 >= self.min_maybe_no {
                let mut fcd16 = self.fcd16_from_maybe_or_non_zero_cc(n16);
                loop {
                    if n16 < MIN_YES_YES_WITH_CC {
                        return prev_boundary;
                    }
                    if src == limit {
                        return src;
                    }
                    let prev_cc = fcd16 & 0xff;
                    c = cp_at(s, src);
                    n16 = self.norm16(c);
                    if n16 >= self.min_maybe_no {
                        fcd16 = self.fcd16_from_maybe_or_non_zero_cc(n16);
                        let cc = (fcd16 >> 8) & 0xff;
                        if !(prev_cc <= cc || cc == 0) {
                            break;
                        }
                    } else {
                        break;
                    }
                    src += char_count(c);
                }
                if self.is_comp_yes_and_zero_cc(n16) {
                    prev_boundary = src;
                    src += char_count(c);
                    continue;
                }
            }
            return prev_boundary;
        }
    }

    /// `decompose(s, 0, len, null)`: the quick check's span.
    fn decompose_span_yes(&self, s: &[u16]) -> usize {
        let limit = s.len();
        let mut src = 0;
        let mut prev_boundary = 0;
        let mut prev_cc = 0;
        loop {
            let prev_src = src;
            let mut c = 0;
            let mut n16 = 0;
            while src != limit {
                c = s[src] as u32;
                if c < self.min_decomp_no_cp || {
                    n16 = self.trie.get(c);
                    self.is_most_decomp_yes_and_zero_cc(n16)
                } {
                    src += 1;
                } else if !is_lead(c) {
                    break;
                } else if src + 1 != limit && is_trail(s[src + 1] as u32) {
                    c = cp_at(s, src);
                    n16 = self.trie.get(c);
                    if self.is_most_decomp_yes_and_zero_cc(n16) {
                        src += 2;
                    } else {
                        break;
                    }
                } else {
                    src += 1;
                }
            }
            if src != prev_src {
                prev_cc = 0;
                prev_boundary = src;
            }
            if src == limit {
                return src;
            }
            src += char_count(c);
            if self.is_decomp_yes(n16) {
                let cc = cc_from_yes_or_maybe_yes(n16);
                if prev_cc <= cc || cc == 0 {
                    prev_cc = cc;
                    if cc <= 1 {
                        prev_boundary = src;
                    }
                    continue;
                }
            }
            return prev_boundary;
        }
    }

    /// `decompose(c, norm16, buffer)`.
    fn decompose_cp(&self, mut c: u32, mut n16: u32, buf: &mut Buf) {
        if n16 >= self.limit_no_no {
            if self.is_maybe_yes_or_non_zero_cc(n16) {
                buf.append_cc(c, cc_from_yes_or_maybe_yes(n16));
                return;
            } else if n16 < self.min_maybe_no {
                c = self.map_algorithmic(c, n16);
                n16 = self.raw_norm16(c);
            }
        }
        if n16 < self.min_yes_no {
            buf.append_cc(c, 0);
        } else if self.is_hangul_lv(n16) || self.is_hangul_lvt(n16) {
            // `Hangul.decompose`.
            let c = c - HANGUL_BASE;
            let t = c % JAMO_T_COUNT;
            let c = c / JAMO_T_COUNT;
            buf.append_zero_cc(&[(JAMO_L_BASE + c / JAMO_V_COUNT) as u16, (JAMO_V_BASE + c % JAMO_V_COUNT) as u16]);
            if t != 0 {
                buf.append_zero_cc(&[(JAMO_T_BASE + t) as u16]);
            }
        } else {
            let mapping = self.data(n16);
            let first = self.x(mapping);
            let length = (first & MAPPING_LENGTH_MASK) as usize;
            let trail_cc = first >> 8;
            let lead_cc = if first & MAPPING_HAS_CCC_LCCC_WORD != 0 { self.x(mapping - 1) >> 8 } else { 0 };
            let m = self.x_slice(mapping + 1, length);
            buf.append_mapping(&m, true, lead_cc, trail_cc);
        }
    }

    /// `decompose(s, src, limit, buffer)` with a buffer (normalizing).
    fn decompose(&self, s: &[u16], buf: &mut Buf) {
        let min_no_cp = self.min_decomp_no_cp;
        let limit = s.len();
        let mut src = 0;
        loop {
            let prev_src = src;
            let mut c = 0;
            let mut n16 = 0;
            while src != limit {
                c = s[src] as u32;
                if c < min_no_cp || {
                    n16 = self.trie.get(c);
                    self.is_most_decomp_yes_and_zero_cc(n16)
                } {
                    src += 1;
                } else if !is_lead(c) {
                    break;
                } else if src + 1 != limit && is_trail(s[src + 1] as u32) {
                    c = cp_at(s, src);
                    n16 = self.trie.get(c);
                    if self.is_most_decomp_yes_and_zero_cc(n16) {
                        src += 2;
                    } else {
                        break;
                    }
                } else {
                    src += 1;
                }
            }
            if src != prev_src {
                buf.append_zero_cc(&s[prev_src..src]);
            }
            if src == limit {
                break;
            }
            src += char_count(c);
            self.decompose_cp(c, n16, buf);
        }
    }

    fn decompose_short(&self, s: &[u16], mut src: usize, limit: usize, stop_at_comp_boundary: bool, buf: &mut Buf) -> usize {
        while src < limit {
            let c = cp_at(s, src);
            if stop_at_comp_boundary && c < self.min_comp_no_maybe_cp {
                return src;
            }
            let n16 = self.norm16(c);
            if stop_at_comp_boundary && self.norm16_has_comp_boundary_before(n16) {
                return src;
            }
            src += char_count(c);
            self.decompose_cp(c, n16, buf);
            if stop_at_comp_boundary && self.norm16_has_comp_boundary_after(n16) {
                return src;
            }
        }
        src
    }

    fn combine(&self, mut list: usize, trail: u32) -> Option<u32> {
        if trail < COMP_1_TRAIL_LIMIT {
            let key1 = trail << 1;
            let mut first;
            loop {
                first = self.x(list);
                if key1 > first {
                    list += 2 + (first & COMP_1_TRIPLE) as usize;
                } else {
                    break;
                }
            }
            if key1 == first & COMP_1_TRAIL_MASK {
                return Some(if first & COMP_1_TRIPLE != 0 {
                    (self.x(list + 1) << 16) | self.x(list + 2)
                } else {
                    self.x(list + 1)
                });
            }
        } else {
            let key1 = COMP_1_TRAIL_LIMIT + ((trail >> COMP_1_TRAIL_SHIFT) & !COMP_1_TRIPLE);
            let key2 = (trail << COMP_2_TRAIL_SHIFT) & 0xffff;
            loop {
                let first = self.x(list);
                if key1 > first {
                    list += 2 + (first & COMP_1_TRIPLE) as usize;
                } else if key1 == first & COMP_1_TRAIL_MASK {
                    let second = self.x(list + 1);
                    if key2 > second {
                        if first & COMP_1_LAST_TUPLE != 0 {
                            break;
                        }
                        list += 3;
                    } else if key2 == second & COMP_2_TRAIL_MASK {
                        return Some(((second & !COMP_2_TRAIL_MASK) << 16) | self.x(list + 2));
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        None
    }

    fn recompose(&self, buf: &mut Buf, start: usize) {
        let mut p = start;
        if p == buf.s.len() {
            return;
        }
        let mut compositions: Option<usize> = None;
        let mut starter = 0usize;
        let mut starter_is_supplementary = false;
        let mut prev_cc = 0;
        loop {
            let c = cp_at(&buf.s, p);
            p += char_count(c);
            let n16 = self.norm16(c);
            let cc = cc_from_yes_or_maybe_yes(n16);
            if self.is_maybe(n16) && compositions.is_some() && (prev_cc < cc || prev_cc == 0) {
                if n16 == JAMO_VT {
                    if c < JAMO_T_BASE {
                        let prev = (buf.s[starter] as u32).wrapping_sub(JAMO_L_BASE);
                        if prev < JAMO_L_COUNT {
                            let remove = p - 1;
                            let mut syllable = HANGUL_BASE + (prev * JAMO_V_COUNT + (c - JAMO_V_BASE)) * JAMO_T_COUNT;
                            if p != buf.s.len() {
                                let t = (buf.s[p] as u32).wrapping_sub(JAMO_T_BASE);
                                if t < JAMO_T_COUNT {
                                    p += 1;
                                    syllable += t;
                                }
                            }
                            buf.s[starter] = syllable as u16;
                            buf.s.drain(remove..p);
                            p = remove;
                        }
                    }
                    if p == buf.s.len() {
                        break;
                    }
                    compositions = None;
                    continue;
                } else if let Some(cf) = self.combine(compositions.unwrap_or(0), c) {
                    let composite = cf >> 1;
                    let remove = p - char_count(c);
                    buf.s.drain(remove..p);
                    p = remove;
                    if starter_is_supplementary {
                        if composite > 0xffff {
                            let mut pair = Vec::new();
                            push_cp(&mut pair, composite);
                            buf.s[starter] = pair[0];
                            buf.s[starter + 1] = pair[1];
                        } else {
                            buf.s[starter] = c as u16;
                            buf.s.remove(starter + 1);
                            starter_is_supplementary = false;
                            p -= 1;
                        }
                    } else if composite > 0xffff {
                        starter_is_supplementary = true;
                        let mut pair = Vec::new();
                        push_cp(&mut pair, composite);
                        buf.s[starter] = pair[0];
                        buf.s.insert(starter + 1, pair[1]);
                        p += 1;
                    } else {
                        buf.s[starter] = composite as u16;
                    }
                    if p == buf.s.len() {
                        break;
                    }
                    compositions = if cf & 1 != 0 {
                        Some(self.compositions_list_for_composite(self.raw_norm16(composite)))
                    } else {
                        None
                    };
                    continue;
                }
            }
            prev_cc = cc;
            if p == buf.s.len() {
                break;
            }
            if cc == 0 {
                compositions = self.compositions_list_for_decomp_yes(n16);
                if compositions.is_some() {
                    if c <= 0xffff {
                        starter_is_supplementary = false;
                        starter = p - 1;
                    } else {
                        starter_is_supplementary = true;
                        starter = p - 2;
                    }
                }
            }
        }
        buf.flush();
    }

    /// `compose(s, src, limit, onlyContiguous=false, doCompose=true, buffer)`.
    fn compose(&self, s: &[u16], buf: &mut Buf) {
        let limit = s.len();
        let mut src = 0;
        let mut prev_boundary = 0;
        let min_no_maybe_cp = self.min_comp_no_maybe_cp;
        loop {
            let mut prev_src;
            let mut c;
            let mut n16: u32;
            loop {
                if src == limit {
                    if prev_boundary != limit {
                        buf.append_zero_cc(&s[prev_boundary..limit]);
                    }
                    return;
                }
                c = s[src] as u32;
                if c < min_no_maybe_cp || {
                    n16 = self.trie.get(c);
                    self.is_comp_yes_and_zero_cc(n16)
                } {
                    src += 1;
                } else {
                    prev_src = src;
                    src += 1;
                    if !is_lead(c) {
                        break;
                    }
                    if src != limit && is_trail(s[src] as u32) {
                        src += 1;
                        c = cp_at(s, prev_src);
                        n16 = self.trie.get(c);
                        if !self.is_comp_yes_and_zero_cc(n16) {
                            break;
                        }
                    }
                }
            }
            if n16 < self.min_maybe_no {
                if self.is_decomp_no_algorithmic(n16) {
                    if self.norm16_has_comp_boundary_after(n16) || self.has_comp_boundary_before_at(s, src) {
                        if prev_boundary != prev_src {
                            buf.append_zero_cc(&s[prev_boundary..prev_src]);
                        }
                        buf.append_cc(self.map_algorithmic(c, n16), 0);
                        prev_boundary = src;
                        continue;
                    }
                } else if n16 < self.min_no_no_comp_boundary_before {
                    if self.norm16_has_comp_boundary_after(n16) || self.has_comp_boundary_before_at(s, src) {
                        if prev_boundary != prev_src {
                            buf.append_zero_cc(&s[prev_boundary..prev_src]);
                        }
                        let mapping = self.data_for_yes_or_no(n16);
                        let length = (self.x(mapping) & MAPPING_LENGTH_MASK) as usize;
                        let m = self.x_slice(mapping + 1, length);
                        buf.append_zero_cc(&m);
                        prev_boundary = src;
                        continue;
                    }
                } else if n16 >= self.min_no_no_empty
                    && (self.has_comp_boundary_before_at(s, src) || self.has_comp_boundary_after_before(s, prev_boundary, prev_src))
                {
                    if prev_boundary != prev_src {
                        buf.append_zero_cc(&s[prev_boundary..prev_src]);
                    }
                    prev_boundary = src;
                    continue;
                }
            } else if n16 == JAMO_VT && prev_boundary != prev_src {
                let prev = s[prev_src - 1] as u32;
                if c < JAMO_T_BASE {
                    let l = prev.wrapping_sub(JAMO_L_BASE);
                    if l < JAMO_L_COUNT {
                        let t: i64 = if src != limit
                            && (s[src] as i64 - JAMO_T_BASE as i64) > 0
                            && (s[src] as i64 - (JAMO_T_BASE as i64)) < JAMO_T_COUNT as i64
                        {
                            let t = s[src] as i64 - JAMO_T_BASE as i64;
                            src += 1;
                            t
                        } else if self.has_comp_boundary_before_at(s, src) {
                            0
                        } else {
                            -1
                        };
                        if t >= 0 {
                            let syllable = HANGUL_BASE + (l * JAMO_V_COUNT + (c - JAMO_V_BASE)) * JAMO_T_COUNT + t as u32;
                            prev_src -= 1;
                            if prev_boundary != prev_src {
                                buf.append_zero_cc(&s[prev_boundary..prev_src]);
                            }
                            buf.append_zero_cc(&[syllable as u16]);
                            prev_boundary = src;
                            continue;
                        }
                    }
                } else if (HANGUL_BASE..HANGUL_BASE + HANGUL_COUNT).contains(&prev) && (prev - HANGUL_BASE).is_multiple_of(JAMO_T_COUNT) {
                    let syllable = prev + c - JAMO_T_BASE;
                    prev_src -= 1;
                    if prev_boundary != prev_src {
                        buf.append_zero_cc(&s[prev_boundary..prev_src]);
                    }
                    buf.append_zero_cc(&[syllable as u16]);
                    prev_boundary = src;
                    continue;
                }
            } else if n16 > JAMO_VT {
                let mut cc = cc_from_normal_yes_or_maybe(n16);
                let mut n16b;
                let mut cb;
                loop {
                    if src == limit {
                        buf.append_zero_cc(&s[prev_boundary..limit]);
                        // The text is appended as is: no reordering is needed.
                        buf.last_cc = 0;
                        return;
                    }
                    let prev_cc = cc;
                    cb = cp_at(s, src);
                    n16b = self.trie.get(cb);
                    if n16b >= MIN_YES_YES_WITH_CC {
                        cc = cc_from_normal_yes_or_maybe(n16b);
                        if prev_cc > cc {
                            break;
                        }
                    } else {
                        break;
                    }
                    src += char_count(cb);
                }
                if self.norm16_has_comp_boundary_before(n16b) {
                    if self.is_comp_yes_and_zero_cc(n16b) {
                        src += char_count(cb);
                    }
                    continue;
                }
            }
            // Slow path: decompose and recompose between the nearest boundaries.
            if prev_boundary != prev_src && !self.norm16_has_comp_boundary_before(n16) {
                let cp = cp_before(s, prev_src);
                let n = self.trie.get(cp);
                if !self.norm16_has_comp_boundary_after(n) {
                    prev_src -= char_count(cp);
                }
            }
            if prev_boundary != prev_src {
                buf.append_zero_cc(&s[prev_boundary..prev_src]);
            }
            let recompose_start = buf.len();
            self.decompose_short(s, prev_src, src, false, buf);
            src = self.decompose_short(s, src, limit, true, buf);
            self.recompose(buf, recompose_start);
            prev_boundary = src;
        }
    }
}

/// A normalization data set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Data {
    Nfc,
    Nfkc,
    NfkcCf,
    NfkcScf,
    Uts46,
    Utr30,
}

impl Data {
    /// `Normalizer2.getInstance(null, name, …)`'s names.
    pub(crate) fn by_name(name: &str) -> Option<Data> {
        Some(match name {
            "nfc" => Data::Nfc,
            "nfkc" => Data::Nfkc,
            "nfkc_cf" => Data::NfkcCf,
            "nfkc_scf" => Data::NfkcScf,
            "uts46" => Data::Uts46,
            _ => return None,
        })
    }

    pub(crate) fn imp(self) -> &'static Impl {
        static I: [OnceLock<Impl>; 6] =
            [OnceLock::new(), OnceLock::new(), OnceLock::new(), OnceLock::new(), OnceLock::new(), OnceLock::new()];
        I[self as usize].get_or_init(|| {
            Impl::load(match self {
                Data::Nfc => include_bytes!("data/nfc.nrm"),
                Data::Nfkc => include_bytes!("data/nfkc.nrm"),
                Data::NfkcCf => include_bytes!("data/nfkc_cf.nrm"),
                Data::NfkcScf => include_bytes!("data/nfkc_scf.nrm"),
                Data::Uts46 => include_bytes!("data/uts46.nrm"),
                Data::Utr30 => include_bytes!("data/utr30.nrm"),
            })
        })
    }
}

/// A configured `Normalizer2`: data, mode and an optional `unicode_set_filter`
/// (`FilteredNormalizer2`).
#[derive(Clone)]
pub(crate) struct Normalizer {
    pub data: Data,
    pub compose: bool,
    pub filter: Option<std::sync::Arc<USet>>,
}

impl Normalizer {
    fn normalize_plain(&self, s: &[u16], out: &mut Vec<u16>) {
        let imp = self.data.imp();
        let mut buf = Buf::new(imp, std::mem::take(out));
        if self.compose {
            imp.compose(s, &mut buf);
        } else {
            imp.decompose(s, &mut buf);
        }
        *out = buf.s;
    }

    /// `normalize(src, dest)`, appending to `out`.
    pub(crate) fn normalize_utf16(&self, s: &[u16], out: &mut Vec<u16>) {
        let Some(set) = &self.filter else {
            let mut tmp = Vec::with_capacity(s.len());
            self.normalize_plain(s, &mut tmp);
            out.extend_from_slice(&tmp);
            return;
        };
        let mut prev = 0;
        let mut contained = true;
        let mut tmp = Vec::new();
        while prev < s.len() {
            let limit = set.span(s, prev, contained);
            if limit > prev {
                if contained {
                    tmp.clear();
                    self.normalize_plain(&s[prev..limit], &mut tmp);
                    out.extend_from_slice(&tmp);
                } else {
                    out.extend_from_slice(&s[prev..limit]);
                }
            }
            contained = !contained;
            prev = limit;
        }
    }

    pub(crate) fn normalize(&self, s: &str) -> String {
        let u: Vec<u16> = s.encode_utf16().collect();
        let mut out = Vec::with_capacity(u.len());
        self.normalize_utf16(&u, &mut out);
        String::from_utf16_lossy(&out)
    }

    fn span_yes_plain(&self, s: &[u16]) -> usize {
        let imp = self.data.imp();
        if self.compose { imp.compose_span_yes(s) } else { imp.decompose_span_yes(s) }
    }

    /// `spanQuickCheckYes`.
    pub(crate) fn span_quick_check_yes(&self, s: &[u16]) -> usize {
        let Some(set) = &self.filter else { return self.span_yes_plain(s) };
        let mut prev = 0;
        let mut contained = true;
        while prev < s.len() {
            let limit = set.span(s, prev, contained);
            if contained {
                let yes = prev + self.span_yes_plain(&s[prev..limit]);
                if yes < limit {
                    return yes;
                }
            }
            contained = !contained;
            prev = limit;
        }
        s.len()
    }

    pub(crate) fn has_boundary_before(&self, c: u32) -> bool {
        self.filter.as_ref().is_some_and(|f| !f.contains(c)) || self.data.imp().has_boundary_before(c, self.compose)
    }

    pub(crate) fn has_boundary_after(&self, c: u32) -> bool {
        self.filter.as_ref().is_some_and(|f| !f.contains(c)) || self.data.imp().has_boundary_after(c, self.compose)
    }
}
