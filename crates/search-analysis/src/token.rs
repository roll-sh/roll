//! Tokens: the mutable attribute record a chain passes along, and the borrowed view callers see.
//!
//! Lucene threads one set of attribute objects through every stage of a token stream and each
//! stage overwrites them in place; [`Tok`] is that record. It is owned by the chain and reused for
//! every token, so the term buffer keeps its capacity and producing a token allocates nothing.
//! Filters that need lookahead (synonyms, shingles, word delimiters) keep pools of `Tok`s and copy
//! with `clone_from`, which also reuses capacity.
//!
//! Offsets are in UTF-16 code units, as in Lucene (and therefore in every Elasticsearch API that
//! reports them: `_analyze`, term vectors, highlighting offsets).

/// Optional attributes a stage of a chain carries, which `_analyze` with `explain` reports.
pub(crate) mod attr {
    /// `KeywordAttribute` (`keyword`): stemmers, `keyword_marker`, `keyword_repeat` …
    pub const KEYWORD: u32 = 1;
    /// `PayloadAttribute` (`payload`).
    pub const PAYLOAD: u32 = 2;
    /// `FlagsAttribute` (`flags`).
    pub const FLAGS: u32 = 4;
    /// `TermFrequencyAttribute` (`termFrequency`): part of Lucene's default packed attributes,
    /// so every tokenizer has it except one built with the plain attribute factory
    /// (`char_group`).
    pub const TERM_FREQ: u32 = 8;
}

/// The default token type (`TypeAttribute.DEFAULT_TYPE`).
pub(crate) const WORD: &str = "word";

/// One token's attributes.
#[derive(Debug)]
pub(crate) struct Tok {
    pub term: String,
    /// Start offset in UTF-16 units of the original text (after offset correction).
    pub start: u32,
    pub end: u32,
    pub pos_inc: u32,
    pub pos_len: u32,
    pub kind: &'static str,
    pub keyword: bool,
    pub flags: u32,
    pub payload: Vec<u8>,
    pub has_payload: bool,
    pub term_freq: u32,
    /// Set by a filter that fails at analysis time (an unparsable payload, a script error); the
    /// filter then ends the stream and the chain reports the error.
    pub error: Option<Box<crate::error::AnalysisError>>,
}

impl Default for Tok {
    fn default() -> Self {
        Tok {
            term: String::new(),
            start: 0,
            end: 0,
            pos_inc: 1,
            pos_len: 1,
            kind: WORD,
            keyword: false,
            flags: 0,
            payload: Vec::new(),
            has_payload: false,
            term_freq: 1,
            error: None,
        }
    }
}

impl Clone for Tok {
    fn clone(&self) -> Self {
        let mut t = Tok::default();
        t.clone_from(self);
        t
    }

    fn clone_from(&mut self, o: &Self) {
        self.term.clone_from(&o.term);
        self.start = o.start;
        self.end = o.end;
        self.pos_inc = o.pos_inc;
        self.pos_len = o.pos_len;
        self.kind = o.kind;
        self.keyword = o.keyword;
        self.flags = o.flags;
        self.has_payload = o.has_payload;
        if o.has_payload {
            self.payload.clone_from(&o.payload);
        } else {
            self.payload.clear();
        }
        self.term_freq = o.term_freq;
    }
}

impl Tok {
    /// `AttributeSource.clearAttributes()`: every attribute back to its default.
    #[inline]
    pub fn clear(&mut self) {
        self.term.clear();
        self.start = 0;
        self.end = 0;
        self.pos_inc = 1;
        self.pos_len = 1;
        self.kind = WORD;
        self.keyword = false;
        self.flags = 0;
        self.has_payload = false;
        self.payload.clear();
        self.term_freq = 1;
    }

    /// `AttributeSource.endAttributes()`: what `end()` starts from (the position increment is
    /// zero so trailing holes can be added).
    #[inline]
    pub fn clear_for_end(&mut self) {
        self.clear();
        self.pos_inc = 0;
    }

    pub fn payload(&self) -> Option<&[u8]> {
        self.has_payload.then_some(&self.payload[..])
    }
}

/// A token as callers of [`crate::Analyzer::analyze`] see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token<'a> {
    pub term: &'a str,
    /// Offsets in UTF-16 code units of the analyzed text (Lucene's offsets).
    pub start_offset: u32,
    pub end_offset: u32,
    /// Absolute position (position increments summed, plus the position increment gap between
    /// values for [`crate::Analyzer::analyze_values`]).
    pub position: u32,
    /// The increment from the previous token (0 for a token stacked on the previous one).
    pub position_increment: u32,
    /// How many positions the token spans in a token graph (1 for ordinary tokens).
    pub position_length: u32,
    /// The token type (`<ALPHANUM>`, `word`, `SYNONYM`, `shingle` …).
    pub kind: &'a str,
    /// Whether a filter marked the token as a keyword (protected from stemming).
    pub keyword: bool,
    pub payload: Option<&'a [u8]>,
}
