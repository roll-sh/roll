//! Tokenizers: `keyword`, the character-class tokenizers (`whitespace`, `letter`, `lowercase`,
//! `char_group`), `ngram`/`edge_ngram`, `path_hierarchy`, `pattern`, `simple_pattern`,
//! `simple_pattern_split`, plus the grammar-based ones in submodules (`standard`, `classic`,
//! `uax_url_email`, `thai`).
//!
//! Each is a port of the Lucene tokenizer Elasticsearch instantiates, reproducing its edge cases
//! (token length limits counted in UTF-16 units, where a cut token resumes, final offsets) rather
//! than a fresh design: those details decide which terms end up in the index. All of them scan the
//! text in place, keep byte and UTF-16 positions side by side, and write only into the reused term
//! buffer.

pub(crate) mod classic;
pub(crate) mod jflex;
pub(crate) mod standard;
pub(crate) mod thai;
pub(crate) mod tlds;
pub(crate) mod uax_url_email;

use std::sync::Arc;

use crate::error::{AnalysisError, Result};
use crate::jregex::JavaRegex;
use crate::lregexp::RunAutomaton;
use crate::stream::Tokenizer;
use crate::token::Tok;
use crate::unicode::{self, gc};

pub(crate) const MAX_TOKEN_LENGTH_LIMIT: i32 = 1024 * 1024;

/// `KeywordTokenizer`: the whole input as one token (an empty token for empty input).
pub(crate) struct KeywordTokenizer {
    done: bool,
}

impl KeywordTokenizer {
    pub fn new(_buffer_size: usize) -> KeywordTokenizer {
        KeywordTokenizer { done: false }
    }
}

impl Tokenizer for KeywordTokenizer {
    fn reset(&mut self, _text: &str) {
        self.done = false;
    }

    fn next(&mut self, text: &str, t: &mut Tok) -> bool {
        if self.done {
            return false;
        }
        self.done = true;
        t.term.push_str(text);
        t.start = 0;
        t.end = unicode::utf16_len(text) as u32;
        true
    }
}

/// Which characters belong to tokens, for [`CharTokenizer`].
pub(crate) trait TokenChar: Send + Sync + 'static {
    fn is_token_char(&self, c: char) -> bool;
}

pub(crate) struct Whitespace;
impl TokenChar for Whitespace {
    #[inline]
    fn is_token_char(&self, c: char) -> bool {
        !unicode::is_whitespace(c)
    }
}

pub(crate) struct Letter;
impl TokenChar for Letter {
    #[inline]
    fn is_token_char(&self, c: char) -> bool {
        unicode::is_letter(c)
    }
}

/// `CharTokenizer`: maximal runs of token characters, cut every `max_len` UTF-16 units (a cut
/// token resumes at the next character). With `lower`, terms are lowercased as they are read
/// (`XLowerCaseTokenizer`, which counts the length of the lowercased text).
pub(crate) struct CharTokenizer<P> {
    pred: Arc<P>,
    max_len: usize,
    lower: bool,
    pos: usize,
    upos: usize,
    /// End of the last token: Lucene's `CharTokenizer` reports it as the final offset when the
    /// stream is ended before it is exhausted (a `limit` filter stopped reading).
    last_end: usize,
    exhausted: bool,
}

impl<P: TokenChar> CharTokenizer<P> {
    pub fn new(pred: Arc<P>, max_len: usize, lower: bool) -> Self {
        CharTokenizer {
            pred,
            max_len,
            lower,
            pos: 0,
            upos: 0,
            last_end: 0,
            exhausted: false,
        }
    }
}

impl<P: TokenChar> Tokenizer for CharTokenizer<P> {
    fn reset(&mut self, _text: &str) {
        self.pos = 0;
        self.upos = 0;
        self.last_end = 0;
        self.exhausted = false;
    }

    fn end(&mut self, text: &str, t: &mut Tok) {
        let end = if self.exhausted {
            unicode::utf16_len(text)
        } else {
            self.last_end
        } as u32;
        t.start = end;
        t.end = end;
    }

    #[inline]
    fn next(&mut self, text: &str, t: &mut Tok) -> bool {
        let bytes = text.as_bytes();
        let mut length = 0usize; // UTF-16 units in the term
        let mut start = 0usize;
        let mut end = 0usize;
        while self.pos < bytes.len() {
            let b = bytes[self.pos];
            let (c, blen) = if b < 0x80 {
                (b as char, 1)
            } else {
                let c = text[self.pos..].chars().next().unwrap_or('\u{FFFD}');
                (c, c.len_utf8())
            };
            let ulen = c.len_utf16();
            if self.pred.is_token_char(c) {
                if length == 0 {
                    start = self.upos;
                    end = start;
                }
                self.pos += blen;
                self.upos += ulen;
                end += ulen;
                if self.lower {
                    let l = unicode::to_lower(c);
                    t.term.push(l);
                    length += l.len_utf16();
                } else {
                    t.term.push(c);
                    length += ulen;
                }
                if length >= self.max_len {
                    break;
                }
            } else {
                self.pos += blen;
                self.upos += ulen;
                if length > 0 {
                    break;
                }
            }
        }
        if length == 0 {
            self.exhausted = true;
            return false;
        }
        t.start = start as u32;
        t.end = end as u32;
        self.last_end = end;
        true
    }
}

/// `max_token_length` validation shared by the char tokenizers.
pub(crate) fn check_max_token_len(len: i32) -> Result<usize> {
    if len > MAX_TOKEN_LENGTH_LIMIT || len <= 0 {
        return Err(AnalysisError::illegal(format!(
            "maxTokenLen must be greater than 0 and less than {MAX_TOKEN_LENGTH_LIMIT} passed: {len}"
        )));
    }
    Ok(len as usize)
}

/// `char_group`: splits on the configured characters and character classes.
#[derive(Debug)]
pub(crate) struct CharGroup {
    chars: Vec<u32>,
    whitespace: bool,
    letter: bool,
    digit: bool,
    punctuation: bool,
    symbol: bool,
}

pub(crate) fn is_punctuation(c: char) -> bool {
    matches!(
        unicode::get_type(c),
        gc::START_PUNCTUATION
            | gc::END_PUNCTUATION
            | gc::OTHER_PUNCTUATION
            | gc::CONNECTOR_PUNCTUATION
            | gc::DASH_PUNCTUATION
            | gc::INITIAL_QUOTE_PUNCTUATION
            | gc::FINAL_QUOTE_PUNCTUATION
    )
}

pub(crate) fn is_symbol(c: char) -> bool {
    matches!(
        unicode::get_type(c),
        gc::CURRENCY_SYMBOL | gc::MATH_SYMBOL | gc::OTHER_SYMBOL | gc::MODIFIER_SYMBOL
    )
}

impl TokenChar for CharGroup {
    fn is_token_char(&self, c: char) -> bool {
        if self.whitespace && unicode::is_whitespace(c) {
            return false;
        }
        if self.letter && unicode::is_letter(c) {
            return false;
        }
        if self.digit && unicode::is_digit(c) {
            return false;
        }
        if self.punctuation && is_punctuation(c) {
            return false;
        }
        if self.symbol && is_symbol(c) {
            return false;
        }
        !self.chars.contains(&(c as u32))
    }
}

fn runtime(msg: String) -> AnalysisError {
    AnalysisError::other("runtime_exception", 500, msg)
}

/// Parses `tokenize_on_chars` (single characters, escapes like `\n` and ` `, or class names).
pub(crate) fn char_group(items: &[String]) -> Result<CharGroup> {
    let mut g = CharGroup {
        chars: Vec::new(),
        whitespace: false,
        letter: false,
        digit: false,
        punctuation: false,
        symbol: false,
    };
    for c in items {
        let units: Vec<u16> = c.encode_utf16().collect();
        if units.is_empty() {
            return Err(runtime(
                "[tokenize_on_chars] cannot contain empty characters".into(),
            ));
        }
        if units.len() == 1 {
            g.chars.push(units[0] as u32);
        } else if units[0] == u16::from(b'\\') {
            g.chars.push(parse_escaped_char(c, &units)? as u32);
        } else {
            match c.as_str() {
                "letter" => g.letter = true,
                "digit" => g.digit = true,
                "whitespace" => g.whitespace = true,
                "punctuation" => g.punctuation = true,
                "symbol" => g.symbol = true,
                _ => return Err(runtime(format!("Invalid escaped char in [{c}]"))),
            }
        }
    }
    Ok(g)
}

fn parse_escaped_char(s: &str, units: &[u16]) -> Result<u16> {
    let c = char::from_u32(units[1] as u32).unwrap_or('\u{FFFD}');
    Ok(match c {
        '\\' => '\\' as u16,
        'n' => '\n' as u16,
        't' => '\t' as u16,
        'r' => '\r' as u16,
        'b' => 8,
        'f' => 12,
        'u' => {
            if units.len() > 6 {
                return Err(runtime(format!("Invalid escaped char in [{s}]")));
            }
            let hex = String::from_utf16_lossy(&units[2..]);
            match i64::from_str_radix(&hex, 16) {
                Ok(v) => v as u16,
                Err(_) => {
                    return Err(AnalysisError::other(
                        "number_format_exception",
                        400,
                        format!(
                            "For input string: \"{hex}\"{}",
                            if hex.is_empty() {
                                ""
                            } else {
                                " under radix 16"
                            }
                        ),
                    ));
                }
            }
        }
        _ => return Err(runtime(format!("Invalid escaped char {c} in [{s}]"))),
    })
}

/// Which characters an n-gram may contain (`token_chars`).
#[derive(Clone, Debug, Default)]
pub(crate) struct TokenChars {
    letter: bool,
    digit: bool,
    whitespace: bool,
    punctuation: bool,
    symbol: bool,
    /// `Character.getType` values.
    categories: u64,
    custom: Vec<u32>,
}

impl TokenChars {
    pub fn matches(&self, c: char) -> bool {
        (self.letter && unicode::is_letter(c))
            || (self.digit && unicode::is_digit(c))
            || (self.whitespace && unicode::is_whitespace(c))
            || (self.punctuation && is_punctuation(c))
            || (self.symbol && is_symbol(c))
            || (self.categories != 0 && self.categories & (1 << unicode::get_type(c)) != 0)
            || (!self.custom.is_empty() && self.custom.contains(&(c as u32)))
    }
}

/// `java.lang.Character`'s general category constants by lowercased field name.
const CATEGORY_NAMES: &[(&str, u8)] = &[
    ("unassigned", 0),
    ("uppercase_letter", 1),
    ("lowercase_letter", 2),
    ("titlecase_letter", 3),
    ("modifier_letter", 4),
    ("other_letter", 5),
    ("non_spacing_mark", 6),
    ("enclosing_mark", 7),
    ("combining_spacing_mark", 8),
    ("decimal_digit_number", 9),
    ("letter_number", 10),
    ("other_number", 11),
    ("space_separator", 12),
    ("line_separator", 13),
    ("paragraph_separator", 14),
    ("control", 15),
    ("format", 16),
    ("private_use", 18),
    ("surrogate", 19),
    ("dash_punctuation", 20),
    ("start_punctuation", 21),
    ("end_punctuation", 22),
    ("connector_punctuation", 23),
    ("other_punctuation", 24),
    ("math_symbol", 25),
    ("currency_symbol", 26),
    ("modifier_symbol", 27),
    ("other_symbol", 28),
    ("initial_quote_punctuation", 29),
    ("final_quote_punctuation", 30),
];

/// The names `token_chars` accepts, in the order Elasticsearch's error message lists them (a
/// Java `HashSet` of 36 strings iterates by `hashCode` bucket at capacity 64).
fn token_char_names_java_order() -> Vec<&'static str> {
    let mut names: Vec<&'static str> =
        vec!["letter", "digit", "whitespace", "punctuation", "symbol"];
    // The matcher map itself is a HashMap filled in this order; its iteration order decides the
    // insertion order into the set, which matters within a bucket.
    names.extend(CATEGORY_NAMES.iter().map(|(n, _)| *n));
    let mut map_order = names.clone();
    map_order.sort_by_key(|n| java_bucket(n, 64));
    map_order.push("custom");
    map_order.sort_by_key(|n| java_bucket(n, 64));
    map_order
}

/// The `HashMap` bucket of a string key at the given table capacity.
pub(crate) fn java_bucket(s: &str, capacity: u32) -> u32 {
    let mut h: i32 = 0;
    for u in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(u as i32);
    }
    let h = h as u32;
    (h ^ (h >> 16)) & (capacity - 1)
}

/// Parses `token_chars` / `custom_token_chars`; None when no classes are configured.
pub(crate) fn parse_token_chars(
    classes: &[String],
    custom: Option<&str>,
) -> Result<Option<TokenChars>> {
    if classes.is_empty() {
        return Ok(None);
    }
    let mut m = TokenChars::default();
    for cls in classes {
        let cls = cls.to_lowercase();
        let cls = cls.trim();
        match cls {
            "letter" => m.letter = true,
            "digit" => m.digit = true,
            "whitespace" => m.whitespace = true,
            "punctuation" => m.punctuation = true,
            "symbol" => m.symbol = true,
            "custom" => {
                let Some(custom) = custom else {
                    return Err(AnalysisError::illegal(
                        "Token type: 'custom' requires setting `custom_token_chars`",
                    ));
                };
                // Java's String.chars(): UTF-16 units.
                // Java's String.chars() yields UTF-16 units, so supplementary characters never match.
                m.custom.extend(custom.encode_utf16().map(u32::from));
            }
            other => match CATEGORY_NAMES.iter().find(|(n, _)| *n == other) {
                Some((_, v)) => m.categories |= 1 << v,
                None => {
                    return Err(AnalysisError::illegal(format!(
                        "Unknown token type: '{other}', must be one of [{}]",
                        token_char_names_java_order().join(", ")
                    )));
                }
            },
        }
    }
    Ok(Some(m))
}

/// `NGramTokenizer` / `EdgeNGramTokenizer`: grams of `min..=max` code points starting at every
/// position (or only at token starts for edges), never spanning a non-token character.
pub(crate) struct NGramTokenizer {
    min: usize,
    max: usize,
    edges: bool,
    chars: Option<Arc<TokenChars>>,
    /// Per code point: (char, byte offset, UTF-16 offset); one extra entry marks the end.
    cps: Vec<(char, u32, u32)>,
    start: usize,
    gram: usize,
    last_checked: isize,
    last_non_token: isize,
}

impl NGramTokenizer {
    pub fn new(
        min: usize,
        max: usize,
        edges: bool,
        chars: Option<Arc<TokenChars>>,
    ) -> NGramTokenizer {
        NGramTokenizer {
            min,
            max,
            edges,
            chars,
            cps: Vec::new(),
            start: 0,
            gram: min,
            last_checked: -1,
            last_non_token: -1,
        }
    }

    fn is_token_char(&self, c: char) -> bool {
        match &self.chars {
            None => true,
            Some(m) => m.matches(c),
        }
    }
}

/// `minGram`/`maxGram` checks of Lucene's n-gram components.
pub(crate) fn check_grams(min: i32, max: i32) -> Result<()> {
    if min < 1 {
        return Err(AnalysisError::illegal("minGram must be greater than zero"));
    }
    if min > max {
        return Err(AnalysisError::illegal(
            "minGram must not be greater than maxGram",
        ));
    }
    Ok(())
}

impl Tokenizer for NGramTokenizer {
    fn reset(&mut self, text: &str) {
        self.cps.clear();
        let mut u = 0u32;
        for (b, c) in text.char_indices() {
            self.cps.push((c, b as u32, u));
            u += c.len_utf16() as u32;
        }
        self.cps.push(('\0', text.len() as u32, u));
        self.start = 0;
        self.gram = self.min;
        self.last_checked = -1;
        self.last_non_token = -1;
    }

    fn next(&mut self, text: &str, t: &mut Tok) -> bool {
        let n = self.cps.len() - 1;
        loop {
            if self.gram > self.max || self.start + self.gram > n {
                if self.start + 1 + self.min > n {
                    return false;
                }
                self.start += 1;
                self.gram = self.min;
            }
            // updateLastNonTokenChar
            let term_end = (self.start + self.gram - 1) as isize;
            if term_end > self.last_checked {
                let mut i = term_end;
                while i > self.last_checked {
                    if !self.is_token_char(self.cps[i as usize].0) {
                        self.last_non_token = i;
                        break;
                    }
                    i -= 1;
                }
                self.last_checked = term_end;
            }
            let s = self.start as isize;
            let contains_non_token =
                self.last_non_token >= s && self.last_non_token < s + self.gram as isize;
            let edge_violation = self.edges && self.last_non_token != s - 1;
            if contains_non_token || edge_violation {
                self.start += 1;
                self.gram = self.min;
                continue;
            }
            let (b0, u0) = (self.cps[self.start].1 as usize, self.cps[self.start].2);
            let (b1, u1) = (
                self.cps[self.start + self.gram].1 as usize,
                self.cps[self.start + self.gram].2,
            );
            t.term.push_str(&text[b0..b1]);
            t.start = u0;
            t.end = u1;
            self.gram += 1;
            return true;
        }
    }
}

/// `PathHierarchyTokenizer`: `/a/b/c` gives `/a`, `/a/b`, `/a/b/c` (after `skip` leading
/// components), with the delimiter replaced.
pub(crate) struct PathHierarchyTokenizer {
    delimiter: char,
    replacement: char,
    skip: usize,
    pos: usize,
    chars_read: usize,
    start_position: usize,
    skipped: usize,
    end_delimiter: bool,
    result: String,
    result_len: usize,
}

impl PathHierarchyTokenizer {
    pub fn new(delimiter: char, replacement: char, skip: usize) -> Self {
        PathHierarchyTokenizer {
            delimiter,
            replacement,
            skip,
            pos: 0,
            chars_read: 0,
            start_position: 0,
            skipped: 0,
            end_delimiter: false,
            result: String::new(),
            result_len: 0,
        }
    }
}

impl Tokenizer for PathHierarchyTokenizer {
    fn reset(&mut self, _text: &str) {
        self.pos = 0;
        self.chars_read = 0;
        self.start_position = 0;
        self.skipped = 0;
        self.end_delimiter = false;
        self.result.clear();
        self.result_len = 0;
    }

    fn next(&mut self, text: &str, t: &mut Tok) -> bool {
        t.term.push_str(&self.result);
        let mut length = 0usize;
        let mut added = false;
        if self.end_delimiter {
            t.term.push(self.replacement);
            length += 1;
            self.end_delimiter = false;
            added = true;
        }
        loop {
            let c = match text[self.pos..].chars().next() {
                Some(c) => {
                    self.pos += c.len_utf8();
                    self.chars_read += c.len_utf16();
                    c
                }
                None => {
                    if self.skipped > self.skip {
                        length += self.result_len;
                        t.start = self.start_position as u32;
                        t.end = (self.start_position + length) as u32;
                        if added {
                            self.result.clone_from(&t.term);
                            self.result_len = length;
                        }
                        return added;
                    }
                    return false;
                }
            };
            let ulen = c.len_utf16();
            if !added {
                added = true;
                self.skipped += 1;
                if self.skipped > self.skip {
                    t.term.push(if c == self.delimiter {
                        self.replacement
                    } else {
                        c
                    });
                    length += ulen;
                } else {
                    self.start_position += ulen;
                }
            } else if c == self.delimiter {
                if self.skipped > self.skip {
                    self.end_delimiter = true;
                    break;
                }
                self.skipped += 1;
                if self.skipped > self.skip {
                    t.term.push(self.replacement);
                    length += 1;
                } else {
                    self.start_position += 1;
                }
            } else if self.skipped > self.skip {
                t.term.push(c);
                length += ulen;
            } else {
                self.start_position += ulen;
            }
        }
        length += self.result_len;
        t.start = self.start_position as u32;
        t.end = (self.start_position + length) as u32;
        self.result.clone_from(&t.term);
        self.result_len = length;
        true
    }

    fn end(&mut self, _text: &str, t: &mut Tok) {
        t.start = self.chars_read as u32;
        t.end = self.chars_read as u32;
    }
}

/// `ReversePathHierarchyTokenizer`: `a/b/c` gives `a/b/c`, `b/c`, `c` (dropping `skip` trailing
/// components).
pub(crate) struct ReversePathHierarchyTokenizer {
    delimiter: char,
    replacement: char,
    skip: usize,
    replaced: String,
    /// Delimiter positions as (UTF-16, byte) offsets into `replaced`.
    positions: Vec<(usize, usize)>,
    end_position: (usize, usize),
    skipped: usize,
    started: bool,
    total: usize,
}

impl ReversePathHierarchyTokenizer {
    pub fn new(delimiter: char, replacement: char, skip: usize) -> Self {
        ReversePathHierarchyTokenizer {
            delimiter,
            replacement,
            skip,
            replaced: String::new(),
            positions: Vec::new(),
            end_position: (0, 0),
            skipped: 0,
            started: false,
            total: 0,
        }
    }
}

impl Tokenizer for ReversePathHierarchyTokenizer {
    fn reset(&mut self, _text: &str) {
        self.replaced.clear();
        self.positions.clear();
        self.end_position = (0, 0);
        self.skipped = 0;
        self.started = false;
        self.total = 0;
    }

    fn next(&mut self, text: &str, t: &mut Tok) -> bool {
        if !self.started {
            self.started = true;
            self.positions.push((0, 0));
            let mut u = 0;
            for c in text.chars() {
                u += c.len_utf16();
                if c == self.delimiter {
                    self.replaced.push(self.replacement);
                    self.positions.push((u, self.replaced.len()));
                } else {
                    self.replaced.push(c);
                }
            }
            self.total = u;
            if self.positions.last().unwrap().0 < u {
                self.positions.push((u, self.replaced.len()));
            }
            let count = self.positions.len();
            if count > self.skip {
                self.end_position = self.positions[count - 1 - self.skip];
            }
        }
        let count = self.positions.len() as isize;
        if (self.skipped as isize) < count - self.skip as isize - 1 {
            let start = self.positions[self.skipped];
            t.term
                .push_str(&self.replaced[start.1..self.end_position.1]);
            t.start = start.0 as u32;
            t.end = self.end_position.0 as u32;
            self.skipped += 1;
            return true;
        }
        false
    }

    fn end(&mut self, _text: &str, t: &mut Tok) {
        t.start = self.total as u32;
        t.end = self.total as u32;
    }
}

/// `PatternTokenizer`: with `group == -1` the text between matches, otherwise the given group of
/// every match (empty strings are skipped either way).
pub(crate) struct PatternTokenizer {
    re: Arc<JavaRegex>,
    group: i32,
    caps: crate::jregex::Captures,
    finder: crate::jregex::Finder,
    /// Split mode: start of the pending token; usize::MAX when exhausted.
    index: usize,
    /// UTF-16 offset cache: (byte, utf16) of the last conversion point.
    conv: (usize, usize),
}

impl PatternTokenizer {
    pub fn new(re: Arc<JavaRegex>, group: i32) -> Self {
        let caps = re.captures();
        PatternTokenizer {
            re,
            group,
            caps,
            finder: crate::jregex::Finder::new(),
            index: 0,
            conv: (0, 0),
        }
    }

    /// UTF-16 offset of byte `b` (offsets are requested in increasing order).
    fn u16_at(&mut self, text: &str, b: usize) -> u32 {
        if b < self.conv.0 {
            self.conv = (0, 0);
        }
        let (cb, cu) = self.conv;
        let u = cu + unicode::utf16_len(&text[cb..b]);
        self.conv = (b, u);
        u as u32
    }
}

impl Tokenizer for PatternTokenizer {
    fn reset(&mut self, _text: &str) {
        self.finder.reset();
        self.index = 0;
        self.conv = (0, 0);
    }

    fn next(&mut self, text: &str, t: &mut Tok) -> bool {
        if self.index >= text.len() {
            return false;
        }
        if self.group >= 0 {
            let g = self.group as usize;
            while self
                .finder
                .next(&self.re, text, &mut self.caps, g > 0)
                .is_some()
            {
                let Some((s, e)) = self.caps.get(g) else {
                    continue;
                };
                self.index = s;
                if s == e {
                    continue;
                }
                t.term.push_str(&text[s..e]);
                t.start = self.u16_at(text, s);
                t.end = self.u16_at(text, e);
                return true;
            }
            self.index = usize::MAX;
            false
        } else {
            while let Some((s, e)) = self.finder.next(&self.re, text, &mut self.caps, false) {
                if s > self.index {
                    t.term.push_str(&text[self.index..s]);
                    t.start = self.u16_at(text, self.index);
                    t.end = self.u16_at(text, s);
                    self.index = e;
                    return true;
                }
                self.index = e;
            }
            if text.len() <= self.index {
                self.index = usize::MAX;
                return false;
            }
            let i = self.index;
            t.term.push_str(&text[i..]);
            t.start = self.u16_at(text, i);
            t.end = self.u16_at(text, text.len());
            self.index = usize::MAX;
            true
        }
    }
}

/// `SimplePatternTokenizer` (`split == false`): the longest match at each position is a token;
/// `SimplePatternSplitTokenizer` (`split == true`): the longest matches separate tokens.
pub(crate) struct SimplePatternTokenizer {
    dfa: Arc<RunAutomaton>,
    split: bool,
    pos: usize,
    upos: usize,
}

impl SimplePatternTokenizer {
    pub fn new(dfa: Arc<RunAutomaton>, split: bool) -> Self {
        SimplePatternTokenizer {
            dfa,
            split,
            pos: 0,
            upos: 0,
        }
    }

    /// The longest non-empty match at byte `at`.
    fn longest(&self, text: &str, at: usize) -> Longest {
        let mut state = RunAutomaton::START;
        let mut best = None;
        let mut units = 0;
        let mut reached_end = true;
        for (i, c) in text[at..].char_indices() {
            state = self.dfa.step(state, c);
            if state == RunAutomaton::DEAD {
                reached_end = false;
                break;
            }
            units += c.len_utf16();
            if self.dfa.accepts(state) {
                best = Some((at + i + c.len_utf8(), units));
            }
        }
        match best {
            Some((end, ulen)) => Longest::Match(end, ulen),
            // Lucene gives up when a match attempt runs into the end of the input.
            None if reached_end && at < text.len() => Longest::Eof,
            None => Longest::None,
        }
    }

    fn skip_char(&mut self, text: &str) {
        let c = text[self.pos..].chars().next().unwrap_or(' ');
        self.pos += c.len_utf8();
        self.upos += c.len_utf16();
    }
}

enum Longest {
    Match(usize, usize),
    None,
    /// No match, and the attempt read to the end of the text.
    Eof,
}

impl Tokenizer for SimplePatternTokenizer {
    fn reset(&mut self, _text: &str) {
        self.pos = 0;
        self.upos = 0;
    }

    /// Lucene reports how far it has read (after any push-back), which is the end of the text
    /// only once the stream is exhausted.
    fn end(&mut self, _text: &str, t: &mut Tok) {
        t.start = self.upos as u32;
        t.end = self.upos as u32;
    }

    fn next(&mut self, text: &str, t: &mut Tok) -> bool {
        if !self.split {
            while self.pos < text.len() {
                match self.longest(text, self.pos) {
                    Longest::Match(end, ulen) => {
                        t.term.push_str(&text[self.pos..end]);
                        t.start = self.upos as u32;
                        t.end = (self.upos + ulen) as u32;
                        self.pos = end;
                        self.upos += ulen;
                        return true;
                    }
                    Longest::Eof => {
                        self.upos += crate::unicode::utf16_len(&text[self.pos..]);
                        self.pos = text.len();
                        return false;
                    }
                    Longest::None => self.skip_char(text),
                }
            }
            return false;
        }
        let (mut tok_b, mut tok_u) = (self.pos, self.upos);
        while self.pos < text.len() {
            match self.longest(text, self.pos) {
                Longest::Match(end, ulen) => {
                    if self.pos > tok_b {
                        t.term.push_str(&text[tok_b..self.pos]);
                        t.start = tok_u as u32;
                        t.end = self.upos as u32;
                        self.pos = end;
                        self.upos += ulen;
                        return true;
                    }
                    self.pos = end;
                    self.upos += ulen;
                    tok_b = self.pos;
                    tok_u = self.upos;
                }
                Longest::Eof => {
                    self.upos += crate::unicode::utf16_len(&text[self.pos..]);
                    self.pos = text.len();
                }
                Longest::None => self.skip_char(text),
            }
        }
        if self.pos > tok_b {
            t.term.push_str(&text[tok_b..self.pos]);
            t.start = tok_u as u32;
            t.end = self.upos as u32;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn run(tk: &mut dyn Tokenizer, text: &str) -> Vec<(String, u32, u32)> {
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

    fn terms(tk: &mut dyn Tokenizer, text: &str) -> Vec<String> {
        run(tk, text).into_iter().map(|t| t.0).collect()
    }

    #[test]
    fn whitespace_and_letter() {
        let mut ws = CharTokenizer::new(Arc::new(Whitespace), 255, false);
        assert_eq!(
            run(&mut ws, " a  bc\td "),
            vec![("a".into(), 1, 2), ("bc".into(), 4, 6), ("d".into(), 7, 8)]
        );
        let mut ws = CharTokenizer::new(Arc::new(Whitespace), 3, false);
        assert_eq!(terms(&mut ws, "abcdefg h"), vec!["abc", "def", "g", "h"]);
        let mut l = CharTokenizer::new(Arc::new(Letter), 255, true);
        assert_eq!(
            terms(&mut l, "It's DÉJÀ-vu 42x"),
            vec!["it", "s", "déjà", "vu", "x"]
        );
        // Offsets count UTF-16 units.
        let mut l = CharTokenizer::new(Arc::new(Letter), 255, false);
        assert_eq!(run(&mut l, "😀ab"), vec![("ab".into(), 2, 4)]);
    }

    #[test]
    fn keyword() {
        let mut k = KeywordTokenizer::new(256);
        assert_eq!(run(&mut k, "New York"), vec![("New York".into(), 0, 8)]);
        assert_eq!(run(&mut k, ""), vec![("".into(), 0, 0)]);
    }

    #[test]
    fn ngrams() {
        let mut ng = NGramTokenizer::new(1, 2, false, None);
        assert_eq!(terms(&mut ng, "abc"), vec!["a", "ab", "b", "bc", "c"]);
        let mut eg = NGramTokenizer::new(1, 3, true, None);
        assert_eq!(terms(&mut eg, "abcd"), vec!["a", "ab", "abc"]);
        let tc = parse_token_chars(&["letter".into()], None)
            .unwrap()
            .map(Arc::new);
        let mut eg = NGramTokenizer::new(2, 3, true, tc.clone());
        assert_eq!(
            run(&mut eg, "ab cde"),
            vec![
                ("ab".into(), 0, 2),
                ("cd".into(), 3, 5),
                ("cde".into(), 3, 6)
            ]
        );
        let mut ng = NGramTokenizer::new(2, 2, false, tc);
        assert_eq!(terms(&mut ng, "ab1cd"), vec!["ab", "cd"]);
    }

    #[test]
    fn token_chars_errors() {
        let e = parse_token_chars(&["nope".into()], None).unwrap_err();
        assert!(
            e.reason
                .starts_with("Unknown token type: 'nope', must be one of [")
        );
        assert!(e.reason.contains("custom") && e.reason.contains("other_letter"));
        let e = parse_token_chars(&["custom".into()], None).unwrap_err();
        assert_eq!(
            e.reason,
            "Token type: 'custom' requires setting `custom_token_chars`"
        );
        let m = parse_token_chars(&["Custom".into(), "digit".into()], Some("+-"))
            .unwrap()
            .unwrap();
        assert!(m.matches('+') && m.matches('5') && !m.matches('a'));
        let m = parse_token_chars(&["math_symbol".into()], None)
            .unwrap()
            .unwrap();
        assert!(m.matches('+') && !m.matches('-'));
    }

    #[test]
    fn paths() {
        let mut p = PathHierarchyTokenizer::new('/', '/', 0);
        assert_eq!(
            run(&mut p, "/a/b/c"),
            vec![
                ("/a".into(), 0, 2),
                ("/a/b".into(), 0, 4),
                ("/a/b/c".into(), 0, 6)
            ]
        );
        let mut p = PathHierarchyTokenizer::new('-', '/', 1);
        assert_eq!(terms(&mut p, "one-two-three"), vec!["/two", "/two/three"]);
        let mut r = ReversePathHierarchyTokenizer::new('.', '.', 0);
        assert_eq!(
            run(&mut r, "www.example.com"),
            vec![
                ("www.example.com".into(), 0, 15),
                ("example.com".into(), 4, 15),
                ("com".into(), 12, 15)
            ]
        );
        let mut r = ReversePathHierarchyTokenizer::new('.', '.', 1);
        assert_eq!(
            terms(&mut r, "www.example.com"),
            vec!["www.example.", "example."]
        );
    }

    #[test]
    fn patterns() {
        let re = Arc::new(crate::jregex::compile("\\W+", 0).unwrap());
        let mut p = PatternTokenizer::new(re, -1);
        assert_eq!(
            run(&mut p, "foo,bar  baz!"),
            vec![
                ("foo".into(), 0, 3),
                ("bar".into(), 4, 7),
                ("baz".into(), 9, 12)
            ]
        );
        let re = Arc::new(crate::jregex::compile("\"((?:\\\\\"|[^\"]|\\\\\")+)\"", 0).unwrap());
        let mut p = PatternTokenizer::new(re, 1);
        assert_eq!(
            terms(&mut p, "\"value\", \"value with embedded \\\" quote\""),
            vec!["value", "value with embedded \\\" quote"]
        );
        let re = Arc::new(crate::jregex::compile(",", 0).unwrap());
        let mut p = PatternTokenizer::new(re, -1);
        assert_eq!(terms(&mut p, ",a,,b,"), vec!["a", "b"]);
        assert!(terms(&mut p, "").is_empty());
    }

    #[test]
    fn simple_patterns() {
        let dfa = Arc::new(crate::lregexp::compile("[0123456789]{3}", false).unwrap());
        let mut p = SimplePatternTokenizer::new(dfa.clone(), false);
        assert_eq!(
            run(&mut p, "fd-786-335-514-x"),
            vec![
                ("786".into(), 3, 6),
                ("335".into(), 7, 10),
                ("514".into(), 11, 14)
            ]
        );
        let dfa = Arc::new(crate::lregexp::compile("_", true).unwrap());
        let mut p = SimplePatternTokenizer::new(dfa, true);
        assert_eq!(
            run(&mut p, "an_underscored_phrase"),
            vec![
                ("an".into(), 0, 2),
                ("underscored".into(), 3, 14),
                ("phrase".into(), 15, 21)
            ]
        );
        assert_eq!(terms(&mut p, "__a__b_"), vec!["a", "b"]);
    }

    #[test]
    fn char_groups() {
        let g = Arc::new(
            char_group(&[
                "whitespace".into(),
                "-".into(),
                "\\n".into(),
                "\\u0021".into(),
            ])
            .unwrap(),
        );
        let mut tk = CharTokenizer::new(g, 255, false);
        assert_eq!(terms(&mut tk, "a-b c\nd!e"), vec!["a", "b", "c", "d", "e"]);
        assert_eq!(
            char_group(&["\\x".into()]).err().unwrap().reason,
            "Invalid escaped char x in [\\x]"
        );
        assert_eq!(
            char_group(&["ab".into()]).err().unwrap().reason,
            "Invalid escaped char in [ab]"
        );
        assert_eq!(java_bucket("letter", 64), java_bucket("letter", 64));
    }
}
