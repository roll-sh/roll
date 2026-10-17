//! Text tokenization (`tokenize.c`, `toksep.h`, `stopwords.c`, `stemmer.c`): separators,
//! escapes, Unicode lowercasing, stopwords, Snowball stems (prefixed `+`) and phonetic codes.

use std::collections::BTreeSet;

/// `ToksepMap_g`.
pub const fn is_sep(c: u8) -> bool {
    matches!(
        c,
        b' ' | b'\t'
            | b','
            | b'.'
            | b'/'
            | b'('
            | b')'
            | b'{'
            | b'}'
            | b'['
            | b']'
            | b':'
            | b';'
            | b'~'
            | b'!'
            | b'@'
            | b'#'
            | b'$'
            | b'%'
            | b'^'
            | b'&'
            | b'*'
            | b'-'
            | b'='
            | b'+'
            | b'|'
            | b'\''
            | b'`'
            | b'"'
            | b'<'
            | b'>'
            | b'?'
    )
}

pub const STEM_PREFIX: u8 = b'+';
pub const PHONETIC_PREFIX: u8 = b'<';
pub const SYNONYM_PREFIX: u8 = b'~';

/// `unicode_tolower`: ASCII stops at an embedded NUL; anything else goes through the Unicode
/// lowercase mapping, code point by code point (stopping at U+0000).
pub fn lowercase(s: &[u8]) -> Vec<u8> {
    let ascii = s.iter().position(|&c| c == 0 || c >= 0x80).unwrap_or(s.len());
    if ascii == s.len() || s[ascii] == 0 {
        let mut out = s.to_vec();
        out[..ascii].make_ascii_lowercase();
        if ascii > 0 {
            out.truncate(ascii);
        }
        return out;
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let (c, n) = utf8_read(&s[i..]);
        i += n;
        if c == 0 {
            break;
        }
        match char::from_u32(c) {
            Some(ch) => {
                for l in ch.to_lowercase() {
                    utf8_write(l as u32, &mut out);
                }
            }
            None => utf8_write(c, &mut out),
        }
    }
    out
}

/// `nu_utf8_write`.
pub fn utf8_write(c: u32, out: &mut Vec<u8>) {
    if c < 0x80 {
        out.push(c as u8);
    } else if c < 0x800 {
        out.extend_from_slice(&[0xc0 | (c >> 6) as u8, 0x80 | (c & 0x3f) as u8]);
    } else if c < 0x10000 {
        out.extend_from_slice(&[0xe0 | (c >> 12) as u8, 0x80 | ((c >> 6) & 0x3f) as u8, 0x80 | (c & 0x3f) as u8]);
    } else {
        out.extend_from_slice(&[
            0xf0 | ((c >> 18) & 0x07) as u8,
            0x80 | ((c >> 12) & 0x3f) as u8,
            0x80 | ((c >> 6) & 0x3f) as u8,
            0x80 | (c & 0x3f) as u8,
        ]);
    }
}

/// `nu_utf8_read`: decodes leniently (the lead byte decides the length).
pub fn utf8_read(s: &[u8]) -> (u32, usize) {
    let b0 = s[0] as u32;
    let get = |i: usize| s.get(i).map(|&b| (b & 0x3f) as u32).unwrap_or(0);
    if b0 < 0x80 {
        (b0, 1)
    } else if b0 < 0xe0 {
        (((b0 & 0x1f) << 6) | get(1), 2.min(s.len()))
    } else if b0 < 0xf0 {
        (((b0 & 0x0f) << 12) | (get(1) << 6) | get(2), 3.min(s.len()))
    } else {
        (((b0 & 0x07) << 18) | (get(1) << 12) | (get(2) << 6) | get(3), 4.min(s.len()))
    }
}

/// A stopword list (`StopWordList`), kept sorted as the trie iterates it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StopWords(pub BTreeSet<Vec<u8>>);

pub const DEFAULT_STOPWORDS: &[&str] = &[
    "a", "is", "the", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "it", "no", "not", "of", "on", "or",
    "such", "that", "their", "then", "there", "these", "they", "this", "to", "was", "will", "with",
];

pub const MAX_STOPWORDS: usize = 1024;

impl StopWords {
    pub fn default_list() -> Self {
        StopWords::from_words(DEFAULT_STOPWORDS.iter().map(|w| w.as_bytes()))
    }

    /// `NewStopWordListAC`: at most 1024 words, each lowercased.
    pub fn from_words<'a>(words: impl IntoIterator<Item = &'a [u8]>) -> Self {
        StopWords(words.into_iter().take(MAX_STOPWORDS).map(|w| lowercase(crate::util::cstr(w))).collect())
    }

    /// `StopWordList_Contains`.
    pub fn contains(&self, term: &[u8]) -> bool {
        !self.0.is_empty() && self.0.contains(&lowercase(term))
    }
}

/// A token (`Token`).
#[derive(Clone, Debug, Default)]
pub struct Token {
    /// The normalized text.
    pub tok: Vec<u8>,
    /// Byte range of the raw token in the input.
    pub raw_start: usize,
    pub raw_len: usize,
    pub pos: u32,
    /// `+stem`, when stemming changed the word.
    pub stem: Option<Vec<u8>>,
    /// `<code`, the primary double-metaphone code.
    pub phonetic: Option<Vec<u8>>,
    /// The token was lowercased into a new buffer (`Token_CopyRaw`).
    pub copied: bool,
}

pub const NOSTEM: u32 = 0x01;
pub const PHONETICS: u32 = 0x08;

/// `simpleTokenizer`.
pub struct Tokenizer<'a> {
    text: Vec<u8>,
    pos: Option<usize>,
    empty_input: bool,
    pub last_offset: u32,
    options: u32,
    stopwords: &'a StopWords,
    stemmer: Option<&'static str>,
    min_stem_len: usize,
    min_phonetic_len: usize,
}

/// The Snowball algorithm a language stems with (`RSLanguage_ToSnowballStemmer`).
pub fn stemmer_for(lang: usize) -> Option<&'static str> {
    let name = super::spec::LANGUAGES.get(lang)?;
    let name = if *name == "malay" { "indonesian" } else { name };
    snowball::has(name).then_some(name)
}

/// `__sbstemmer_Stem`: `+stem`, unless it equals the word (ignoring ASCII case).
pub fn stem(lang: &str, word: &[u8]) -> Option<Vec<u8>> {
    let w = std::str::from_utf8(word).ok()?;
    let s = snowball::stem(lang, w)?;
    if s.len() == word.len() && s.as_bytes().eq_ignore_ascii_case(word) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() + 1);
    out.push(STEM_PREFIX);
    out.extend_from_slice(s.as_bytes());
    Some(out)
}

impl<'a> Tokenizer<'a> {
    pub fn new(stopwords: &'a StopWords, stemmer: Option<&'static str>, cfg: &crate::config::Config) -> Self {
        Self::with(stopwords, stemmer, cfg.num("search-min-stem-len") as usize, cfg.num("search-min-phonetic-term-len") as usize)
    }

    pub fn with(stopwords: &'a StopWords, stemmer: Option<&'static str>, min_stem_len: usize, min_phonetic_len: usize) -> Self {
        Tokenizer { text: Vec::new(), pos: None, empty_input: true, last_offset: 0, options: 0, stopwords, stemmer, min_stem_len, min_phonetic_len }
    }

    /// `simpleTokenizer_Start` (positions continue across starts).
    pub fn start(&mut self, text: &[u8], options: u32) {
        self.text = text.to_vec();
        self.options = options;
        self.empty_input = text.is_empty();
        self.pos = Some(0);
    }

    /// `toksep`: the next raw token, `pos` moved past its separator (None after the last).
    fn toksep(&mut self) -> Option<(usize, usize)> {
        let start = self.pos?;
        let t = &self.text;
        let mut escaped = false;
        let mut i = start;
        while i < t.len() && t[i] != 0 {
            if is_sep(t[i]) && !escaped {
                let len = i - start;
                i += 1;
                self.pos = if i < t.len() && t[i] != 0 { Some(i) } else { None };
                return Some((start, len));
            }
            escaped = !escaped && t[i] == b'\\';
            i += 1;
        }
        self.pos = None;
        Some((start, i - start))
    }

    pub fn next_token(&mut self) -> Option<Token> {
        while self.pos.is_some() {
            let (start, len) = self.toksep()?;
            let raw = &self.text[start..start + len];
            let (normalized, unescaped, copied) = normalize(raw);
            if normalized.is_empty() && !self.empty_input {
                continue;
            }
            if !self.empty_input && self.stopwords.contains(&normalized) {
                continue;
            }
            self.last_offset += 1;
            let mut t = Token { tok: normalized, raw_start: start, raw_len: len, pos: self.last_offset, copied, ..Default::default() };
            if self.options & NOSTEM == 0
                && let Some(lang) = self.stemmer
                && t.tok.len() >= self.min_stem_len
            {
                t.stem = stem(lang, &t.tok);
            }
            if self.options & PHONETICS != 0 && t.tok.len() >= self.min_phonetic_len {
                // Phonetics read the token's buffer after in-place normalization: the lowercase
                // text, or (when lowercasing grew it) the unescaped text and whatever follows.
                let src = if copied {
                    let mut b = unescaped;
                    b.extend_from_slice(&self.text[(start + b.len()).min(self.text.len())..]);
                    b.truncate(t.tok.len());
                    b
                } else {
                    t.tok.clone()
                };
                t.phonetic = super::phonetic::primary(crate::util::cstr(&src));
            }
            return Some(t);
        }
        None
    }
}

/// `DefaultNormalize`: unescaped blanks and control characters dropped, backslash escapes
/// removed, then lowercased. Returns the unescaped text too, and whether lowercasing grew
/// the token (`Token_CopyRaw`).
pub fn normalize(s: &[u8]) -> (Vec<u8>, Vec<u8>, bool) {
    let mut dst = Vec::with_capacity(s.len());
    let mut escaped = false;
    for &c in s {
        let blank = c == b' ' || c == b'\t';
        let cntrl = c < 0x20 || c == 0x7f;
        if (blank && !escaped) || cntrl {
        } else if c == b'\\' && !escaped {
            escaped = true;
            continue;
        } else {
            dst.push(c);
        }
        escaped = false;
    }
    let lower = lowercase(&dst);
    let grew = lower.len() > dst.len();
    (lower, dst, grew)
}
