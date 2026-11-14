//! The Japanese char filters: `kuromoji_iteration_mark` (`JapaneseIterationMarkCharFilter`)
//! and the `CJKWidthCharFilter` the `kuromoji` analyzers read their input through (Lucene
//! 10.5.1).

use crate::error::Result;
use crate::stream::{CharFilter, OffsetMap};

const KANJI_ITERATION_MARK: u16 = 0x3005; // 々
const HIRAGANA_ITERATION_MARK: u16 = 0x309d; // ゝ
const HIRAGANA_VOICED_ITERATION_MARK: u16 = 0x309e; // ゞ
const KATAKANA_ITERATION_MARK: u16 = 0x30fd; // ヽ
const KATAKANA_VOICED_ITERATION_MARK: u16 = 0x30fe; // ヾ
const FULL_STOP_PUNCTUATION: u16 = 0x3002; // 。

/// Voiced forms of hiragana か (U+304B) to ぼ.
const H2D: [u16; 50] = [
    0x304c, 0x304c, 0x304e, 0x304e, 0x3050, 0x3050, 0x3052, 0x3052, 0x3054, 0x3054, 0x3056, 0x3056,
    0x3058, 0x3058, 0x305a, 0x305a, 0x305c, 0x305c, 0x305e, 0x305e, 0x3060, 0x3060, 0x3062, 0x3062,
    0x3063, 0x3065, 0x3065, 0x3067, 0x3067, 0x3069, 0x3069, 0x306a, 0x306b, 0x306c, 0x306d, 0x306e,
    0x3070, 0x3070, 0x3071, 0x3073, 0x3073, 0x3074, 0x3076, 0x3076, 0x3077, 0x3079, 0x3079, 0x307a,
    0x307c, 0x307c,
];
const HIRAGANA_BASE: u16 = 0x304b;
const KATAKANA_BASE: u16 = 0x30ab;

/// `kuromoji_iteration_mark`: iteration marks become the characters they repeat (one for one,
/// so offsets are unchanged).
pub(crate) struct IterationMark {
    pub kanji: bool,
    pub kana: bool,
    units: Vec<u16>,
}

impl IterationMark {
    pub fn new(kanji: bool, kana: bool) -> IterationMark {
        IterationMark {
            kanji,
            kana,
            units: Vec::new(),
        }
    }

    fn is_hiragana_mark(&self, c: u16) -> bool {
        self.kana && (c == HIRAGANA_ITERATION_MARK || c == HIRAGANA_VOICED_ITERATION_MARK)
    }
    fn is_katakana_mark(&self, c: u16) -> bool {
        self.kana && (c == KATAKANA_ITERATION_MARK || c == KATAKANA_VOICED_ITERATION_MARK)
    }
    fn is_mark(&self, c: u16) -> bool {
        (self.kanji && c == KANJI_ITERATION_MARK) || self.is_hiragana_mark(c) || self.is_katakana_mark(c)
    }

    fn normalize(&self, c: u16, m: u16) -> u16 {
        let (base, voiced_mark, plain_mark) = if self.is_hiragana_mark(m) {
            (HIRAGANA_BASE, HIRAGANA_VOICED_ITERATION_MARK, HIRAGANA_ITERATION_MARK)
        } else if self.is_katakana_mark(m) {
            (KATAKANA_BASE, KATAKANA_VOICED_ITERATION_MARK, KATAKANA_ITERATION_MARK)
        } else {
            return c;
        };
        let inside = c >= base && c < base + 50;
        let lookup = |c: u16| if inside { H2D[(c - base) as usize] + (base - HIRAGANA_BASE) } else { c };
        if m == plain_mark {
            if inside && c == lookup(c) { c - 1 } else { c }
        } else if m == voiced_mark {
            lookup(c)
        } else {
            c
        }
    }
}

impl CharFilter for IterationMark {
    fn filter(&mut self, input: &str, out: &mut String, _map: &mut OffsetMap) -> Result<()> {
        let mut units = std::mem::take(&mut self.units);
        units.clear();
        units.extend(input.encode_utf16());
        let mut res: Vec<u16> = Vec::with_capacity(units.len());
        let mut span_size: usize = 0;
        let mut span_end: usize = 0;
        for pos in 0..units.len() {
            let mut c = units[pos];
            if (0xD800..=0xDFFF).contains(&c) {
                span_end = pos + 1;
            }
            if c == FULL_STOP_PUNCTUATION {
                span_end = pos + 1;
            }
            if self.is_mark(c) {
                c = if pos < span_end {
                    self.normalize(units[pos - span_size], c)
                } else if pos == span_end {
                    span_end += 1;
                    c
                } else {
                    // The size of the run of marks starting here, at most back to the end of
                    // the last span.
                    let mut size = units[pos..].iter().take_while(|&&u| self.is_mark(u)).count();
                    if pos < size + span_end {
                        size = pos - span_end;
                    }
                    span_size = size;
                    span_end = pos + size;
                    self.normalize(units[pos - size], c)
                };
            }
            res.push(c);
        }
        crate::morph::push_utf16(out, &res);
        self.units = units;
        Ok(())
    }
}

/// Halfwidth katakana U+FF65..U+FF9F to fullwidth.
const KANA_NORM: [u16; 59] = [
    0x30fb, 0x30f2, 0x30a1, 0x30a3, 0x30a5, 0x30a7, 0x30a9, 0x30e3, 0x30e5, 0x30e7, 0x30c3, 0x30fc,
    0x30a2, 0x30a4, 0x30a6, 0x30a8, 0x30aa, 0x30ab, 0x30ad, 0x30af, 0x30b1, 0x30b3, 0x30b5, 0x30b7,
    0x30b9, 0x30bb, 0x30bd, 0x30bf, 0x30c1, 0x30c4, 0x30c6, 0x30c8, 0x30ca, 0x30cb, 0x30cc, 0x30cd,
    0x30ce, 0x30cf, 0x30d2, 0x30d5, 0x30d8, 0x30db, 0x30de, 0x30df, 0x30e0, 0x30e1, 0x30e2, 0x30e4,
    0x30e6, 0x30e8, 0x30e9, 0x30ea, 0x30eb, 0x30ec, 0x30ed, 0x30ef, 0x30f3, 0x3099, 0x309a,
];
/// Kana combining diffs U+30A6..U+30FD.
const KANA_COMBINE_VOICED: [u8; 88] = [
    78, 0, 0, 0, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 0, 1,
    0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 1, 0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 8, 8, 8, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
];
const KANA_COMBINE_SEMI_VOICED: [u8; 88] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 2, 0, 0, 2, 0, 0, 2, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];
const HW_KATAKANA_VOICED_MARK: u16 = 0xFF9E;
const HW_KATAKANA_SEMI_VOICED_MARK: u16 = 0xFF9F;

/// `CJKWidthCharFilter`: fullwidth ASCII to basic Latin and halfwidth katakana to fullwidth,
/// combining a halfwidth (semi-)voiced sound mark into the kana before it.
#[derive(Default)]
pub(crate) struct CjkWidth {
    units: Vec<u16>,
}

impl CharFilter for CjkWidth {
    fn filter(&mut self, input: &str, out: &mut String, map: &mut OffsetMap) -> Result<()> {
        if input.chars().all(|c| !('\u{FF01}'..='\u{FF9F}').contains(&c)) {
            out.push_str(input);
            return Ok(());
        }
        let mut res = std::mem::take(&mut self.units);
        res.clear();
        let mut prev: i32 = -1;
        let mut input_off: i32 = 0;
        for ch in input.encode_utf16() {
            input_off += 1;
            if ch == HW_KATAKANA_SEMI_VOICED_MARK || ch == HW_KATAKANA_VOICED_MARK {
                let mut combined = prev;
                if (0x30A6..=0x30FD).contains(&prev) {
                    let i = (prev - 0x30A6) as usize;
                    combined += if ch == HW_KATAKANA_SEMI_VOICED_MARK {
                        KANA_COMBINE_SEMI_VOICED[i]
                    } else {
                        KANA_COMBINE_VOICED[i]
                    } as i32;
                }
                if prev != combined {
                    prev = -1;
                    let diff = map.last_diff();
                    map.add((input_off - 1 - diff) as u32, diff + 1);
                    res.push(combined as u16);
                    continue;
                }
            }
            if prev != -1 {
                res.push(prev as u16);
            }
            prev = if (0xFF01..=0xFF5E).contains(&ch) {
                (ch - 0xFEE0) as i32
            } else if (0xFF65..=0xFF9F).contains(&ch) {
                KANA_NORM[(ch - 0xFF65) as usize] as i32
            } else {
                ch as i32
            };
        }
        if prev != -1 {
            res.push(prev as u16);
        }
        crate::morph::push_utf16(out, &res);
        self.units = res;
        Ok(())
    }
}
