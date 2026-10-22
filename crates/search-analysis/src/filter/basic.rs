//! The small token filters: `stop`, `length`, `limit`, `trim`, `truncate`, `reverse`, `unique`,
//! `remove_duplicates`, `keep`, `keep_types`, `keyword_marker`, `keyword_repeat`, `apostrophe`,
//! `classic`, `elision`, `decimal_digit`, `delimited_payload` and `fingerprint`.

use std::sync::Arc;

use super::{Filtering, Keep, WordSet};
use crate::error::AnalysisError;
use crate::stream::{Filter, TokenStream};
use crate::token::Tok;
use crate::unicode::{self, utf16_len};

/// `StopFilter`.
pub(crate) struct Stop {
    pub words: Arc<WordSet>,
    pub scratch: String,
}

impl Keep for Stop {
    #[inline]
    fn keep(&mut self, t: &Tok) -> bool {
        !self.words.contains(&t.term, &mut self.scratch)
    }
}

pub(crate) fn stop(words: Arc<WordSet>) -> Filtering<Stop> {
    Filtering::new(Stop {
        words,
        scratch: String::new(),
    })
}

/// `SuggestStopFilter` (`remove_trailing: false`): like `stop`, except that a stop word ending
/// the input without trailing separator is kept and marked as a keyword (it may be the prefix of
/// a word being typed).
pub(crate) struct SuggestStop {
    words: Arc<WordSet>,
    scratch: String,
    end_state: Option<Tok>,
    saved: Tok,
}

impl SuggestStop {
    pub fn new(words: Arc<WordSet>) -> Self {
        SuggestStop {
            words,
            scratch: String::new(),
            end_state: None,
            saved: Tok::default(),
        }
    }
}

impl Filter for SuggestStop {
    fn reset(&mut self) {
        self.end_state = None;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if self.end_state.is_some() {
            return false;
        }
        if !input.next(t) {
            return false;
        }
        let mut skipped = 0;
        loop {
            if self.words.contains(&t.term, &mut self.scratch) {
                let pos_inc = t.pos_inc;
                let end_offset = t.end;
                self.saved.clone_from(t);
                if input.next(t) {
                    skipped += pos_inc;
                } else {
                    t.clear_for_end();
                    input.end(t);
                    let final_end = t.end;
                    self.end_state = Some(t.clone());
                    if final_end > end_offset {
                        return false;
                    }
                    t.clone_from(&self.saved);
                    t.pos_inc += skipped;
                    t.keyword = true;
                    return true;
                }
            } else {
                t.pos_inc += skipped;
                return true;
            }
        }
    }

    fn end(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        match &self.end_state {
            None => input.end(t),
            Some(s) => t.clone_from(s),
        }
    }
}

/// `LengthFilter`: keeps terms whose UTF-16 length is within `[min, max]`.
pub(crate) struct Length {
    pub min: usize,
    pub max: usize,
}

impl Keep for Length {
    fn keep(&mut self, t: &Tok) -> bool {
        let len = utf16_len(&t.term);
        len >= self.min && len <= self.max
    }
}

/// `LimitTokenCountFilter`.
pub(crate) struct Limit {
    pub max: usize,
    pub consume_all: bool,
    count: usize,
    exhausted: bool,
}

impl Limit {
    pub fn new(max: usize, consume_all: bool) -> Self {
        Limit {
            max,
            consume_all,
            count: 0,
            exhausted: false,
        }
    }
}

impl Filter for Limit {
    fn reset(&mut self) {
        self.count = 0;
        self.exhausted = false;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if self.exhausted {
            return false;
        }
        if self.count < self.max {
            if input.next(t) {
                self.count += 1;
                return true;
            }
            self.exhausted = true;
            return false;
        }
        while self.consume_all && input.next(t) {}
        false
    }
}

/// `TrimFilter`: strips Java whitespace (per UTF-16 unit) from both ends.
pub(crate) fn trim(t: &mut Tok) {
    let trimmed = t.term.trim_matches(unicode::is_whitespace);
    if trimmed.len() != t.term.len() {
        let (start, len) = (
            trimmed.as_ptr() as usize - t.term.as_ptr() as usize,
            trimmed.len(),
        );
        t.term.truncate(start + len);
        t.term.drain(..start);
    }
}

/// `TruncateTokenFilter` (by UTF-16 units, as Elasticsearch uses it); keyword tokens are kept.
pub(crate) fn truncate(t: &mut Tok, len: usize) {
    if t.keyword || t.term.len() <= len {
        return;
    }
    let mut units = 0;
    for (i, c) in t.term.char_indices() {
        let u = c.len_utf16();
        if units + u > len {
            // A cut through a surrogate pair leaves a lone high surrogate in Java, which
            // becomes U+FFFD when the term is indexed (and in `_analyze` output).
            t.term.truncate(i);
            t.term.push('\u{FFFD}');
            return;
        }
        units += u;
        if units == len {
            t.term.truncate(i + c.len_utf8());
            return;
        }
    }
}

/// `ReverseStringFilter`: reverses code points (surrogate pairs stay intact).
pub(crate) fn reverse(t: &mut Tok, scratch: &mut String) {
    scratch.clear();
    scratch.extend(t.term.chars().rev());
    std::mem::swap(&mut t.term, scratch);
}

/// `UniqueTokenFilter` (`unique`): drops terms seen before in the stream (or, with
/// `only_on_same_position`, at the same position).
pub(crate) struct Unique {
    pub only_same_position: bool,
    seen: super::TermSet,
}

impl Unique {
    pub fn new(only_same_position: bool) -> Self {
        Unique {
            only_same_position,
            seen: super::TermSet::default(),
        }
    }
}

impl Keep for Unique {
    fn reset(&mut self) {
        self.seen.clear();
    }

    fn keep(&mut self, t: &Tok) -> bool {
        if self.only_same_position && t.pos_inc > 0 {
            self.seen.clear();
        }
        let fresh = self.seen.insert(&t.term);
        // With `only_on_same_position`, a token starting a position is always kept.
        fresh || (self.only_same_position && t.pos_inc > 0)
    }
}

/// `RemoveDuplicatesTokenFilter`: drops a token identical to one at the same position. Removed
/// tokens have increment 0, so no position bookkeeping is needed.
pub(crate) struct RemoveDuplicates {
    seen: Vec<String>,
    used: usize,
}

impl RemoveDuplicates {
    pub fn new() -> Self {
        RemoveDuplicates {
            seen: Vec::new(),
            used: 0,
        }
    }
}

impl Filter for RemoveDuplicates {
    fn reset(&mut self) {
        self.used = 0;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        while input.next(t) {
            if t.pos_inc > 0 {
                self.used = 0;
            }
            let duplicate = t.pos_inc == 0 && self.seen[..self.used].contains(&t.term);
            if self.used == self.seen.len() {
                self.seen.push(String::new());
            }
            self.seen[self.used].clone_from(&t.term);
            self.used += 1;
            if !duplicate {
                return true;
            }
        }
        false
    }
}

/// `KeepWordFilter`.
pub(crate) struct KeepWords {
    pub words: Arc<WordSet>,
    pub scratch: String,
}

impl Keep for KeepWords {
    fn keep(&mut self, t: &Tok) -> bool {
        self.words.contains(&t.term, &mut self.scratch)
    }
}

/// `TypeTokenFilter` (`keep_types`).
pub(crate) struct KeepTypes {
    pub types: Arc<Vec<String>>,
    pub include: bool,
}

impl Keep for KeepTypes {
    fn keep(&mut self, t: &Tok) -> bool {
        self.types.iter().any(|ty| ty == t.kind) == self.include
    }
}

/// `SetKeywordMarkerFilter` / `PatternKeywordMarkerFilter`.
pub(crate) enum KeywordMarker {
    Set(Arc<WordSet>, String),
    /// Marks tokens the pattern matches entirely (`Matcher.matches()`).
    Pattern(Arc<crate::jregex::JavaRegex>),
}

impl Filter for KeywordMarker {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !input.next(t) {
            return false;
        }
        let is = match self {
            KeywordMarker::Set(words, scratch) => words.contains(&t.term, scratch),
            KeywordMarker::Pattern(re) => re.full_match(&t.term),
        };
        if is {
            t.keyword = true;
        }
        true
    }
}

/// `KeywordRepeatFilter`: every token twice, first marked as keyword, then unmarked at the same
/// position.
pub(crate) struct KeywordRepeat {
    pending: Option<Tok>,
}

impl KeywordRepeat {
    pub fn new() -> Self {
        KeywordRepeat { pending: None }
    }
}

impl Filter for KeywordRepeat {
    fn reset(&mut self) {
        self.pending = None;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if let Some(p) = self.pending.take() {
            t.clone_from(&p);
            t.pos_inc = 0;
            t.keyword = false;
            return true;
        }
        if input.next(t) {
            self.pending = Some(t.clone());
            t.keyword = true;
            return true;
        }
        false
    }
}

/// `ApostropheFilter`: drops everything from the first apostrophe.
pub(crate) fn apostrophe(t: &mut Tok) {
    if let Some(i) = t.term.find(['\'', '\u{2019}']) {
        t.term.truncate(i);
    }
}

/// `ClassicFilter`: removes `'s` from `<APOSTROPHE>` tokens and dots from `<ACRONYM>` tokens.
pub(crate) fn classic(t: &mut Tok) {
    if t.kind == crate::tokenizer::classic::APOSTROPHE {
        if t.term.ends_with("'s") || t.term.ends_with("'S") {
            let n = t.term.len() - 2;
            t.term.truncate(n);
        }
    } else if t.kind == crate::tokenizer::classic::ACRONYM {
        t.term.retain(|c| c != '.');
    }
}

/// `ElisionFilter`: removes an article and its apostrophe (`l'avion` → `avion`).
pub(crate) struct Elision {
    pub articles: Arc<WordSet>,
    pub scratch: String,
}

impl Filter for Elision {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !input.next(t) {
            return false;
        }
        if let Some(i) = t.term.find(['\'', '\u{2019}']) {
            let quote_len = t.term[i..].chars().next().map_or(1, char::len_utf8);
            if self.articles.contains(&t.term[..i], &mut self.scratch) {
                t.term.drain(..i + quote_len);
            }
        }
        true
    }
}

/// `DecimalDigitFilter`: any Unicode decimal digit to its ASCII digit.
pub(crate) fn decimal_digit(t: &mut Tok, scratch: &mut String) {
    if t.term.is_ascii() {
        return;
    }
    scratch.clear();
    let mut changed = false;
    for c in t.term.chars() {
        if !c.is_ascii() && unicode::is_digit(c) {
            scratch.push((b'0' + unicode::digit_value(c).unwrap_or(0) as u8) as char);
            changed = true;
        } else {
            scratch.push(c);
        }
    }
    if changed {
        std::mem::swap(&mut t.term, scratch);
    }
}

/// Payload encoders of `delimited_payload`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PayloadEncoding {
    Float,
    Int,
    Identity,
}

/// `DelimitedPayloadTokenFilter`: `term|payload`.
pub(crate) struct DelimitedPayload {
    pub delimiter: char,
    pub encoding: PayloadEncoding,
}

impl Filter for DelimitedPayload {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !input.next(t) {
            return false;
        }
        match t.term.find(self.delimiter) {
            Some(i) => {
                let value = &t.term[i + self.delimiter.len_utf8()..];
                t.payload.clear();
                match self.encoding {
                    PayloadEncoding::Identity => t.payload.extend_from_slice(value.as_bytes()),
                    PayloadEncoding::Float => match java_parse_float(value) {
                        Some(f) => t.payload.extend_from_slice(&f.to_bits().to_be_bytes()),
                        None => {
                            // `FloatingDecimal.readJavaFormatString` trims first.
                            let trimmed = value.trim_matches(|c: char| c <= ' ');
                            let msg = if trimmed.is_empty() {
                                "empty String".to_string()
                            } else {
                                format!("For input string: \"{trimmed}\"")
                            };
                            t.error = Some(Box::new(number_format(&msg)));
                            return false;
                        }
                    },
                    PayloadEncoding::Int => match lucene_parse_int(value) {
                        Some(v) => t.payload.extend_from_slice(&v.to_be_bytes()),
                        None => {
                            let msg = if value.is_empty() {
                                "chars length is 0"
                            } else {
                                "Unable to parse"
                            };
                            t.error = Some(Box::new(number_format(msg)));
                            return false;
                        }
                    },
                }
                t.has_payload = true;
                t.term.truncate(i);
            }
            None => {
                t.has_payload = false;
                t.payload.clear();
            }
        }
        true
    }
}

fn number_format(msg: &str) -> AnalysisError {
    AnalysisError::other("number_format_exception", 400, msg)
}

/// `Float.parseFloat`.
pub(crate) fn java_parse_float(s: &str) -> Option<f32> {
    let s = s.trim_matches(|c: char| c <= ' ');
    let (neg, body) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let v = match body {
        "NaN" => f32::NAN,
        "Infinity" => f32::INFINITY,
        _ => {
            let body = body.strip_suffix(['f', 'F', 'd', 'D']).unwrap_or(body);
            if body.is_empty()
                || !body
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
            {
                return None;
            }
            if !body.bytes().any(|b| b.is_ascii_digit()) {
                return None;
            }
            body.parse::<f32>().ok()?
        }
    };
    Some(if neg { -v } else { v })
}

/// `ArrayUtil.parseInt` (decimal, optional leading `-`, Unicode digits allowed).
pub(crate) fn lucene_parse_int(s: &str) -> Option<i32> {
    if s.is_empty() || s.starts_with('+') {
        return None;
    }
    crate::settings::java_parse_int(s)
}

/// `FingerprintFilter`: all distinct terms sorted (by UTF-16 code units) and joined by
/// `separator` as a single token, or nothing if that would exceed `max_output_size`.
///
/// Distinct terms are kept in one arena string as byte ranges, sorted as they arrive (a binary
/// search both deduplicates and finds the insertion point), so collecting a document's terms
/// reuses the same two buffers and costs O(n log n) comparisons rather than a scan per term.
pub(crate) struct Fingerprint {
    pub max_output_size: usize,
    pub separator: char,
    arena: String,
    /// Byte ranges into `arena`, in UTF-16 order of their text.
    sorted: Vec<(u32, u32)>,
    ended: bool,
    has_final: bool,
    final_state: Tok,
}

impl Fingerprint {
    pub fn new(max_output_size: usize, separator: char) -> Self {
        Fingerprint {
            max_output_size,
            separator,
            arena: String::new(),
            sorted: Vec::new(),
            ended: false,
            has_final: false,
            final_state: Tok::default(),
        }
    }

    /// Adds `term` unless already present; true if it was new.
    fn insert(&mut self, term: &str) -> bool {
        let arena = &self.arena;
        let found = self
            .sorted
            .binary_search_by(|&(a, b)| cmp_utf16(&arena[a as usize..b as usize], term));
        match found {
            Ok(_) => false,
            Err(at) => {
                let start = self.arena.len() as u32;
                self.arena.push_str(term);
                self.sorted.insert(at, (start, self.arena.len() as u32));
                true
            }
        }
    }
}

pub(crate) fn cmp_utf16(a: &str, b: &str) -> std::cmp::Ordering {
    if a.is_ascii() && b.is_ascii() {
        return a.cmp(b);
    }
    a.encode_utf16().cmp(b.encode_utf16())
}

impl Filter for Fingerprint {
    fn reset(&mut self) {
        self.ended = false;
        self.arena.clear();
        self.sorted.clear();
        self.has_final = false;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if self.ended {
            return false;
        }
        self.arena.clear();
        self.sorted.clear();
        let mut size = 0usize;
        while input.next(t) {
            if size > self.max_output_size {
                continue;
            }
            let had = !self.sorted.is_empty();
            if self.insert(&t.term) {
                if had {
                    size += 1;
                }
                size += utf16_len(&t.term);
            }
        }
        t.clear_for_end();
        input.end(t);
        self.ended = true;
        let end = t.end;
        let mut ok = true;
        t.term.clear();
        t.start = 0;
        t.end = end;
        t.pos_len = 1;
        t.pos_inc = 1;
        t.kind = "fingerprint";
        if self.sorted.is_empty() || size > self.max_output_size {
            ok = false;
        } else {
            for &(a, b) in self.sorted.iter() {
                // Lucene separates only once the output is non-empty, so an empty term (which
                // sorts first) leaves no trace.
                if !t.term.is_empty() {
                    t.term.push(self.separator);
                }
                t.term.push_str(&self.arena[a as usize..b as usize]);
            }
        }
        self.arena.clear();
        self.sorted.clear();
        self.final_state.clone_from(t);
        self.has_final = true;
        ok
    }

    fn end(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        if !self.ended {
            input.end(t);
            self.ended = true;
        }
        if self.has_final {
            t.clone_from(&self.final_state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::Map;
    use crate::filter::tests::{run_filters, terms};

    fn words(w: &[&str], ic: bool) -> Arc<WordSet> {
        Arc::new(WordSet::new(w, ic))
    }

    #[test]
    fn stop_and_positions() {
        let out = run_filters(
            vec![Box::new(stop(words(&["the", "a"], false)))],
            "the quick a fox the",
        );
        assert_eq!(
            out,
            vec![("quick".into(), 2, 4, 9), ("fox".into(), 2, 12, 15)]
        );
        let out = run_filters(
            vec![Box::new(SuggestStop::new(words(&["the"], false)))],
            "go the",
        );
        assert_eq!(
            out.iter().map(|t| t.0.as_str()).collect::<Vec<_>>(),
            vec!["go", "the"]
        );
        let out = run_filters(
            vec![Box::new(SuggestStop::new(words(&["the"], false)))],
            "go the ",
        );
        assert_eq!(
            out.iter().map(|t| t.0.as_str()).collect::<Vec<_>>(),
            vec!["go"]
        );
    }

    #[test]
    fn small_filters() {
        assert_eq!(
            terms(
                vec![Box::new(Filtering::new(Length { min: 2, max: 3 }))],
                "a ab abc abcd"
            ),
            vec!["ab", "abc"]
        );
        assert_eq!(
            terms(vec![Box::new(Limit::new(2, false))], "a b c"),
            vec!["a", "b"]
        );
        assert_eq!(
            terms(
                vec![Box::new(Map(|t: &mut Tok| truncate(t, 3)))],
                "abcdef ab 😀😀"
            ),
            // Three UTF-16 units end inside the second emoji: Java keeps its high surrogate,
            // which Elasticsearch renders (and indexes) as U+FFFD.
            vec!["abc", "ab", "😀\u{FFFD}"]
        );
        let mut s = String::new();
        assert_eq!(
            terms(
                vec![Box::new(Map(move |t: &mut Tok| reverse(t, &mut s)))],
                "abc a😀b"
            ),
            vec!["cba", "b😀a"]
        );
        assert_eq!(
            terms(
                vec![Box::new(Filtering::new(Unique::new(false)))],
                "a b a c b"
            ),
            vec!["a", "b", "c"]
        );
        assert_eq!(
            terms(vec![Box::new(KeywordRepeat::new())], "a b"),
            vec!["a", "a", "b", "b"]
        );
        assert_eq!(
            terms(vec![Box::new(Map(apostrophe))], "Türkiye'de it’s"),
            vec!["Türkiye", "it"]
        );
        let el = Elision {
            articles: words(&["l", "j"], true),
            scratch: String::new(),
        };
        assert_eq!(
            terms(vec![Box::new(el)], "l'avion J’ai d'abord"),
            vec!["avion", "ai", "d'abord"]
        );
        let mut s = String::new();
        assert_eq!(
            terms(
                vec![Box::new(Map(move |t: &mut Tok| decimal_digit(t, &mut s)))],
                "١٢٣ x𝟘"
            ),
            vec!["123", "x0"]
        );
        let mut fp = vec![Box::new(Fingerprint::new(255, ' ')) as Box<dyn Filter>];
        assert_eq!(terms(std::mem::take(&mut fp), "b a c a"), vec!["a b c"]);
    }

    #[test]
    fn payloads() {
        assert_eq!(java_parse_float("1.5"), Some(1.5));
        assert_eq!(java_parse_float(" 2f "), Some(2.0));
        assert_eq!(java_parse_float("inf"), None);
        assert_eq!(java_parse_float("-Infinity"), Some(f32::NEG_INFINITY));
        assert_eq!(lucene_parse_int("-12"), Some(-12));
        assert_eq!(lucene_parse_int("+1"), None);
        let out = {
            let tk: Box<dyn crate::stream::Tokenizer> =
                Box::new(crate::tokenizer::CharTokenizer::new(
                    Arc::new(crate::tokenizer::Whitespace),
                    255,
                    false,
                ));
            let mut chain = crate::stream::Chain::new(
                Vec::new(),
                tk,
                vec![Box::new(DelimitedPayload {
                    delimiter: '|',
                    encoding: PayloadEncoding::Float,
                })],
            );
            let mut v = Vec::new();
            chain
                .run("a|1.0 b", 1, &mut |_, _| {}, &mut |t| {
                    v.push((t.term.clone(), t.payload().map(<[u8]>::to_vec)))
                })
                .unwrap();
            v
        };
        assert_eq!(
            out,
            vec![
                ("a".into(), Some(vec![0x3f, 0x80, 0, 0])),
                ("b".into(), None)
            ]
        );
    }
}
