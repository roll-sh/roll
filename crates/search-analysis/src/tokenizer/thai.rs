//! `thai` tokenizer (Lucene's `ThaiTokenizer`).
//!
//! Lucene segments text in three layers, all reproduced here because each one decides token
//! boundaries somewhere:
//!
//! 1. **Chunks** (`SegmentingTokenizerBase`): the input is read into a 1024-unit UTF-16 buffer
//!    that is cut after its last line break; a line longer than that is cut hard every 1024
//!    units, even through a word or a surrogate pair.
//! 2. **Sentences**: each chunk is split by the JDK's legacy rule-based sentence iterator (root
//!    locale `SentenceBreakRules`). Words never cross a sentence end, which is visible in cases
//!    like `x?.5` (the sentence ends after `?.`, so `5` is a word rather than part of `.5`).
//!    [`sentence_next`] is that state machine, written out by hand: its `.*?` rules
//!    restart scanning when a terminator candidate fails, which is not a plain longest match.
//! 3. **Words**: the JDK's `DictionaryBasedBreakIterator` for Thai (`BreakIteratorRules_th`).
//!    Its rules are compiled by [`crate::dfa`] with three flags per state (accepting, lookahead
//!    point, lookahead end) to emulate the iterator's one lookahead rule (`ฯ` followed by
//!    anything but `ล`), and characters of the rules' `<ignore>` class (marks and format
//!    characters no rule names) are skipped without changing state. Runs of Thai letters are
//!    then divided with a word dictionary by the JDK's backtracking longest-match algorithm
//!    ([`ThaiTokenizer::divide`]), whose quirks (a stale trie state after a failed word that
//!    reached the end of the range, the "blessing" of the farthest partial parse) are kept.
//!
//! Segments that do not start with a letter or digit are dropped, as Lucene does.
//!
//! The dictionary is a trie ([`ThaiDictionary`]). The built-in word list is ICU's Thai
//! dictionary (Unicode License v3), which overlaps the JDK's own list but is not identical, so
//! Thai words outside the common part can segment differently from Elasticsearch. Operators who
//! need identical segmentation can install the JDK's `thai_dict` file with
//! [`set_thai_dictionary`] / [`ThaiDictionary::from_jdk`]; with it the output matches
//! Elasticsearch exactly. Everything outside Thai script is independent of the dictionary.
//!
//! The tokenizer keeps the text as UTF-16 (offsets and the JDK's index arithmetic are in UTF-16
//! units) in a buffer reused across documents; segmentation allocates nothing per token.

use std::sync::OnceLock;

use crate::dfa::{self, Dfa, Ranges, alt, opt, plus, seq, set, star};
use crate::error::AnalysisError;
use crate::stream::Tokenizer;
use crate::token::Tok;
use crate::unicode::{self, gc};

/// `CharacterIterator.DONE`: the end of the text, and also what a literal U+FFFF reads as, which
/// stops the JDK's scanning loops just like the end does.
const DONE: u32 = 0xFFFF;

/// `SegmentingTokenizerBase.BUFFERMAX`.
const BUFFER: usize = 1024;

/// The general category as the JDK's break iterator data has it. Its build tool
/// (`CharacterCategory`) reads UnicodeData.txt and, when a `<…, First>` range entry follows an
/// entry of the same category after a gap, extends the previous range over the gap: the
/// unassigned code points before several CJK extensions (and two before plane 16's private use
/// area) count as the neighbouring category (Unicode 17 data).
#[inline]
fn break_type(c: char) -> u8 {
    match c as u32 {
        0x2A6E0..=0x2A6FF
        | 0x2B81E..=0x2B81F
        | 0x2CEAE..=0x2CEAF
        | 0x2EBE1..=0x2EBEF
        | 0x2FA1E..=0x2FFFF
        | 0x3134B..=0x3134F => gc::OTHER_LETTER,
        0xFFFFE..=0xFFFFF => gc::PRIVATE_USE,
        _ => unicode::get_type(c),
    }
}

fn category_ranges(pred: impl Fn(u8) -> bool) -> Ranges {
    let mut v: Ranges = Vec::new();
    for cp in 0..=0x10FFFFu32 {
        let Some(c) = char::from_u32(cp) else {
            continue;
        };
        if pred(break_type(c)) {
            match v.last_mut() {
                Some(last) if last.1 + 1 == cp => last.1 = cp,
                _ => v.push((cp, cp)),
            }
        }
    }
    v
}

/// The Thai rules' `<dictionary>` class: the characters the dictionary divides.
#[inline]
fn is_dictionary_char(cp: u32) -> bool {
    matches!(cp, 0x0E01..=0x0E2E | 0x0E30..=0x0E3A | 0x0E40..=0x0E44 | 0x0E47..=0x0E4E)
}

#[inline]
fn is_mark_or_format(c: char) -> bool {
    !c.is_ascii()
        && matches!(
            break_type(c),
            gc::NON_SPACING_MARK | gc::ENCLOSING_MARK | gc::FORMAT
        )
}

/// The word iterator's ignore class: `[:Mn::Me::Cf:]` minus the dictionary characters and minus
/// every character another rule expression names (the JDK rule builder removes those).
#[inline]
fn is_word_ignored(c: char) -> bool {
    let cp = c as u32;
    is_mark_or_format(c) && !is_dictionary_char(cp) && !matches!(cp, 0x00AD | 0x3099 | 0x309A)
}

/// A JDK `char`/code point as a Rust `char` for classification. Lone surrogates (only produced
/// by a hard chunk cut) belong to no class of either rule set, like private-use characters.
#[inline]
fn as_char(cp: u32) -> char {
    char::from_u32(cp).unwrap_or('\u{E000}')
}

/// `RuleBasedBreakIterator.getCurrent`: the code point at `i` of `s` (a surrogate pair is read
/// whole), or [`DONE`] at the end.
#[inline]
fn cp_at(s: &[u16], i: usize) -> u32 {
    let Some(&c1) = s.get(i) else {
        return DONE;
    };
    if (0xD800..0xDC00).contains(&c1)
        && let Some(&c2) = s.get(i + 1)
        && (0xDC00..0xE000).contains(&c2)
    {
        return 0x10000 + ((c1 as u32 - 0xD800) << 10) + (c2 as u32 - 0xDC00);
    }
    c1 as u32
}

#[inline]
fn cp_len(s: &[u16], i: usize) -> usize {
    if cp_at(s, i) > 0xFFFF { 2 } else { 1 }
}

/// `getNextIndex`: the index after the current code point.
#[inline]
fn next_index(s: &[u16], i: usize) -> usize {
    (i + cp_len(s, i)).min(s.len())
}

/// `getNext`: advances past the current code point and reads the next one. On the last code
/// point it returns [`DONE`] *without* moving, which the dictionary algorithm's index
/// arithmetic depends on.
#[inline]
fn get_next(s: &[u16], i: &mut usize) -> u32 {
    if *i == s.len() {
        return DONE;
    }
    let j = *i + cp_len(s, *i);
    if j >= s.len() {
        return DONE;
    }
    *i = j;
    cp_at(s, j)
}

// ---------------------------------------------------------------------------------------------
// Dictionary

/// A word list for Thai segmentation, stored as a trie whose transitions follow the JDK's
/// `BreakDictionary`: from a node, a character of the dictionary's alphabet leads to a child
/// (or to the error state 0), and any other character leads to "end of word" (-1) if the node
/// ends a word.
pub struct ThaiDictionary {
    /// Column of U+0E00..U+0E7F (0 = not in the alphabet).
    thai_cols: [u16; 128],
    /// Columns of other characters, sorted by code point.
    other_cols: Vec<(u32, u16)>,
    /// Children of node `n`: `labels/targets[first[n]..first[n + 1]]`, sorted by label.
    first: Vec<u32>,
    labels: Vec<u16>,
    targets: Vec<u32>,
    words: Vec<bool>,
}

impl std::fmt::Debug for ThaiDictionary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThaiDictionary")
            .field("nodes", &self.words.len())
            .field("words", &self.words.iter().filter(|w| **w).count())
            .finish()
    }
}

/// The characters with a column in the JDK's Thai dictionary. The built-in word list uses a
/// subset of them; giving it the same alphabet keeps the dictionary-independent part of the
/// algorithm (which characters can continue a word at all) identical.
const JDK_ALPHABET: &[(u32, u32)] = &[(0x0E01, 0x0E3A), (0x0E40, 0x0E4C), (0x0E51, 0x0E51)];

/// ICU's Thai word list, front-coded (see `tools/gen_thai_dict.py`).
static BUILTIN_WORDS: &[u8] = include_bytes!("thai_words.bin");

impl ThaiDictionary {
    /// A dictionary of `words`. Its alphabet is every character the words use.
    pub fn from_words<I, S>(words: I) -> ThaiDictionary
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let words: Vec<Vec<char>> = words
            .into_iter()
            .map(|w| w.as_ref().chars().collect::<Vec<_>>())
            .filter(|w| !w.is_empty())
            .collect();
        let mut alphabet: Vec<u32> = words.iter().flatten().map(|&c| c as u32).collect();
        alphabet.sort_unstable();
        alphabet.dedup();
        Self::build(&alphabet, words.iter().map(|w| w.as_slice()))
    }

    /// Reads the JDK's binary dictionary format (`sun/text/resources/ext/thai_dict` in the
    /// `jdk.localedata` module), the dictionary Elasticsearch's `thai` tokenizer uses.
    pub fn from_jdk(data: &[u8]) -> Result<ThaiDictionary, AnalysisError> {
        let bad =
            |what: &str| AnalysisError::illegal(format!("invalid JDK Thai dictionary: {what}"));
        let mut r = BeReader { data, pos: 0 };
        let version = r.i32().ok_or_else(|| bad("truncated"))?;
        if version != 1 {
            return Err(bad(&format!("unsupported version {version}")));
        }
        let len = r.i32().ok_or_else(|| bad("truncated"))?;
        if len < 0 || r.pos + len as usize != data.len() {
            return Err(bad("wrong size"));
        }
        let index = r.shorts().ok_or_else(|| bad("truncated column index"))?;
        let values = r.bytes().ok_or_else(|| bad("truncated column values"))?;
        if index.len() != 512 {
            return Err(bad("column index"));
        }
        let num_cols = r.i32().ok_or_else(|| bad("truncated"))?;
        let _num_col_groups = r.i32().ok_or_else(|| bad("truncated"))?;
        let row_index = r.shorts().ok_or_else(|| bad("truncated row index"))?;
        let flags_index = r.shorts().ok_or_else(|| bad("truncated flags index"))?;
        let flags = r.ints().ok_or_else(|| bad("truncated flags"))?;
        let shifts = r.bytes().ok_or_else(|| bad("truncated shifts"))?;
        let table = r.shorts().ok_or_else(|| bad("truncated table"))?;
        let supplementary = r.ints().ok_or_else(|| bad("truncated supplementary map"))?;
        if r.pos != data.len() || num_cols <= 0 {
            return Err(bad("trailing data"));
        }
        let rows = row_index.len();
        if flags_index.len() != rows || shifts.len() != rows {
            return Err(bad("row tables"));
        }
        let num_cols = num_cols as usize;
        // Column of a BMP character (CompactByteArray with 128-character blocks).
        let col_of = |c: usize| -> Option<i64> {
            let base = (index[c >> 7] as u16) as usize;
            values.get(base + (c & 0x7F)).map(|&v| v as i8 as i64)
        };
        let next = |row: usize, col: usize| -> Option<i64> {
            let f = *flags_index.get(row)?;
            let populated = if f < 0 {
                col as i64 == -(f as i64)
            } else {
                let w = *flags.get(f as usize + (col >> 5))?;
                (w >> (col & 31)) & 1 != 0
            };
            if !populated {
                return Some(0);
            }
            let phys = (row_index[row] as u16) as usize * num_cols;
            let c = col as i64 + shifts[row] as i8 as i64;
            if c < 0 {
                return None;
            }
            table.get(phys + c as usize).map(|&v| v as i64)
        };
        let mut alphabet: Vec<(u32, usize)> = Vec::new();
        for c in 0..0x10000usize {
            match col_of(c) {
                Some(0) => {}
                Some(k) if k > 0 && (k as usize) < num_cols => {
                    alphabet.push((c as u32, k as usize))
                }
                _ => return Err(bad("column map")),
            }
        }
        // Supplementary characters: (start << 8 | column) entries; the JDK file maps none.
        for pair in supplementary.windows(2) {
            let (start, end) = ((pair[0] as u32) >> 8, (pair[1] as u32) >> 8);
            let col = (pair[0] & 0xFF) as usize;
            if col != 0 && col != 0xFF {
                if end <= start || end - start > 4096 || col >= num_cols {
                    return Err(bad("supplementary map"));
                }
                alphabet.extend((start..end).map(|c| (c, col)));
            }
        }
        // Expand the trie into its words (rows may be shared, so walk paths, bounded).
        let mut words: Vec<Vec<char>> = Vec::new();
        let mut total = 0usize;
        let mut stack: Vec<(usize, usize, char)> = vec![(0, 0, '\0')];
        let mut path: Vec<char> = Vec::new();
        while let Some((row, depth, c)) = stack.pop() {
            if depth > 0 {
                path.truncate(depth - 1);
                path.push(c);
            }
            if row >= rows {
                return Err(bad("row out of range"));
            }
            if depth > 0 && next(row, 0).ok_or_else(|| bad("table"))? == -1 {
                total += depth;
                if words.len() >= 4_000_000 || total > 64_000_000 {
                    return Err(bad("too many words"));
                }
                words.push(path.clone());
            }
            for &(cp, col) in alphabet.iter().rev() {
                match next(row, col).ok_or_else(|| bad("table"))? {
                    0 => {}
                    s if s > 0 => {
                        if depth >= 255 {
                            return Err(bad("word too long"));
                        }
                        let ch = char::from_u32(cp).ok_or_else(|| bad("column map"))?;
                        stack.push((s as usize, depth + 1, ch));
                    }
                    _ => return Err(bad("end-of-word transition on a word character")),
                }
            }
        }
        let chars: Vec<u32> = alphabet.iter().map(|&(c, _)| c).collect();
        Ok(Self::build(&chars, words.iter().map(|w| w.as_slice())))
    }

    fn build<'a>(alphabet: &[u32], words: impl Iterator<Item = &'a [char]>) -> ThaiDictionary {
        let mut thai_cols = [0u16; 128];
        let mut other_cols = Vec::new();
        for (i, &c) in alphabet.iter().enumerate() {
            let col = (i + 1) as u16;
            if (0x0E00..0x0E80).contains(&c) {
                thai_cols[(c - 0x0E00) as usize] = col;
            } else {
                other_cols.push((c, col));
            }
        }
        other_cols.sort_unstable();
        let col = |c: char| -> u16 {
            let cp = c as u32;
            if (0x0E00..0x0E80).contains(&cp) {
                thai_cols[(cp - 0x0E00) as usize]
            } else {
                other_cols
                    .binary_search_by_key(&cp, |&(c, _)| c)
                    .map_or(0, |i| other_cols[i].1)
            }
        };
        let mut children: Vec<Vec<(u16, u32)>> = vec![Vec::new()];
        let mut is_word = vec![false];
        for w in words {
            let mut node = 0usize;
            for &c in w {
                let l = col(c);
                debug_assert!(l != 0);
                node = match children[node].iter().find(|&&(x, _)| x == l) {
                    Some(&(_, n)) => n as usize,
                    None => {
                        let n = children.len();
                        children.push(Vec::new());
                        is_word.push(false);
                        children[node].push((l, n as u32));
                        n
                    }
                };
            }
            is_word[node] = true;
        }
        let mut first = Vec::with_capacity(children.len() + 1);
        let mut labels = Vec::new();
        let mut targets = Vec::new();
        for ch in &mut children {
            ch.sort_unstable();
            first.push(labels.len() as u32);
            for &(l, t) in ch.iter() {
                labels.push(l);
                targets.push(t);
            }
        }
        first.push(labels.len() as u32);
        ThaiDictionary {
            thai_cols,
            other_cols,
            first,
            labels,
            targets,
            words: is_word,
        }
    }

    fn builtin() -> ThaiDictionary {
        let mut words: Vec<Vec<char>> = Vec::with_capacity(27_000);
        let mut prev: Vec<char> = Vec::new();
        let mut i = 0;
        while i < BUILTIN_WORDS.len() {
            let shared = BUILTIN_WORDS[i] as usize;
            i += 1;
            prev.truncate(shared);
            while i < BUILTIN_WORDS.len() && BUILTIN_WORDS[i] != 0 {
                prev.push(char::from_u32(0x0E00 + BUILTIN_WORDS[i] as u32).unwrap_or('\u{0E00}'));
                i += 1;
            }
            i += 1;
            words.push(prev.clone());
        }
        let mut alphabet: Vec<u32> = JDK_ALPHABET.iter().flat_map(|&(lo, hi)| lo..=hi).collect();
        alphabet.extend(words.iter().flatten().map(|&c| c as u32));
        alphabet.sort_unstable();
        alphabet.dedup();
        Self::build(&alphabet, words.iter().map(|w| w.as_slice()))
    }

    /// Whether `state` ends a word (`getNextState(state, 0) == -1`).
    #[inline]
    fn ends_word(&self, state: i32) -> bool {
        state >= 0 && self.words.get(state as usize).copied().unwrap_or(false)
    }

    /// `getNextStateFromCharacter`: a child (> 0), the error state 0, or -1 ("end of word").
    #[inline]
    fn next(&self, state: i32, cp: u32) -> i32 {
        let col = if (0x0E00..0x0E80).contains(&cp) {
            self.thai_cols[(cp - 0x0E00) as usize]
        } else if cp < 0x80 || self.other_cols.is_empty() {
            0
        } else {
            self.other_cols
                .binary_search_by_key(&cp, |&(c, _)| c)
                .map_or(0, |i| self.other_cols[i].1)
        };
        if state < 0 {
            return 0;
        }
        if col == 0 {
            return if self.ends_word(state) { -1 } else { 0 };
        }
        let s = state as usize;
        let (lo, hi) = (self.first[s] as usize, self.first[s + 1] as usize);
        match self.labels[lo..hi].binary_search(&col) {
            Ok(k) => self.targets[lo + k] as i32,
            Err(_) => 0,
        }
    }
}

struct BeReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl BeReader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    fn i32(&mut self) -> Option<i32> {
        self.take(4)
            .map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    /// A length-prefixed array; the length is checked against the bytes present before
    /// anything is allocated.
    fn count(&mut self, width: usize) -> Option<usize> {
        let n = self.i32()?;
        if n < 0 || (n as usize).checked_mul(width)? > self.data.len() - self.pos {
            return None;
        }
        Some(n as usize)
    }
    fn shorts(&mut self) -> Option<Vec<i16>> {
        let n = self.count(2)?;
        let b = self.take(n * 2)?;
        Some(
            b.chunks_exact(2)
                .map(|c| i16::from_be_bytes([c[0], c[1]]))
                .collect(),
        )
    }
    fn ints(&mut self) -> Option<Vec<i32>> {
        let n = self.count(4)?;
        let b = self.take(n * 4)?;
        Some(
            b.chunks_exact(4)
                .map(|c| i32::from_be_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        )
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.count(1)?;
        self.take(n).map(<[u8]>::to_vec)
    }
}

static DICTIONARY: OnceLock<ThaiDictionary> = OnceLock::new();

/// Installs the dictionary the `thai` tokenizer divides Thai text with, for the whole process.
/// It must be called before the first Thai text is tokenized; afterwards (or on a second call)
/// it fails and the dictionary in use stays.
pub fn set_thai_dictionary(dictionary: ThaiDictionary) -> Result<(), AnalysisError> {
    DICTIONARY
        .set(dictionary)
        .map_err(|_| AnalysisError::illegal("the Thai dictionary is already in use"))
}

fn dictionary() -> &'static ThaiDictionary {
    DICTIONARY.get_or_init(ThaiDictionary::builtin)
}

// ---------------------------------------------------------------------------------------------
// Word rules

/// Lexical states of the word DFA used as per-state flags.
const ACCEPT: usize = 0;
const LOOKAHEAD: usize = 1;
const LOOKAHEAD_END: usize = 2;

fn word_dfa() -> &'static Dfa {
    static D: OnceLock<Dfa> = OnceLock::new();
    D.get_or_init(|| {
        let r = |v: &[(u32, u32)]| dfa::ranges(v.to_vec());
        let dict = r(&[
            (0x0E01, 0x0E2E),
            (0x0E30, 0x0E3A),
            (0x0E40, 0x0E44),
            (0x0E47, 0x0E4E),
        ]);
        let marks = category_ranges(|t| {
            matches!(t, gc::NON_SPACING_MARK | gc::ENCLOSING_MARK | gc::FORMAT)
        });
        // `<ignore>` as the rule text spells it (the lookahead classes subtract this, not the
        // runtime ignore class).
        let ignore_text = dfa::difference(&marks, &dict);
        let paiyannoi = r(&[(0x0E2F, 0x0E2F)]);
        let maiyamok = r(&[(0x0E46, 0x0E46)]);
        let lo_ling = r(&[(0x0E25, 0x0E25)]);
        let danda = r(&[(0x0964, 0x0965)]);
        let kanji = r(&[(0x3005, 0x3005), (0x4E00, 0x9FA5), (0xF900, 0xFA2D)]);
        let kata = r(&[(0x30A1, 0x30FA)]);
        let hira = r(&[(0x3041, 0x3094)]);
        let cjk_diacrit = r(&[(0x3099, 0x309C)]);
        let letters = category_ranges(|t| {
            matches!(
                t,
                gc::UPPERCASE_LETTER
                    | gc::LOWERCASE_LETTER
                    | gc::TITLECASE_LETTER
                    | gc::MODIFIER_LETTER
                    | gc::OTHER_LETTER
                    | gc::COMBINING_SPACING_MARK
            )
        });
        let let_ = dfa::difference(
            &letters,
            &dfa::union(&[&kanji, &kata, &hira, &cjk_diacrit, &dict]),
        );
        let dgt = category_ranges(|t| {
            matches!(
                t,
                gc::DECIMAL_DIGIT_NUMBER | gc::LETTER_NUMBER | gc::OTHER_NUMBER
            )
        });
        let pd = category_ranges(|t| t == gc::DASH_PUNCTUATION);
        let mid_word = dfa::union(&[&pd, &dfa::chars("\u{00AD}\u{2027}\"'.")]);
        let mid_num = dfa::chars("\"',\u{066B}.");
        let sc = category_ranges(|t| t == gc::CURRENCY_SYMBOL);
        let pre_num = dfa::difference(
            &dfa::union(&[&sc, &dfa::chars("#.")]),
            &dfa::chars("\u{00A2}"),
        );
        let post_num = dfa::chars("%&\u{00A2}\u{066A}\u{2030}\u{2031}");
        let ls = dfa::chars("\n\u{000C}\u{2028}\u{2029}");
        let zs = category_ranges(|t| t == gc::SPACE_SEPARATOR);
        let ws = dfa::union(&[&zs, &dfa::chars("\t")]);
        let any = vec![(0u32, 0x10FFFFu32)];
        let not_lo_ling = dfa::negate(&dfa::union(&[&lo_ling, &ignore_text]));
        let not_paiyannoi = dfa::negate(&dfa::union(&[&paiyannoi, &ignore_text]));
        let word = || {
            seq(vec![
                plus(set(&let_)),
                star(seq(vec![set(&mid_word), plus(set(&let_))])),
                opt(set(&danda)),
            ])
        };
        let number = || {
            seq(vec![
                plus(set(&dgt)),
                star(seq(vec![set(&mid_num), plus(set(&dgt))])),
            ])
        };
        let tail = || {
            seq(vec![
                star(seq(vec![number(), word()])),
                opt(seq(vec![number(), opt(set(&post_num))])),
            ])
        };
        let thai_run = || plus(set(&dict));
        let rules = vec![
            set(&any),
            seq(vec![opt(word()), tail()]),
            seq(vec![set(&pre_num), tail()]),
            seq(vec![
                thai_run(),
                opt(seq(vec![opt(set(&paiyannoi)), set(&maiyamok)])),
            ]),
            // The lookahead rule `<dictionary>+<paiyannoi>/(...)`: its lookahead point ...
            seq(vec![thai_run(), set(&paiyannoi)]),
            // ... and its end.
            seq(vec![
                thai_run(),
                set(&paiyannoi),
                alt(vec![
                    set(&not_lo_ling),
                    seq(vec![set(&lo_ling), set(&not_paiyannoi)]),
                ]),
            ]),
            seq(vec![set(&paiyannoi), set(&lo_ling), set(&paiyannoi)]),
            seq(vec![star(set(&ws)), opt(dfa::lit("\r")), opt(set(&ls))]),
            star(set(&dfa::union(&[&kata, &cjk_diacrit]))),
            star(set(&dfa::union(&[&hira, &cjk_diacrit]))),
            star(set(&kanji)),
        ];
        let a = 1 << ACCEPT;
        let active = [
            a,
            a,
            a,
            a,
            1 << LOOKAHEAD,
            1 << LOOKAHEAD_END,
            a,
            a,
            a,
            a,
            a,
        ];
        Dfa::with_states(&rules, &active)
    })
}

/// The rule-based part of the word iterator (`RuleBasedBreakIterator.handleNext`) from `pos`
/// (< `s.len()`): the tentative break and the number of dictionary characters looked at (which
/// includes the character the state machine stopped on).
fn rule_next(d: &Dfa, s: &[u16], pos: usize) -> (usize, u32) {
    let mut i = pos;
    let mut result = next_index(s, i);
    let mut lookahead = 0usize;
    let mut state = dfa::START;
    let mut c = cp_at(s, i);
    let mut dict_chars = 0u32;
    while c != DONE && state != dfa::DEAD {
        let ch = as_char(c);
        if !is_word_ignored(ch) {
            if is_dictionary_char(c) {
                dict_chars += 1;
            }
            state = d.step(state, ch);
        }
        let la_end = d.accepts_in(LOOKAHEAD_END, state).is_some();
        if la_end || d.accepts_in(LOOKAHEAD, state).is_some() {
            if la_end || d.accepts_in(ACCEPT, state).is_some() {
                result = lookahead;
            } else {
                lookahead = next_index(s, i);
            }
        } else if d.accepts_in(ACCEPT, state).is_some() {
            result = next_index(s, i);
        }
        c = get_next(s, &mut i);
    }
    if c == DONE && lookahead == s.len() {
        result = lookahead;
    }
    (result, dict_chars)
}

// ---------------------------------------------------------------------------------------------
// Sentence rules

/// States of the root sentence iterator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sent {
    /// Scanning (`.*?`); every position is a possible end (rule `.*?{ }`).
    Scan,
    /// After a period, in `[<period><end>]*`: the lookahead point of the second period rule.
    Period,
    /// A quote right after a period: both still in `[<period><end>]*` and already in the second
    /// period rule's lookahead (quotes are opening and closing punctuation).
    PeriodQuote,
    /// After a period and spaces: the lookahead point of both period rules.
    PeriodSpace,
    /// In the lookahead `[<start-punctuation><sent-start>]+`, waiting for a letter.
    Lookahead,
    /// The lookahead matched: the sentence ends at the lookahead point.
    LookaheadEnd,
    /// After `! ? 。 ！ ？` and `[<term><period><end>]*`.
    Term,
    /// After a terminator and spaces.
    TermSpace,
    /// After a danda and spaces.
    Danda,
    /// After U+2029.
    Paragraph,
    Dead,
}

#[derive(Clone, Copy)]
struct SentClass {
    letter: bool,
    lower: bool,
    space: bool,
    start_punct: bool,
    end_punct: bool,
    sent_start: bool,
    term: bool,
    period: bool,
    danda: bool,
    paragraph: bool,
}

fn sentence_class(c: char) -> SentClass {
    let cp = c as u32;
    let t = break_type(c);
    let letter = matches!(t, 1..=5);
    let space = matches!(cp, 0x09 | 0x0D | 0x0C | 0x0A | 0x2028) || t == gc::SPACE_SEPARATOR;
    let quote = cp == '"' as u32 || cp == '\'' as u32;
    let start_punct = quote || matches!(t, gc::START_PUNCTUATION | gc::INITIAL_QUOTE_PUNCTUATION);
    let end_punct = quote || matches!(t, gc::END_PUNCTUATION | gc::FINAL_QUOTE_PUNCTUATION);
    let digit = matches!(
        t,
        gc::DECIMAL_DIGIT_NUMBER | gc::LETTER_NUMBER | gc::OTHER_NUMBER
    );
    let term = matches!(cp, 0x21 | 0x3F | 0x3002 | 0xFF01 | 0xFF1F);
    let period = cp == 0x2E || cp == 0xFF0E;
    let paragraph = cp == 0x2029;
    let sent_start = !(letter
        || space
        || start_punct
        || end_punct
        || digit
        || term
        || period
        || cp == ',' as u32
        || paragraph
        || is_mark_or_format(c));
    SentClass {
        letter,
        lower: t == gc::LOWERCASE_LETTER,
        space,
        start_punct,
        end_punct,
        sent_start,
        term,
        period,
        danda: cp == 0x0964 || cp == 0x0965,
        paragraph,
    }
}

impl Sent {
    fn flags(self) -> (bool, bool) {
        // (lookahead, accepting)
        match self {
            Sent::Scan | Sent::Term | Sent::TermSpace | Sent::Danda | Sent::Paragraph => {
                (false, true)
            }
            Sent::Period | Sent::PeriodQuote | Sent::PeriodSpace => (true, false),
            Sent::LookaheadEnd => (true, true),
            Sent::Lookahead | Sent::Dead => (false, false),
        }
    }

    fn scan(k: SentClass) -> Sent {
        if k.paragraph {
            Sent::Paragraph
        } else if k.danda {
            Sent::Danda
        } else if k.period {
            Sent::Period
        } else if k.term {
            Sent::Term
        } else {
            Sent::Scan
        }
    }

    fn step(self, k: SentClass) -> Sent {
        // The states between the scanning loop and a rule's end fall back to the loop's
        // transition when they have none of their own (the rule builder's "backfilling").
        let own = match self {
            Sent::Scan => return Sent::scan(k),
            Sent::Period | Sent::PeriodQuote => {
                let in_lookahead = self == Sent::PeriodQuote;
                if k.period {
                    Some(Sent::Period)
                } else if k.start_punct && k.end_punct {
                    Some(Sent::PeriodQuote)
                } else if k.end_punct {
                    Some(Sent::Period)
                } else if k.space {
                    Some(Sent::PeriodSpace)
                } else if k.start_punct || k.sent_start {
                    Some(Sent::Lookahead)
                } else if in_lookahead && k.letter {
                    Some(Sent::LookaheadEnd)
                } else {
                    None
                }
            }
            Sent::PeriodSpace => {
                if k.space {
                    Some(Sent::PeriodSpace)
                } else if k.letter && !k.lower {
                    Some(Sent::LookaheadEnd)
                } else if k.start_punct || k.sent_start {
                    Some(Sent::Lookahead)
                } else {
                    None
                }
            }
            Sent::Lookahead => {
                if k.start_punct || k.sent_start {
                    Some(Sent::Lookahead)
                } else if k.letter {
                    Some(Sent::LookaheadEnd)
                } else {
                    None
                }
            }
            Sent::Term => {
                return if k.term || k.period || k.end_punct {
                    Sent::Term
                } else if k.space {
                    Sent::TermSpace
                } else if k.paragraph {
                    Sent::Paragraph
                } else {
                    Sent::Dead
                };
            }
            Sent::TermSpace => {
                return if k.space {
                    Sent::TermSpace
                } else if k.paragraph {
                    Sent::Paragraph
                } else {
                    Sent::Dead
                };
            }
            Sent::Danda => return if k.space { Sent::Danda } else { Sent::Dead },
            Sent::Paragraph | Sent::LookaheadEnd | Sent::Dead => return Sent::Dead,
        };
        own.unwrap_or_else(|| Sent::scan(k))
    }
}

/// `next()` of the root sentence iterator from `pos` (< `s.len()`).
fn sentence_next(s: &[u16], pos: usize) -> usize {
    let mut i = pos;
    let mut result = next_index(s, i);
    let mut lookahead = 0usize;
    let mut state = Sent::Scan;
    let mut c = cp_at(s, i);
    while c != DONE && state != Sent::Dead {
        let ch = as_char(c);
        if !is_mark_or_format(ch) {
            state = state.step(sentence_class(ch));
        }
        let (la, end) = state.flags();
        if la {
            if end {
                result = lookahead;
            } else {
                lookahead = next_index(s, i);
            }
        } else if end {
            result = next_index(s, i);
        }
        c = get_next(s, &mut i);
    }
    if c == DONE && lookahead == s.len() {
        result = lookahead;
    }
    result
}

// ---------------------------------------------------------------------------------------------
// Tokenizer

/// `ThaiTokenizer`.
pub(crate) struct ThaiTokenizer {
    rules: &'static Dfa,
    dict: &'static ThaiDictionary,
    /// The whole text in UTF-16.
    buf: Vec<u16>,
    /// `SegmentingTokenizerBase`: the chunk is `buf[offset..offset + length]`, of which the
    /// first `usable` units are segmented.
    offset: usize,
    length: usize,
    usable: usize,
    /// The sentence iterator's position within the chunk.
    sentence_pos: usize,
    /// The current sentence, absolute.
    sentence: (usize, usize),
    /// The word iterator's position within the sentence.
    word_pos: usize,
    /// `cachedBreakPositions` (None when empty) and `positionInCache`.
    cache: Vec<usize>,
    has_cache: bool,
    cache_pos: usize,
    // Scratch stacks of the dictionary algorithm.
    current: Vec<usize>,
    possible: Vec<usize>,
    wrong: Vec<usize>,
    best: Vec<usize>,
}

impl ThaiTokenizer {
    pub fn new() -> Self {
        ThaiTokenizer {
            rules: word_dfa(),
            dict: dictionary(),
            buf: Vec::new(),
            offset: 0,
            length: 0,
            usable: 0,
            sentence_pos: 0,
            sentence: (0, 0),
            word_pos: 0,
            cache: Vec::new(),
            has_cache: false,
            cache_pos: 0,
            current: Vec::new(),
            possible: Vec::new(),
            wrong: Vec::new(),
            best: Vec::new(),
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
            chunk
                .iter()
                .rposition(|&u| is_safe_end(u))
                .map_or(self.length, |i| i + 1)
        };
        self.sentence_pos = 0;
    }

    /// `incrementSentence`.
    fn increment_sentence(&mut self, t: &mut Tok) -> bool {
        if self.length == 0 {
            return false;
        }
        loop {
            let start = self.sentence_pos;
            if start >= self.usable {
                return false;
            }
            let chunk = &self.buf[self.offset..self.offset + self.usable];
            let end = sentence_next(chunk, start);
            self.sentence_pos = end;
            self.sentence = (self.offset + start, self.offset + end);
            self.word_pos = 0;
            self.has_cache = false;
            self.cache_pos = 0;
            if self.increment_word(t) {
                return true;
            }
        }
    }

    /// `DictionaryBasedBreakIterator.next()` within the current sentence.
    fn word_next(&mut self) -> Option<usize> {
        let (s0, s1) = self.sentence;
        if !self.has_cache || self.cache_pos + 1 == self.cache.len() {
            let start = self.word_pos;
            if start >= s1 - s0 {
                self.has_cache = false;
                return None;
            }
            let (result, dict_chars) = rule_next(self.rules, &self.buf[s0..s1], start);
            if dict_chars > 1 && result - start > 1 {
                self.divide(start, result);
            } else {
                self.has_cache = false;
                self.word_pos = result;
                return Some(result);
            }
        }
        self.cache_pos += 1;
        self.word_pos = self.cache[self.cache_pos];
        Some(self.word_pos)
    }

    /// `divideUpDictionaryRange`: breaks `[start, end)` of the sentence with the dictionary.
    fn divide(&mut self, start: usize, end: usize) {
        let (s0, s1) = self.sentence;
        let s = &self.buf[s0..s1];
        let dict = self.dict;
        let mut i = start;
        let mut c = cp_at(s, i);
        while !is_dictionary_char(c) && i < s.len() {
            if get_next(s, &mut i) == DONE {
                break;
            }
            c = cp_at(s, i);
        }
        let current = &mut self.current;
        let possible = &mut self.possible;
        let wrong = &mut self.wrong;
        let best = &mut self.best;
        current.clear();
        possible.clear();
        wrong.clear();
        best.clear();
        // `bestBreakPositions` is null until the first partial parse is recorded; once it is
        // adopted as `currentBreakPositions` the two are the same object until the next copy.
        let mut has_best = false;
        let mut aliased = false;
        let mut state: i32 = 0;
        let mut farthest = i;
        // The JDK loop always terminates; this bound only guards against a malformed
        // installed dictionary.
        let mut budget = 64 * (end - start + 16) * (end - start + 16);
        loop {
            budget = budget.saturating_sub(1);
            if budget == 0 {
                break;
            }
            if dict.ends_word(state) {
                possible.push(i);
            }
            state = dict.next(state, c);
            if state == -1 {
                current.push(i);
                break;
            } else if state == 0 || i >= end {
                if i > farthest {
                    farthest = i;
                    best.clear();
                    best.extend_from_slice(current);
                    has_best = true;
                    aliased = false;
                }
                while let Some(&p) = possible.last()
                    && wrong.contains(&p)
                {
                    possible.pop();
                }
                if possible.is_empty() {
                    if has_best {
                        if !aliased {
                            current.clear();
                            current.extend_from_slice(best);
                            aliased = true;
                        }
                        if farthest < end {
                            i = farthest + 1;
                        } else {
                            break;
                        }
                    } else {
                        if current.last() != Some(&i) && i != start {
                            current.push(i);
                        }
                        get_next(s, &mut i);
                        current.push(i);
                    }
                } else {
                    let p = possible.pop().unwrap_or(i);
                    while let Some(&top) = current.last()
                        && p < top
                    {
                        current.pop();
                        wrong.push(top);
                    }
                    current.push(p);
                    i = p;
                }
                c = cp_at(s, i);
                if i >= end {
                    break;
                }
            } else {
                c = get_next(s, &mut i);
            }
        }
        current.pop();
        current.push(end);
        self.cache.clear();
        self.cache.push(start);
        self.cache.extend_from_slice(current);
        self.has_cache = true;
        self.cache_pos = 0;
    }

    /// `ThaiTokenizer.incrementWord`.
    fn increment_word(&mut self, t: &mut Tok) -> bool {
        let (s0, s1) = self.sentence;
        let mut start = self.word_pos;
        let mut end = self.word_next();
        while let Some(e) = end {
            let first = if s0 + start < s1 {
                cp_at(&self.buf[..s1], s0 + start)
            } else {
                DONE
            };
            if unicode::is_letter_or_digit(as_char(first)) {
                break;
            }
            start = e;
            end = self.word_next();
        }
        let Some(end) = end else {
            return false;
        };
        let (a, b) = (s0 + start, (s0 + end).max(s0 + start));
        t.term.extend(
            char::decode_utf16(self.buf[a..b].iter().copied()).map(|r| r.unwrap_or('\u{FFFD}')),
        );
        t.start = a as u32;
        t.end = b as u32;
        true
    }
}

/// `SegmentingTokenizerBase.isSafeEnd`: only line breaks end a chunk (Lucene 10.5's
/// `ThaiTokenizer` does not add whitespace), so a line of more than 1024 units is cut hard.
#[inline]
fn is_safe_end(u: u16) -> bool {
    matches!(u, 0x0D | 0x0A | 0x85 | 0x2028 | 0x2029)
}

impl Tokenizer for ThaiTokenizer {
    fn reset(&mut self, text: &str) {
        self.buf.clear();
        self.buf.extend(text.encode_utf16());
        self.offset = 0;
        self.length = 0;
        self.usable = 0;
        self.sentence_pos = 0;
        self.sentence = (0, 0);
        self.word_pos = 0;
        self.has_cache = false;
        self.cache_pos = 0;
    }

    /// `SegmentingTokenizerBase.end`: the end of the chunk read so far, which is the end of the
    /// text once the stream is exhausted (a `limit` filter can end it earlier).
    fn end(&mut self, _text: &str, t: &mut Tok) {
        let end = (self.offset + self.length) as u32;
        t.start = end;
        t.end = end;
    }

    fn next(&mut self, _text: &str, t: &mut Tok) -> bool {
        if self.length == 0 || !self.increment_word(t) {
            while !self.increment_sentence(t) {
                self.refill();
                if self.length == 0 {
                    return false;
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str) -> Vec<(String, u32, u32)> {
        let mut tk = ThaiTokenizer::new();
        tk.reset(text);
        let mut out = Vec::new();
        let mut t = Tok::default();
        loop {
            t.clear();
            if !tk.next(text, &mut t) {
                break;
            }
            out.push((t.term.clone(), t.start, t.end));
        }
        out
    }

    fn terms(text: &str) -> Vec<String> {
        tokens(text).into_iter().map(|t| t.0).collect()
    }

    #[test]
    fn jdk_word_rules() {
        assert_eq!(
            terms("brown-fox I.B.M. $12 1,000.50 v1.2.3 x86_64 5¢ ¢5 it’s"),
            vec![
                "brown-fox",
                "I.B.M",
                "1,000.50",
                "v1.2.3",
                "x86",
                "64",
                "5¢",
                "5",
                "it",
                "s"
            ]
        );
        assert_eq!(
            terms("日本語のテキスト 我是中国人"),
            vec!["日本語", "の", "テキスト", "我是中国人"]
        );
        assert_eq!(
            terms("e\u{301}x 1\u{FE0F}\u{20E3}"),
            vec!["e\u{301}x", "1\u{FE0F}\u{20E3}"]
        );
        // Ignored characters are transparent from the first character on, so a segment that
        // starts with one does not start with a letter and is dropped.
        assert_eq!(terms("\u{200D}ab cd"), vec!["cd"]);
        // The prolonged sound mark is a letter in the Thai rules; U+309A is not ignored.
        assert_eq!(terms("ー7ｶ3.14"), vec!["ー7ｶ3.14"]);
        assert_eq!(terms("Ｆ\u{309A}۵\u{FE0F}5"), vec!["Ｆ", "۵\u{FE0F}5"]);
        // The soft hyphen is a mid-word character, not an ignored one.
        assert_eq!(terms("6\u{AD}๒"), vec!["6", "๒"]);
        // A literal U+FFFF stops the scan like the end of the text.
        assert_eq!(terms("a\u{FFFF}b c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn lookahead_rule() {
        // `<dictionary>+ฯ` ends before a character other than ล, and wins over `ฯๆ`.
        assert_eq!(terms("กฯๆ"), vec!["กฯ", "ๆ"]);
        assert_eq!(terms("กฯๆa"), vec!["กฯ", "ๆa"]);
        assert_eq!(terms("กฯx"), vec!["กฯ", "x"]);
        assert_eq!(terms("กฯ"), vec!["กฯ"]);
        assert_eq!(terms("กฯล"), vec!["ก", "ฯ", "ล"]);
        assert_eq!(terms("กฯลฯ"), vec!["ก", "ฯลฯ"]);
        assert_eq!(terms("กฯ\u{AD}"), vec!["ก", "ฯ"]);
        assert_eq!(terms("กฯ\u{301}a"), vec!["กฯ\u{301}", "a"]);
        assert_eq!(terms("กๆ"), vec!["กๆ"]);
    }

    #[test]
    fn sentences_bound_words() {
        // `?.` ends a sentence, so `.5` is not one pre-number segment.
        assert_eq!(terms("x?.5"), vec!["x", "5"]);
        assert_eq!(terms("ab?.5c"), vec!["ab", "5c"]);
        // A spacing mark after a period starts a sentence when a letter follows.
        assert_eq!(terms("x.\u{0903}b"), vec!["x"]);
        assert_eq!(terms("x. .5"), vec!["x"]);
        assert_eq!(terms("a\u{2029}.5"), vec!["a"]);
    }

    #[test]
    fn chunks_of_1024_units() {
        let long = "a".repeat(1500);
        let t = tokens(&long);
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].1, t[0].2, t[1].1, t[1].2), (0, 1024, 1024, 1500));
        // Spaces do not end a chunk, line breaks do.
        let text = format!("{} {} d", "a".repeat(1021), "bcd");
        let t = terms(&text);
        assert_eq!(&t[1..], ["bc", "d", "d"]);
        let text = format!("{}\n{}", "b".repeat(1000), "c".repeat(100));
        assert_eq!(tokens(&text).len(), 2);
        // A hard cut through a surrogate pair leaves lone halves, which read as U+FFFD.
        let text = format!("{}😀", "d".repeat(1023));
        let t = tokens(&text);
        assert_eq!(t[0].2, 1023);
    }

    fn dict(words: &[&str]) -> ThaiDictionary {
        ThaiDictionary::from_words(words.iter())
    }

    #[test]
    fn dictionary_division() {
        let d: &'static ThaiDictionary = Box::leak(Box::new(dict(&[
            "ภาษา",
            "ไทย",
            "ง่าย",
            "นิด",
            "เดียว",
            "นิดเดียว",
        ])));
        let mut tk = ThaiTokenizer::new();
        tk.dict = d;
        let text = "ภาษาไทยง่ายนิดเดียว";
        tk.reset(text);
        let mut out = Vec::new();
        let mut t = Tok::default();
        loop {
            t.clear();
            if !tk.next(text, &mut t) {
                break;
            }
            out.push(t.term.clone());
        }
        // Longest match first, backtracking only on failure.
        assert_eq!(out, vec!["ภาษา", "ไทย", "ง่าย", "นิดเดียว"]);
    }

    #[test]
    fn builtin_dictionary_loads() {
        let d = dictionary();
        let mut s = 0;
        for c in "ภาษา".chars() {
            s = d.next(s, c as u32);
            assert!(s > 0);
        }
        assert!(d.ends_word(s));
        assert_eq!(d.next(s, 'x' as u32), -1);
        assert_eq!(d.next(0, 'x' as u32), 0);
    }

    #[test]
    fn jdk_format_rejects_garbage() {
        assert!(ThaiDictionary::from_jdk(&[]).is_err());
        assert!(ThaiDictionary::from_jdk(&[0, 0, 0, 1, 0x7F, 0xFF, 0xFF, 0xFF]).is_err());
        let mut v = vec![0, 0, 0, 1, 0, 0, 0, 8];
        v.extend_from_slice(&[0x7F, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0]);
        assert!(ThaiDictionary::from_jdk(&v).is_err());
    }
}
