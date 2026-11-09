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

/// The Snowball algorithm a language stems with (`RSLanguage_ToSnowballStemmer`): libstemmer's,
/// and the Tagalog one RediSearch adds.
pub fn stemmer_for(lang: usize) -> Option<&'static str> {
    let name = super::spec::LANGUAGES.get(lang)?;
    let name = if *name == "malay" { "indonesian" } else { name };
    (snowball::has(name) || name == "tagalog").then_some(name)
}

/// `sb_stemmer_stem` with a [`stemmer_for`] algorithm.
pub fn snowball_stem(lang: &str, word: &str) -> Option<String> {
    if lang == "tagalog" {
        return Some(super::tagalog::stem(word));
    }
    snowball::stem(lang, word)
}

/// `__sbstemmer_Stem`: `+stem`, unless it equals the word (ignoring ASCII case).
pub fn stem(lang: &str, word: &[u8]) -> Option<Vec<u8>> {
    let w = std::str::from_utf8(word).ok()?;
    let s = snowball_stem(lang, w)?;
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

/// `CNTOKENIZE_BUF_MAX`.
const CN_ESCBUF_MAX: usize = 256;

/// `cnTokenizer` (`tokenize_cn.c`): friso's words as they come (no lowercasing beyond
/// friso's own, no stemming, phonetics or stopwords — its stopword list is never set), its
/// punctuation, unknown characters, synonyms and English-punctuation words skipped, and
/// backslash escapes joining words (up to 256 bytes).
pub struct CnTokenizer {
    friso: &'static super::friso::Friso,
    task: super::friso::Task<'static>,
    text: Vec<u8>,
    escbuf: Vec<u8>,
    pub last_offset: u32,
}

impl CnTokenizer {
    pub fn new(cfg: &crate::config::Config) -> Self {
        super::friso::configure(cfg);
        Self::with(super::friso::global())
    }

    pub fn with(friso: &'static super::friso::Friso) -> Self {
        CnTokenizer { friso, task: Default::default(), text: Vec::new(), escbuf: Vec::new(), last_offset: 0 }
    }

    /// `cnTokenizer_Start`.
    pub fn start(&mut self, text: &[u8]) {
        self.text = text.to_vec();
        self.task.set_text(text);
        self.escbuf.clear();
    }

    fn at(&self, i: usize) -> u8 {
        self.text.get(i).copied().unwrap_or(0)
    }

    /// `appendToEscbuf`: whether all of `s` fit.
    fn append(&mut self, s: &[u8]) -> bool {
        let n = s.len().min(CN_ESCBUF_MAX - self.escbuf.len());
        self.escbuf.extend_from_slice(&s[..n]);
        n == s.len()
    }

    /// `appendEscapedChars`: the escaped character goes in; whether the word goes on.
    fn append_escaped(&mut self, tok_end: usize, next: bool) -> bool {
        let (at, skip) = if next { (tok_end + 1, 2) } else { (tok_end, 1) };
        let c = self.at(at);
        if self.append(&[c]) {
            self.task.idx = self.task.idx.wrapping_add(skip);
            if (self.task.idx as usize) < self.text.len() && !is_sep(self.at(self.task.idx as usize)) {
                return true;
            }
        }
        false
    }

    /// `cnTokenizer_Next`.
    pub fn next_token(&mut self) -> Option<Token> {
        use super::friso::{CJK_UNITS, ENPUN_WORDS, NCSYN_WORDS, PUNC_WORDS, STOPWORDS};
        let mut use_esc = false;
        let mut tok: Option<Token> = None;
        self.escbuf.clear();
        let done = |t: Option<Token>, s: &Self, esc: bool, word: &[u8]| -> Option<Token> {
            let mut t = t?;
            t.tok = if esc { s.escbuf.clone() } else { word.to_vec() };
            Some(t)
        };
        loop {
            if self.friso.next_token(&mut self.task).is_none() {
                return if use_esc { done(tok, self, true, &[]) } else { None };
            }
            let ft = self.task.token.clone();
            let offset = ft.offset as u32 as usize;
            match ft.ty {
                STOPWORDS | ENPUN_WORDS | CJK_UNITS | NCSYN_WORDS => continue,
                PUNC_WORDS => {
                    if ft.word.first() != Some(&b'\\') || !is_sep(self.at(offset + 1)) {
                        continue;
                    }
                    // An escaped separator: it joins the word (when more follows), else the
                    // backslash is a word of its own.
                    if self.append_escaped(offset + ft.length as usize, false) {
                        if tok.is_none() {
                            tok = Some(self.init_token(&ft));
                        }
                        use_esc = true;
                        continue;
                    }
                }
                _ => {}
            }
            match tok.as_mut() {
                None => tok = Some(self.init_token(&ft)),
                Some(t) => t.raw_len = self.text.len() - t.raw_start,
            }
            let end = offset + ft.rlen as usize;
            if self.at(end) == b'\\' && is_sep(self.at(end + 1)) {
                // A trailing escape: the word goes on past the separator.
                use_esc = true;
                if !self.append(&ft.word) || !self.append_escaped(offset + ft.length as usize, true) {
                    return done(tok, self, true, &[]);
                }
                continue;
            }
            if use_esc {
                self.append(&ft.word);
            }
            return done(tok, self, use_esc, &ft.word);
        }
    }

    /// `initToken`.
    fn init_token(&mut self, ft: &super::friso::Token) -> Token {
        self.last_offset += 1;
        Token { tok: Vec::new(), raw_start: ft.offset as u32 as usize, raw_len: ft.rlen as usize, pos: self.last_offset, copied: true, ..Default::default() }
    }
}

/// The tokenizer a document's (or a query's) language picks (`GetTokenizer`).
pub enum AnyTokenizer<'a> {
    Simple(Tokenizer<'a>),
    Chinese(Box<CnTokenizer>),
}

impl AnyTokenizer<'_> {
    pub fn start(&mut self, text: &[u8], options: u32) {
        match self {
            AnyTokenizer::Simple(t) => t.start(text, options),
            AnyTokenizer::Chinese(t) => t.start(text),
        }
    }

    pub fn next_token(&mut self) -> Option<Token> {
        match self {
            AnyTokenizer::Simple(t) => t.next_token(),
            AnyTokenizer::Chinese(t) => t.next_token(),
        }
    }

    pub fn last_offset(&self) -> u32 {
        match self {
            AnyTokenizer::Simple(t) => t.last_offset,
            AnyTokenizer::Chinese(t) => t.last_offset,
        }
    }

    pub fn last_offset_mut(&mut self) -> &mut u32 {
        match self {
            AnyTokenizer::Simple(t) => &mut t.last_offset,
            AnyTokenizer::Chinese(t) => &mut t.last_offset,
        }
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
