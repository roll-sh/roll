//! `word_delimiter` and `word_delimiter_graph` (Lucene's `WordDelimiterFilter`,
//! `WordDelimiterGraphFilter` and `WordDelimiterIterator`).
//!
//! Both split a token into subwords at delimiters, case changes and letter/digit transitions, and
//! optionally emit catenations and the original. Lucene works on UTF-16 code units (character
//! types are looked up per unit, surrogates never split) and computes offsets and positions from
//! unit indices, so the term is held as UTF-16 here too. The legacy filter emits a flat stream
//! sorted by start offset; the graph filter emits a correct token graph (position lengths on
//! catenations and the original) for query-time phrase building. Buffers are reused across
//! tokens.

use std::sync::Arc;

use super::WordSet;
use crate::component::{FilterFactory, SynonymRole};
use crate::error::{AnalysisError, Result};
use crate::registry::{word_list, word_set};
use crate::settings::Settings;
use crate::stream::{Filter, TokenStream};
use crate::token::{Tok, attr};
use crate::unicode::{self, gc};

pub(crate) const LOWER: u8 = 0x01;
pub(crate) const UPPER: u8 = 0x02;
pub(crate) const DIGIT: u8 = 0x04;
pub(crate) const SUBWORD_DELIM: u8 = 0x08;
pub(crate) const ALPHA: u8 = 0x03;
pub(crate) const ALPHANUM: u8 = 0x07;

pub(crate) const GENERATE_WORD_PARTS: u32 = 1;
pub(crate) const GENERATE_NUMBER_PARTS: u32 = 2;
pub(crate) const CATENATE_WORDS: u32 = 4;
pub(crate) const CATENATE_NUMBERS: u32 = 8;
pub(crate) const CATENATE_ALL: u32 = 16;
pub(crate) const PRESERVE_ORIGINAL: u32 = 32;
pub(crate) const SPLIT_ON_CASE_CHANGE: u32 = 64;
pub(crate) const SPLIT_ON_NUMERICS: u32 = 128;
pub(crate) const STEM_ENGLISH_POSSESSIVE: u32 = 256;
pub(crate) const IGNORE_KEYWORDS: u32 = 512;

/// The flags of the pre-configured filters.
pub(crate) const DEFAULT_FLAGS: u32 = GENERATE_WORD_PARTS
    | GENERATE_NUMBER_PARTS
    | SPLIT_ON_CASE_CHANGE
    | SPLIT_ON_NUMERICS
    | STEM_ENGLISH_POSSESSIVE;

/// `WordDelimiterIterator.getType` for a UTF-16 unit.
pub(crate) fn get_type(u: u16) -> u8 {
    let Some(c) = char::from_u32(u as u32) else {
        // Surrogates: never split a pair.
        return ALPHA | DIGIT;
    };
    match unicode::get_type(c) {
        gc::UPPERCASE_LETTER => UPPER,
        gc::LOWERCASE_LETTER => LOWER,
        gc::TITLECASE_LETTER
        | gc::MODIFIER_LETTER
        | gc::OTHER_LETTER
        | gc::NON_SPACING_MARK
        | gc::ENCLOSING_MARK
        | gc::COMBINING_SPACING_MARK => ALPHA,
        gc::DECIMAL_DIGIT_NUMBER | gc::LETTER_NUMBER | gc::OTHER_NUMBER => DIGIT,
        _ => SUBWORD_DELIM,
    }
}

/// `DEFAULT_WORD_DELIM_TABLE` (Latin-1, classified with `isLowerCase`/`isUpperCase`/`isDigit`).
pub(crate) fn default_table() -> Arc<Vec<u8>> {
    static T: std::sync::OnceLock<Arc<Vec<u8>>> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        Arc::new(
            (0u32..256)
                .map(|i| {
                    let c = char::from_u32(i).unwrap_or('\0');
                    if unicode::is_lowercase(c) {
                        LOWER
                    } else if unicode::is_uppercase(c) {
                        UPPER
                    } else if unicode::is_digit(c) {
                        DIGIT
                    } else {
                        SUBWORD_DELIM
                    }
                })
                .collect(),
        )
    })
    .clone()
}

const DONE: usize = usize::MAX;

fn is_alpha(t: u8) -> bool {
    t & ALPHA != 0
}
fn is_digit(t: u8) -> bool {
    t & DIGIT != 0
}
fn is_delim(t: u8) -> bool {
    t & SUBWORD_DELIM != 0
}
fn is_upper(t: u8) -> bool {
    t & UPPER != 0
}

/// `WordDelimiterIterator` over a UTF-16 buffer.
struct Iter {
    table: Arc<Vec<u8>>,
    split_case: bool,
    split_numerics: bool,
    possessive: bool,
    len: usize,
    start_bounds: usize,
    end_bounds: usize,
    current: usize,
    end: usize,
    has_final_possessive: bool,
    skip_possessive: bool,
}

impl Iter {
    fn char_type(&self, u: u16) -> u8 {
        match self.table.get(u as usize) {
            Some(&t) => t,
            None => get_type(u),
        }
    }

    fn set_text(&mut self, text: &[u16]) {
        self.len = text.len();
        self.end_bounds = text.len();
        self.current = 0;
        self.start_bounds = 0;
        self.end = 0;
        self.skip_possessive = false;
        self.has_final_possessive = false;
        while self.start_bounds < self.len && is_delim(self.char_type(text[self.start_bounds])) {
            self.start_bounds += 1;
        }
        while self.end_bounds > self.start_bounds
            && is_delim(self.char_type(text[self.end_bounds - 1]))
        {
            self.end_bounds -= 1;
        }
        if self.ends_with_possessive(text, self.end_bounds) {
            self.has_final_possessive = true;
        }
        self.current = self.start_bounds;
    }

    fn ends_with_possessive(&self, text: &[u16], pos: usize) -> bool {
        self.possessive
            && pos > 2
            && text[pos - 2] == b'\'' as u16
            && (text[pos - 1] == b's' as u16 || text[pos - 1] == b'S' as u16)
            && is_alpha(self.char_type(text[pos - 3]))
            && (pos == self.end_bounds || is_delim(self.char_type(text[pos])))
    }

    fn is_break(&self, last: u8, ty: u8) -> bool {
        if ty & last != 0 {
            return false;
        }
        let case_join = (!self.split_case || is_upper(last)) && is_alpha(last) && is_alpha(ty);
        let numeric_join = !self.split_numerics
            && ((is_alpha(last) && is_digit(ty)) || (is_digit(last) && is_alpha(ty)));
        !(case_join || numeric_join)
    }

    fn next(&mut self, text: &[u16]) -> usize {
        self.current = self.end;
        if self.current == DONE {
            return DONE;
        }
        if self.skip_possessive {
            self.current += 2;
            self.skip_possessive = false;
        }
        let mut last = 0;
        while self.current < self.end_bounds {
            last = self.char_type(text[self.current]);
            if !is_delim(last) {
                break;
            }
            self.current += 1;
        }
        if self.current >= self.end_bounds {
            self.end = DONE;
            return DONE;
        }
        self.end = self.current + 1;
        while self.end < self.end_bounds {
            let ty = self.char_type(text[self.end]);
            if self.is_break(last, ty) {
                break;
            }
            last = ty;
            self.end += 1;
        }
        if self.end < self.end_bounds.saturating_sub(1)
            && self.ends_with_possessive(text, self.end + 2)
        {
            self.skip_possessive = true;
        }
        self.end
    }

    fn ty(&self, text: &[u16]) -> u8 {
        if self.end == DONE {
            return 0;
        }
        match self.char_type(text[self.current]) {
            LOWER | UPPER => ALPHA,
            t => t,
        }
    }

    fn is_single_word(&self) -> bool {
        if self.has_final_possessive {
            self.current == self.start_bounds && self.end == self.end_bounds - 2
        } else {
            self.current == self.start_bounds && self.end == self.end_bounds
        }
    }
}

#[derive(Clone)]
pub(crate) struct Config {
    pub flags: u32,
    pub table: Arc<Vec<u8>>,
    pub protected: Option<Arc<WordSet>>,
    pub adjust_offsets: bool,
}

impl Config {
    fn has(&self, f: u32) -> bool {
        self.flags & f != 0
    }

    fn iter(&self) -> Iter {
        Iter {
            table: self.table.clone(),
            split_case: self.has(SPLIT_ON_CASE_CHANGE),
            split_numerics: self.has(SPLIT_ON_NUMERICS),
            possessive: self.has(STEM_ENGLISH_POSSESSIVE),
            len: 0,
            start_bounds: 0,
            end_bounds: 0,
            current: 0,
            end: 0,
            has_final_possessive: false,
            skip_possessive: false,
        }
    }

    fn should_concatenate(&self, ty: u8) -> bool {
        (self.has(CATENATE_WORDS) && is_alpha(ty)) || (self.has(CATENATE_NUMBERS) && is_digit(ty))
    }

    fn should_generate(&self, ty: u8) -> bool {
        (self.has(GENERATE_WORD_PARTS) && is_alpha(ty))
            || (self.has(GENERATE_NUMBER_PARTS) && is_digit(ty))
    }
}

fn push_utf16(out: &mut String, units: &[u16]) {
    out.extend(char::decode_utf16(units.iter().copied()).map(|r| r.unwrap_or('\u{FFFD}')));
}

fn is_protected(cfg: &Config, term: &str, scratch: &mut String) -> bool {
    cfg.protected
        .as_ref()
        .is_some_and(|p| p.contains(term, scratch))
}

#[derive(Default)]
struct Concat {
    buf: Vec<u16>,
    start: usize,
    end: usize,
    start_pos: usize,
    ty: u8,
    count: usize,
}

impl Concat {
    fn clear(&mut self) {
        self.buf.clear();
        self.start = 0;
        self.end = 0;
        self.ty = 0;
        self.count = 0;
    }
}

// ---------------------------------------------------------------------------------------------
// The legacy filter.

/// One buffered output token of [`WordDelimiter`].
struct Part {
    tok: Tok,
    start: u32,
    pos_inc: u32,
}

/// `WordDelimiterFilter`.
pub(crate) struct WordDelimiter {
    cfg: Arc<Config>,
    it: Iter,
    text: Vec<u16>,
    concat: Concat,
    concat_all: Concat,
    last_concat_count: usize,
    accum_pos_inc: u32,
    saved_start: u32,
    saved_end: u32,
    saved_type: &'static str,
    has_saved_state: bool,
    has_illegal_offsets: bool,
    has_output_token: bool,
    has_output_following_original: bool,
    first: bool,
    parts: Vec<Part>,
    spare: Vec<Part>,
    pos: usize,
    scratch: String,
}

impl WordDelimiter {
    pub fn new(cfg: Arc<Config>) -> Self {
        let it = cfg.iter();
        WordDelimiter {
            cfg,
            it,
            text: Vec::new(),
            concat: Concat::default(),
            concat_all: Concat::default(),
            last_concat_count: 0,
            accum_pos_inc: 0,
            saved_start: 0,
            saved_end: 0,
            saved_type: crate::token::WORD,
            has_saved_state: false,
            has_illegal_offsets: false,
            has_output_token: false,
            has_output_following_original: false,
            first: true,
            parts: Vec::new(),
            spare: Vec::new(),
            pos: 0,
            scratch: String::new(),
        }
    }

    fn position(&mut self, inject: bool) -> u32 {
        let pos_inc = self.accum_pos_inc;
        if self.has_output_token {
            self.accum_pos_inc = 0;
            return if inject { 0 } else { pos_inc.max(1) };
        }
        self.has_output_token = true;
        if !self.has_output_following_original {
            self.has_output_following_original = true;
            return 0;
        }
        self.accum_pos_inc = 0;
        pos_inc.max(1)
    }

    fn generate_part(&mut self, t: &mut Tok, single: bool) {
        t.clear();
        push_utf16(&mut t.term, &self.text[self.it.current..self.it.end]);
        let start = self.saved_start + self.it.current as u32;
        let end = self.saved_start + self.it.end as u32;
        if self.has_illegal_offsets {
            if single && start <= self.saved_end {
                t.start = start;
                t.end = self.saved_end;
            } else {
                t.start = self.saved_start;
                t.end = self.saved_end;
            }
        } else {
            t.start = start;
            t.end = end;
        }
        t.pos_inc = self.position(false);
        t.kind = self.saved_type;
    }

    fn concatenate(&mut self, all: bool) {
        let c = if all {
            &mut self.concat_all
        } else {
            &mut self.concat
        };
        if c.buf.is_empty() {
            c.start = self.saved_start as usize + self.it.current;
        }
        c.buf
            .extend_from_slice(&self.text[self.it.current..self.it.end]);
        c.count += 1;
        c.end = self.saved_start as usize + self.it.end;
    }

    /// `WordDelimiterConcatenation.write` + `clear`, into `t`.
    fn write_concat(&mut self, all: bool, t: &mut Tok) {
        t.clear();
        let (start, end) = {
            let c = if all { &self.concat_all } else { &self.concat };
            push_utf16(&mut t.term, &c.buf);
            (c.start, c.end)
        };
        if self.has_illegal_offsets {
            t.start = self.saved_start;
            t.end = self.saved_end;
        } else {
            t.start = start as u32;
            t.end = end as u32;
        }
        t.pos_inc = self.position(true);
        t.kind = self.saved_type;
        self.accum_pos_inc = 0;
        if all {
            self.concat_all.clear()
        } else {
            self.concat.clear()
        }
    }

    /// `flushConcatenation`: true when it wrote a token into `t`.
    fn flush_concat(&mut self, t: &mut Tok) -> bool {
        self.last_concat_count = self.concat.count;
        if self.concat.count != 1 || !self.cfg.should_generate(self.concat.ty) {
            self.write_concat(false, t);
            return true;
        }
        self.concat.clear();
        false
    }

    fn buffer(&mut self, t: &Tok) {
        let mut p = self.spare.pop().unwrap_or(Part {
            tok: Tok::default(),
            start: 0,
            pos_inc: 0,
        });
        p.tok.clone_from(t);
        p.start = t.start;
        p.pos_inc = t.pos_inc;
        self.parts.push(p);
    }
}

impl Filter for WordDelimiter {
    fn reset(&mut self) {
        self.has_saved_state = false;
        self.concat.clear();
        self.concat_all.clear();
        self.accum_pos_inc = 0;
        self.spare.append(&mut self.parts);
        self.pos = 0;
        self.first = true;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        let cfg = self.cfg.clone();
        loop {
            if !self.has_saved_state {
                if !input.next(t) {
                    return false;
                }
                if cfg.has(IGNORE_KEYWORDS) && t.keyword {
                    return true;
                }
                self.accum_pos_inc += t.pos_inc;
                self.text.clear();
                self.text.extend(t.term.encode_utf16());
                self.it.set_text(&self.text);
                self.it.next(&self.text);
                if (self.it.current == 0 && self.it.end == self.text.len())
                    || is_protected(&cfg, &t.term, &mut self.scratch)
                {
                    t.pos_inc = self.accum_pos_inc;
                    self.accum_pos_inc = 0;
                    self.first = false;
                    return true;
                }
                if self.it.end == DONE && !cfg.has(PRESERVE_ORIGINAL) {
                    if t.pos_inc == 1 && !self.first {
                        self.accum_pos_inc -= 1;
                    }
                    continue;
                }
                // saveState
                self.saved_start = t.start;
                self.saved_end = t.end;
                self.has_illegal_offsets =
                    (t.end as i64 - t.start as i64) != self.text.len() as i64;
                self.saved_type = t.kind;
                self.has_saved_state = true;
                self.has_output_token = false;
                self.has_output_following_original = !cfg.has(PRESERVE_ORIGINAL);
                self.last_concat_count = 0;
                if cfg.has(PRESERVE_ORIGINAL) {
                    t.pos_inc = self.accum_pos_inc;
                    self.accum_pos_inc = 0;
                    self.first = false;
                    return true;
                }
            }
            if self.it.end == DONE {
                if !self.concat.buf.is_empty() && self.flush_concat(t) {
                    self.buffer(t);
                    continue;
                }
                if !self.concat_all.buf.is_empty() {
                    if self.concat_all.count > self.last_concat_count {
                        self.write_concat(true, t);
                        self.buffer(t);
                        continue;
                    }
                    self.concat_all.clear();
                }
                if self.pos < self.parts.len() {
                    if self.pos == 0 {
                        // Stable: by start offset, then larger increments first.
                        self.parts
                            .sort_by(|a, b| a.start.cmp(&b.start).then(b.pos_inc.cmp(&a.pos_inc)));
                    }
                    t.clone_from(&self.parts[self.pos].tok);
                    self.pos += 1;
                    if self.first && t.pos_inc == 0 {
                        t.pos_inc = 1;
                    }
                    self.first = false;
                    return true;
                }
                self.spare.append(&mut self.parts);
                self.pos = 0;
                self.has_saved_state = false;
                continue;
            }
            if self.it.is_single_word() {
                self.generate_part(t, true);
                self.it.next(&self.text);
                self.first = false;
                return true;
            }
            let ty = self.it.ty(&self.text);
            if !self.concat.buf.is_empty() && (self.concat.ty & ty) == 0 {
                if self.flush_concat(t) {
                    self.has_output_token = false;
                    self.buffer(t);
                    continue;
                }
                self.has_output_token = false;
            }
            if cfg.should_concatenate(ty) {
                if self.concat.buf.is_empty() {
                    self.concat.ty = ty;
                }
                self.concatenate(false);
            }
            if cfg.has(CATENATE_ALL) {
                self.concatenate(true);
            }
            if cfg.should_generate(ty) {
                self.generate_part(t, false);
                self.buffer(t);
            }
            self.it.next(&self.text);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The graph filter.

/// A buffered part of [`WordDelimiterGraph`]: positions and unit range, and the catenated text
/// (None: the range of the saved term).
struct GraphPart {
    start_pos: usize,
    end_pos: usize,
    start_part: usize,
    end_part: usize,
    text: Option<usize>,
}

/// `WordDelimiterGraphFilter`.
pub(crate) struct WordDelimiterGraph {
    cfg: Arc<Config>,
    it: Iter,
    text: Vec<u16>,
    concat: Concat,
    concat_all: Concat,
    last_concat_count: usize,
    accum_pos_inc: u32,
    saved: Tok,
    has_saved: bool,
    saved_start: u32,
    saved_end: u32,
    adjusting_offsets: bool,
    word_pos: usize,
    last_start_offset: u32,
    parts: Vec<GraphPart>,
    texts: Vec<Vec<u16>>,
    ntexts: usize,
    pos: usize,
    scratch: String,
}

impl WordDelimiterGraph {
    pub fn new(cfg: Arc<Config>) -> Self {
        let it = cfg.iter();
        WordDelimiterGraph {
            cfg,
            it,
            text: Vec::new(),
            concat: Concat::default(),
            concat_all: Concat::default(),
            last_concat_count: 0,
            accum_pos_inc: 0,
            saved: Tok::default(),
            has_saved: false,
            saved_start: 0,
            saved_end: 0,
            adjusting_offsets: false,
            word_pos: 0,
            last_start_offset: 0,
            parts: Vec::new(),
            texts: Vec::new(),
            ntexts: 0,
            pos: 0,
            scratch: String::new(),
        }
    }

    fn buffer(
        &mut self,
        text: Option<&[u16]>,
        start_pos: usize,
        end_pos: usize,
        start_part: usize,
        end_part: usize,
    ) {
        let text = text.map(|t| {
            if self.ntexts == self.texts.len() {
                self.texts.push(Vec::new());
            }
            let slot = &mut self.texts[self.ntexts];
            slot.clear();
            slot.extend_from_slice(t);
            self.ntexts += 1;
            self.ntexts - 1
        });
        self.parts.push(GraphPart {
            start_pos,
            end_pos,
            start_part,
            end_part,
            text,
        });
    }

    fn concatenate(&mut self, all: bool) {
        let ty = self.it.ty(&self.text);
        let word_pos = self.word_pos;
        let c = if all {
            &mut self.concat_all
        } else {
            &mut self.concat
        };
        if c.buf.is_empty() {
            c.ty = ty;
            c.start = self.it.current;
            c.start_pos = word_pos;
        }
        c.buf
            .extend_from_slice(&self.text[self.it.current..self.it.end]);
        c.count += 1;
        c.end = self.it.end;
    }

    fn write_concat(&mut self, all: bool) {
        let c = if all {
            std::mem::take(&mut self.concat_all)
        } else {
            std::mem::take(&mut self.concat)
        };
        self.buffer(Some(&c.buf), c.start_pos, self.word_pos, c.start, c.end);
        if all {
            self.concat_all = c
        } else {
            self.concat = c
        }
    }

    fn flush_concat(&mut self) {
        if self.word_pos == self.concat.start_pos {
            self.word_pos += 1;
        }
        self.last_concat_count = self.concat.count;
        if self.concat.count != 1 || !self.cfg.should_generate(self.concat.ty) {
            self.write_concat(false);
        }
        self.concat.clear();
    }

    fn buffer_word_parts(&mut self, t: &Tok) {
        let cfg = self.cfg.clone();
        self.saved.clone_from(t);
        self.has_saved = true;
        self.saved_start = t.start;
        self.saved_end = t.end;
        let len = self.text.len();
        self.adjusting_offsets =
            cfg.adjust_offsets && (t.end as i64 - t.start as i64) == len as i64;
        self.parts.clear();
        self.ntexts = 0;
        self.last_concat_count = 0;
        self.word_pos = 0;
        if cfg.has(PRESERVE_ORIGINAL) {
            self.buffer(None, 0, 1, 0, len);
        }
        if self.it.is_single_word() {
            let (c, e) = (self.it.current, self.it.end);
            self.buffer(None, self.word_pos, self.word_pos + 1, c, e);
            self.word_pos += 1;
            self.it.next(&self.text);
        } else {
            while self.it.end != DONE {
                let ty = self.it.ty(&self.text);
                if !self.concat.buf.is_empty() && (self.concat.ty & ty) == 0 {
                    self.flush_concat();
                }
                if cfg.should_concatenate(ty) {
                    self.concatenate(false);
                }
                if cfg.has(CATENATE_ALL) {
                    self.concatenate(true);
                }
                if cfg.should_generate(ty) {
                    let (c, e) = (self.it.current, self.it.end);
                    self.buffer(None, self.word_pos, self.word_pos + 1, c, e);
                    self.word_pos += 1;
                }
                self.it.next(&self.text);
            }
            if !self.concat.buf.is_empty() {
                self.flush_concat();
            }
            if !self.concat_all.buf.is_empty() {
                if self.concat_all.count > self.last_concat_count {
                    if self.word_pos == self.concat_all.start_pos {
                        self.word_pos += 1;
                    }
                    self.write_concat(true);
                }
                self.concat_all.clear();
            }
        }
        if cfg.has(PRESERVE_ORIGINAL) {
            if self.word_pos == 0 {
                self.word_pos += 1;
            }
            self.parts[0].end_pos = self.word_pos;
        }
        let from = if cfg.has(PRESERVE_ORIGINAL) { 1 } else { 0 };
        // Stable: by start unit, then longer parts first.
        self.parts[from..].sort_by(|a, b| {
            a.start_part
                .cmp(&b.start_part)
                .then(b.end_part.cmp(&a.end_part))
        });
        self.word_pos = 0;
        self.pos = 0;
    }
}

impl Filter for WordDelimiterGraph {
    fn reset(&mut self) {
        self.accum_pos_inc = 0;
        self.has_saved = false;
        self.last_start_offset = 0;
        self.concat.clear();
        self.concat_all.clear();
        self.parts.clear();
        self.pos = 0;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        let cfg = self.cfg.clone();
        loop {
            if !self.has_saved {
                if !input.next(t) {
                    return false;
                }
                if cfg.has(IGNORE_KEYWORDS) && t.keyword {
                    return true;
                }
                self.accum_pos_inc += t.pos_inc;
                self.text.clear();
                self.text.extend(t.term.encode_utf16());
                self.it.set_text(&self.text);
                self.it.next(&self.text);
                if (self.it.current == 0 && self.it.end == self.text.len())
                    || is_protected(&cfg, &t.term, &mut self.scratch)
                {
                    t.pos_inc = self.accum_pos_inc;
                    self.accum_pos_inc = 0;
                    return true;
                }
                if self.it.end == DONE {
                    if !cfg.has(PRESERVE_ORIGINAL) {
                        continue;
                    }
                    self.accum_pos_inc = 0;
                    return true;
                }
                self.buffer_word_parts(t);
            }
            if self.pos < self.parts.len() {
                let p = &self.parts[self.pos];
                let (start_pos, end_pos, start_part, end_part, text) =
                    (p.start_pos, p.end_pos, p.start_part, p.end_part, p.text);
                self.pos += 1;
                t.clone_from(&self.saved);
                let (mut start, mut end) = if self.adjusting_offsets {
                    (
                        self.saved_start + start_part as u32,
                        self.saved_start + end_part as u32,
                    )
                } else {
                    (self.saved_start, self.saved_end)
                };
                start = start.max(self.last_start_offset);
                end = end.max(self.last_start_offset);
                t.start = start;
                t.end = end;
                self.last_start_offset = start;
                t.term.clear();
                match text {
                    None => push_utf16(&mut t.term, &self.text[start_part..end_part]),
                    Some(i) => push_utf16(&mut t.term, &self.texts[i]),
                }
                t.pos_inc = (self.accum_pos_inc as i64 + start_pos as i64 - self.word_pos as i64)
                    .max(0) as u32;
                self.accum_pos_inc = 0;
                t.pos_len = (end_pos - start_pos) as u32;
                self.word_pos = start_pos;
                return true;
            }
            self.has_saved = false;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Factories.

/// `WordDelimiterTokenFilterFactory.parseTypes`.
fn parse_types(rules: &[String]) -> Result<Vec<u8>> {
    static RULE: std::sync::OnceLock<crate::jregex::JavaRegex> = std::sync::OnceLock::new();
    let re = RULE.get_or_init(|| {
        crate::jregex::compile("(.*)\\s*=>\\s*(.*)\\s*$", 0).expect("valid pattern")
    });
    let mut caps = re.captures();
    let mut map: std::collections::BTreeMap<u16, u8> = std::collections::BTreeMap::new();
    let runtime = |m: String| AnalysisError::other("runtime_exception", 500, m);
    for rule in rules {
        if !re.find_at(rule, 0, &mut caps) {
            return Err(runtime(format!("Invalid Mapping Rule : [{rule}]")));
        }
        let g = |i| caps.get(i).map_or("", |(s, e)| &rule[s..e]);
        let lhs = parse_string(g(1).trim_matches(|c: char| c <= ' '))?;
        let rhs = match g(2).trim_matches(|c: char| c <= ' ') {
            "LOWER" => Some(LOWER),
            "UPPER" => Some(UPPER),
            "ALPHA" => Some(ALPHA),
            "DIGIT" => Some(DIGIT),
            "ALPHANUM" => Some(ALPHANUM),
            "SUBWORD_DELIM" => Some(SUBWORD_DELIM),
            _ => None,
        };
        if lhs.len() != 1 {
            return Err(runtime(format!(
                "Invalid Mapping Rule : [{rule}]. Only a single character is allowed."
            )));
        }
        let Some(rhs) = rhs else {
            return Err(runtime(format!(
                "Invalid Mapping Rule : [{rule}]. Illegal type."
            )));
        };
        map.insert(lhs[0], rhs);
    }
    let size = map
        .keys()
        .next_back()
        .map_or(0, |&k| k as usize + 1)
        .max(256);
    let mut types: Vec<u8> = (0..size).map(|i| get_type(i as u16)).collect();
    for (k, v) in map {
        types[k as usize] = v;
    }
    Ok(types)
}

/// `parseString` of the type table (`\\ \n \t \r \b \f \uXXXX`), as UTF-16 units.
fn parse_string(s: &str) -> Result<Vec<u16>> {
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut out = Vec::with_capacity(units.len());
    let invalid = || {
        AnalysisError::other(
            "runtime_exception",
            500,
            format!("Invalid escaped char in [{s}]"),
        )
    };
    let mut i = 0;
    while i < units.len() {
        let mut c = units[i];
        i += 1;
        if c == b'\\' as u16 {
            if i >= units.len() {
                return Err(invalid());
            }
            c = units[i];
            i += 1;
            c = match c {
                0x6E => 0x0A,
                0x74 => 0x09,
                0x72 => 0x0D,
                0x62 => 0x08,
                0x66 => 0x0C,
                0x75 => {
                    if i + 3 >= units.len() {
                        return Err(invalid());
                    }
                    let hex = String::from_utf16_lossy(&units[i..i + 4]);
                    i += 4;
                    match i32::from_str_radix(&hex, 16) {
                        Ok(v) => v as u16,
                        Err(_) => {
                            return Err(AnalysisError::other(
                                "number_format_exception",
                                400,
                                format!("For input string: \"{hex}\" under radix 16"),
                            ));
                        }
                    }
                }
                other => other,
            };
        }
        out.push(c);
    }
    Ok(out)
}

struct WdFactory {
    cfg: Arc<Config>,
    graph: bool,
}

impl FilterFactory for WdFactory {
    fn create(&self) -> Box<dyn Filter> {
        if self.graph {
            Box::new(WordDelimiterGraph::new(self.cfg.clone()))
        } else {
            Box::new(WordDelimiter::new(self.cfg.clone()))
        }
    }
    fn attrs(&self) -> u32 {
        // Both declare a keyword attribute (for `ignore_keywords`).
        attr::KEYWORD
    }
    fn synonym_role(&self) -> SynonymRole {
        SynonymRole::Forbidden
    }
}

/// `word_delimiter` / `word_delimiter_graph`.
pub(crate) fn factory(s: &Settings, graph: bool) -> Result<Arc<dyn FilterFactory>> {
    let table = match word_list(s, "type_table")? {
        None => default_table(),
        // `parseTypes` sizes the table from `typeMap.lastKey()`, which throws on no rules.
        Some(rules) if rules.is_empty() => {
            return Err(AnalysisError::other("no_such_element_exception", 500, ""));
        }
        Some(rules) => Arc::new(parse_types(&rules)?),
    };
    let mut flags = 0;
    for (flag, key, default) in [
        (GENERATE_WORD_PARTS, "generate_word_parts", true),
        (GENERATE_NUMBER_PARTS, "generate_number_parts", true),
        (CATENATE_WORDS, "catenate_words", false),
        (CATENATE_NUMBERS, "catenate_numbers", false),
        (CATENATE_ALL, "catenate_all", false),
        (SPLIT_ON_CASE_CHANGE, "split_on_case_change", true),
        (PRESERVE_ORIGINAL, "preserve_original", false),
        (SPLIT_ON_NUMERICS, "split_on_numerics", true),
        (STEM_ENGLISH_POSSESSIVE, "stem_english_possessive", true),
    ] {
        if s.get_bool(key, default)? {
            flags |= flag;
        }
    }
    if graph && s.get_bool("ignore_keywords", false)? {
        flags |= IGNORE_KEYWORDS;
    }
    let protected = word_set(s, "protected_words")?.map(Arc::new);
    let adjust_offsets = if graph {
        s.get_bool("adjust_offsets", true)?
    } else {
        false
    };
    Ok(Arc::new(WdFactory {
        cfg: Arc::new(Config {
            flags,
            table,
            protected,
            adjust_offsets,
        }),
        graph,
    }))
}

pub(crate) fn prebuilt(graph: bool) -> Arc<dyn FilterFactory> {
    Arc::new(WdFactory {
        cfg: Arc::new(Config {
            flags: DEFAULT_FLAGS,
            table: default_table(),
            protected: None,
            adjust_offsets: true,
        }),
        graph,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::tests::run_filters;

    fn cfg(flags: u32) -> Arc<Config> {
        Arc::new(Config {
            flags,
            table: default_table(),
            protected: None,
            adjust_offsets: true,
        })
    }

    #[test]
    fn splits_like_lucene() {
        let out = run_filters(
            vec![Box::new(WordDelimiter::new(cfg(DEFAULT_FLAGS)))],
            "PowerShot SD500 O'Neil's wi-fi",
        );
        let terms: Vec<_> = out.iter().map(|t| t.0.as_str()).collect();
        assert_eq!(
            terms,
            vec!["Power", "Shot", "SD", "500", "O", "Neil", "wi", "fi"]
        );
        let out = run_filters(
            vec![Box::new(WordDelimiter::new(cfg(DEFAULT_FLAGS
                | CATENATE_ALL
                | PRESERVE_ORIGINAL)))],
            "wi-fi",
        );
        let terms: Vec<_> = out.iter().map(|t| (t.0.as_str(), t.1)).collect();
        assert_eq!(terms, vec![("wi-fi", 1), ("wi", 0), ("wifi", 0), ("fi", 1)]);
        let out = run_filters(
            vec![Box::new(WordDelimiterGraph::new(cfg(
                DEFAULT_FLAGS | CATENATE_WORDS
            )))],
            "wi-fi x",
        );
        let terms: Vec<_> = out.iter().map(|t| (t.0.as_str(), t.1)).collect();
        assert_eq!(terms, vec![("wifi", 1), ("wi", 0), ("fi", 1), ("x", 1)]);
    }

    #[test]
    fn type_table_errors() {
        assert_eq!(
            parse_types(&["a => NOPE".into()]).unwrap_err().reason,
            "Invalid Mapping Rule : [a => NOPE]. Illegal type."
        );
        assert_eq!(
            parse_types(&["ab => ALPHA".into()]).unwrap_err().reason,
            "Invalid Mapping Rule : [ab => ALPHA]. Only a single character is allowed."
        );
        let t = parse_types(&["+ => ALPHA".into(), "\\u2000 => DIGIT".into()]).unwrap();
        assert_eq!(t[b'+' as usize], ALPHA);
        assert_eq!(t[0x2000], DIGIT);
    }
}
