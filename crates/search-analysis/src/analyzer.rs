//! Analyzers and normalizers: named, shareable analysis chains.
//!
//! An [`Analyzer`] is an `Arc` around its components (char filters, tokenizer, token filters)
//! and a pool of chain instances. `analyze` borrows an instance from the pool, so concurrent
//! callers never contend on a chain and a chain's buffers are reused across documents: after
//! warm-up, analysis allocates nothing per token or per document. Built-in Lucene analyzers
//! (`standard`, `english` …) are composed from the same components but report as non-custom in
//! `_analyze` explain, as Elasticsearch does.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use crate::component::{AnalysisMode, NamedCharFilter, NamedFilter, NamedTokenizer};
use crate::error::AnalysisError;
use crate::stream::{Chain, End};
use crate::token::{Tok, Token, attr};

pub(crate) struct AnalyzerInner {
    pub name: String,
    pub char_filters: Vec<NamedCharFilter>,
    pub tokenizer: NamedTokenizer,
    pub filters: Vec<NamedFilter>,
    /// The components `normalize` applies (Lucene's `Analyzer.normalize` chain): the char
    /// filters and token filters that are normalizing, over the whole text as one token.
    pub norm_char_filters: Vec<NamedCharFilter>,
    pub norm_filters: Vec<NamedFilter>,
    /// Whether `_analyze` explain breaks it into stages (`custom_analyzer: true`).
    pub custom: bool,
    pub pos_gap: i32,
    pub offset_gap: u32,
    /// The gap `_analyze` explain uses between values: Elasticsearch reads it from the
    /// unwrapped analyzer, which for inline `_analyze` chains and normalizers is 0.
    pub explain_pos_gap: i32,
    pub mode: AnalysisMode,
    /// The name `_analyze` explain reports (Elasticsearch reports null for analyzers it built
    /// on the fly without naming them).
    pub explain_name: Option<String>,
    pool: Mutex<Vec<Chain>>,
    norm_pool: Mutex<Vec<Chain>>,
}

impl AnalyzerInner {
    fn new_chain(&self) -> Chain {
        Chain::new(
            self.char_filters
                .iter()
                .map(|c| c.factory.create())
                .collect(),
            self.tokenizer.factory.create(),
            self.filters.iter().map(|f| f.factory.create()).collect(),
        )
    }

    fn new_norm_chain(&self) -> Chain {
        Chain::new(
            self.norm_char_filters
                .iter()
                .map(|c| c.factory.create())
                .collect(),
            Box::new(crate::tokenizer::KeywordTokenizer::new(256)),
            self.norm_filters
                .iter()
                .map(|f| f.factory.create_normalizer())
                .collect(),
        )
    }

    /// Attributes present after the tokenizer and the first `n` filters.
    fn attrs_upto(&self, n: usize) -> u32 {
        self.filters[..n]
            .iter()
            .fold(self.tokenizer.factory.attrs(), |a, f| a | f.factory.attrs())
    }
}

/// Builder for [`Analyzer`].
pub(crate) struct AnalyzerSpec {
    pub name: String,
    pub char_filters: Vec<NamedCharFilter>,
    pub tokenizer: NamedTokenizer,
    pub filters: Vec<NamedFilter>,
    pub custom: bool,
    pub pos_gap: i32,
    pub offset_gap: u32,
    /// For built-in analyzers: the filters `normalize` applies (custom analyzers derive them).
    pub norm_filters: Option<Vec<NamedFilter>>,
}

impl AnalyzerSpec {
    pub fn build(self) -> std::result::Result<Analyzer, AnalysisError> {
        let gap = self.pos_gap;
        self.build_with_explain_gap(gap)
    }

    pub fn build_with_explain_gap(
        self,
        explain_pos_gap: i32,
    ) -> std::result::Result<Analyzer, AnalysisError> {
        self.tokenizer.factory.check()?;
        for f in &self.filters {
            f.factory.check()?;
        }
        let mut mode = AnalysisMode::All;
        for f in &self.filters {
            mode = mode.merge(f.factory.mode()).map_err(|_| {
                AnalysisError::other(
                    "illegal_state_exception",
                    500,
                    "Cannot merge SEARCH_TIME and INDEX_TIME analysis mode.",
                )
            })?;
        }
        let norm_filters = match self.norm_filters {
            Some(f) => f,
            None => self
                .filters
                .iter()
                .filter(|f| f.factory.normalizing())
                .cloned()
                .collect(),
        };
        let norm_char_filters = self
            .char_filters
            .iter()
            .filter(|c| c.factory.normalizing())
            .cloned()
            .collect();
        Ok(Analyzer {
            inner: Arc::new(AnalyzerInner {
                explain_name: Some(self.name.clone()),
                name: self.name,
                char_filters: self.char_filters,
                tokenizer: self.tokenizer,
                filters: self.filters,
                norm_char_filters,
                norm_filters,
                custom: self.custom,
                pos_gap: self.pos_gap,
                offset_gap: self.offset_gap,
                explain_pos_gap,
                mode,
                pool: Mutex::new(Vec::new()),
                norm_pool: Mutex::new(Vec::new()),
            }),
        })
    }
}

/// A named analysis chain. Cheap to clone; `Send + Sync`.
#[derive(Clone)]
pub struct Analyzer {
    pub(crate) inner: Arc<AnalyzerInner>,
}

impl std::fmt::Debug for Analyzer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Analyzer({})", self.inner.name)
    }
}

/// `FixedShingleFilter`'s size check.
fn check_shingle_size(size: usize) -> Result<(), AnalysisError> {
    let max = crate::filter::fixed_shingle::MAX_SHINGLE_SIZE;
    if size <= 1 || size > max {
        return Err(AnalysisError::illegal(format!(
            "Shingle size must be between 2 and {max}, got {size}"
        )));
    }
    Ok(())
}

/// Runs `f` with a pooled chain.
fn with_pool<R>(
    pool: &Mutex<Vec<Chain>>,
    make: impl FnOnce() -> Chain,
    f: impl FnOnce(&mut Chain) -> R,
) -> R {
    let pooled = pool.lock().unwrap_or_else(|e| e.into_inner()).pop();
    let mut chain = pooled.unwrap_or_else(make);
    let r = f(&mut chain);
    pool.lock().unwrap_or_else(|e| e.into_inner()).push(chain);
    r
}

/// Accumulates positions and offsets across stages and values, as `_analyze` and the indexing
/// chain do.
#[derive(Clone, Copy)]
struct Cursor {
    last_position: i64,
    last_offset: i64,
}

impl Cursor {
    fn new() -> Cursor {
        Cursor {
            last_position: -1,
            last_offset: 0,
        }
    }

    #[inline]
    fn token(&mut self, t: &Tok) -> (i64, i64, i64) {
        if t.pos_inc > 0 {
            self.last_position += t.pos_inc as i64;
        }
        (
            self.last_position,
            self.last_offset + t.start as i64,
            self.last_offset + t.end as i64,
        )
    }

    fn end(&mut self, end: End, pos_gap: i32, offset_gap: u32) {
        self.last_offset += end.final_offset as i64;
        self.last_position += end.pos_inc as i64;
        self.last_position += pos_gap as i64;
        self.last_offset += offset_gap as i64;
    }
}

fn clamp(v: i64) -> u32 {
    v.clamp(0, u32::MAX as i64) as u32
}

impl Analyzer {
    /// The same analyzer reported without a name in `_analyze` explain.
    pub(crate) fn anonymous(mut self) -> Analyzer {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.explain_name = None,
            None => {
                let i = &*self.inner;
                self.inner = Arc::new(AnalyzerInner {
                    name: i.name.clone(),
                    char_filters: i.char_filters.clone(),
                    tokenizer: i.tokenizer.clone(),
                    filters: i.filters.clone(),
                    norm_char_filters: i.norm_char_filters.clone(),
                    norm_filters: i.norm_filters.clone(),
                    custom: i.custom,
                    pos_gap: i.pos_gap,
                    offset_gap: i.offset_gap,
                    explain_pos_gap: i.explain_pos_gap,
                    mode: i.mode,
                    explain_name: None,
                    pool: Mutex::new(Vec::new()),
                    norm_pool: Mutex::new(Vec::new()),
                });
            }
        }
        self
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// This analyzer with more filters after its own (Elasticsearch's `AnalyzerWrapper`s that
    /// field mappers build), with its own position increment gap.
    fn wrapped(&self, extra: Vec<NamedFilter>, pos_gap: i32) -> Analyzer {
        let i = &*self.inner;
        let mut filters = i.filters.clone();
        filters.extend(extra);
        Analyzer {
            inner: Arc::new(AnalyzerInner {
                name: i.name.clone(),
                char_filters: i.char_filters.clone(),
                tokenizer: i.tokenizer.clone(),
                filters,
                norm_char_filters: i.norm_char_filters.clone(),
                norm_filters: i.norm_filters.clone(),
                custom: i.custom,
                pos_gap,
                offset_gap: i.offset_gap,
                explain_pos_gap: pos_gap,
                mode: i.mode,
                explain_name: i.explain_name.clone(),
                pool: Mutex::new(Vec::new()),
                norm_pool: Mutex::new(Vec::new()),
            }),
        }
    }

    /// The gap of the Lucene analyzer itself, which `search_as_you_type` subfields use (their
    /// wrapper delegates to it rather than to the named analyzer): a custom analyzer's
    /// `position_increment_gap`, 0 for Lucene's built-in analyzers.
    fn raw_position_increment_gap(&self) -> i32 {
        if self.inner.custom {
            self.inner.pos_gap
        } else {
            0
        }
    }

    /// The analyzer of a text field's `._index_prefix` subfield (`index_prefixes`, Elasticsearch's
    /// `PrefixWrappedAnalyzer`): every token's edge n-grams of `min_chars..=max_chars` code
    /// points (the token itself only when it is that short). `position_increment_gap` is the
    /// field's.
    pub fn with_index_prefixes(
        &self,
        min_chars: usize,
        max_chars: usize,
        position_increment_gap: i32,
    ) -> Analyzer {
        let f = crate::component::filter_fn(move || {
            Box::new(crate::filter::ngram::NGram::new(
                min_chars, max_chars, true, false,
            ))
        });
        self.wrapped(
            vec![NamedFilter {
                name: "index_prefixes".into(),
                factory: f,
            }],
            position_increment_gap,
        )
    }

    /// The analyzer of a text field's `._index_phrase` subfield (`index_phrases`,
    /// `PhraseWrappedAnalyzer`): two-token shingles (`FixedShingleFilter`, `_` for gaps).
    /// `position_increment_gap` is the field's.
    pub fn with_index_phrases(&self, position_increment_gap: i32) -> Analyzer {
        let f = crate::component::filter_fn(|| {
            Box::new(crate::filter::fixed_shingle::FixedShingle::new(2, " ", "_"))
        });
        self.wrapped(
            vec![NamedFilter {
                name: "index_phrases".into(),
                factory: f,
            }],
            position_increment_gap,
        )
    }

    /// The analyzer of a `search_as_you_type` field's `._<n>gram` subfield
    /// (`SearchAsYouTypeAnalyzer.withShingle`): shingles of exactly `shingle_size` tokens, empty
    /// fillers for gaps. Sizes are 2 to 4.
    pub fn with_shingles(&self, shingle_size: usize) -> Result<Analyzer, AnalysisError> {
        check_shingle_size(shingle_size)?;
        let f = crate::component::filter_fn(move || {
            Box::new(crate::filter::fixed_shingle::FixedShingle::new(
                shingle_size,
                " ",
                "",
            ))
        });
        Ok(self.wrapped(
            vec![NamedFilter {
                name: "shingle".into(),
                factory: f,
            }],
            self.raw_position_increment_gap(),
        ))
    }

    /// The analyzer of a `search_as_you_type` field's `._index_prefix` subfield
    /// (`SearchAsYouTypeAnalyzer.withShingleAndPrefix`): `shingle_size` shingles including the
    /// shorter ones at the end of the text (trailing positions padded with empty fillers), then
    /// their edge n-grams of 1 to 20 code points plus the shingle itself.
    pub fn with_shingles_and_prefixes(
        &self,
        shingle_size: usize,
    ) -> Result<Analyzer, AnalysisError> {
        check_shingle_size(shingle_size)?;
        let extra = (shingle_size - 1) as u32;
        let trailing = crate::component::filter_fn(move || {
            Box::new(crate::filter::fixed_shingle::TrailingShingle { extra })
        });
        let shingles = crate::component::filter_fn(move || {
            Box::new(crate::filter::fixed_shingle::FixedShingle::new(
                shingle_size,
                " ",
                "",
            ))
        });
        let grams = crate::component::filter_fn(|| {
            Box::new(crate::filter::ngram::NGram::new(1, 20, true, true))
        });
        Ok(self.wrapped(
            vec![
                NamedFilter {
                    name: "trailing_shingle".into(),
                    factory: trailing,
                },
                NamedFilter {
                    name: "shingle".into(),
                    factory: shingles,
                },
                NamedFilter {
                    name: "edge_ngram".into(),
                    factory: grams,
                },
            ],
            self.raw_position_increment_gap(),
        ))
    }

    /// The position gap between values of a multi-valued field (100 unless configured).
    pub fn position_increment_gap(&self) -> i32 {
        self.inner.pos_gap
    }

    /// The offset gap between values (1 unless configured).
    pub fn offset_gap(&self) -> u32 {
        self.inner.offset_gap
    }

    pub fn analysis_mode(&self) -> AnalysisMode {
        self.inner.mode
    }

    /// Whether `_analyze` explain reports it stage by stage.
    pub fn is_custom(&self) -> bool {
        self.inner.custom
    }

    /// `NamedAnalyzer.checkAllowedInMode`: Elasticsearch's mapper error when a search-time-only
    /// analyzer (e.g. with updateable synonyms) is used to index.
    pub fn check_allowed_in_mode(&self, mode: AnalysisMode) -> Result<(), AnalysisError> {
        let own = self.inner.mode;
        if own == AnalysisMode::All || own == mode {
            return Ok(());
        }
        let offending: Vec<&str> = self
            .inner
            .filters
            .iter()
            .filter(|f| {
                let m = f.factory.mode();
                m != AnalysisMode::All && m != mode
            })
            .map(|f| &*f.name)
            .collect();
        Err(AnalysisError::other(
            "mapper_exception",
            500,
            format!(
                "analyzer [{}] contains filters [{}] that are not allowed to run in {} mode.",
                self.inner.name,
                offending.join(", "),
                mode.readable_name()
            ),
        ))
    }

    /// Analyzes one value. Positions start at 0; offsets are UTF-16 units of `text`.
    pub fn analyze(&self, text: &str, sink: &mut dyn FnMut(&Token)) {
        self.analyze_values(&[text], sink);
    }

    /// Analyzes the values of a multi-valued field: positions continue across values with
    /// [`Analyzer::position_increment_gap`] between them, offsets with the end offset of the
    /// previous value plus [`Analyzer::offset_gap`] (Lucene's indexing chain and `_analyze`).
    /// A filter failing at analysis time (see [`Analyzer::try_analyze_values`]) ends the output.
    pub fn analyze_values(&self, values: &[&str], sink: &mut dyn FnMut(&Token)) {
        let _ = self.try_analyze_values(values, sink);
    }

    /// [`Analyzer::analyze`], reporting analysis-time failures (an unparsable `delimited_payload`,
    /// a failing `condition` script) as Elasticsearch would when indexing the document.
    pub fn try_analyze(
        &self,
        text: &str,
        sink: &mut dyn FnMut(&Token),
    ) -> Result<(), AnalysisError> {
        self.try_analyze_values(&[text], sink)
    }

    /// [`Analyzer::analyze_values`], reporting analysis-time failures.
    pub fn try_analyze_values(
        &self,
        values: &[&str],
        sink: &mut dyn FnMut(&Token),
    ) -> Result<(), AnalysisError> {
        let inner = &*self.inner;
        with_pool(
            &inner.pool,
            || inner.new_chain(),
            |chain| {
                let mut cur = Cursor::new();
                let n = chain.filters.len();
                for text in values {
                    let end = chain.run(text, n, &mut |_, _| {}, &mut |t| {
                        let (pos, start, end) = cur.token(t);
                        sink(&Token {
                            term: &t.term,
                            start_offset: clamp(start),
                            end_offset: clamp(end),
                            position: clamp(pos),
                            position_increment: t.pos_inc,
                            position_length: t.pos_len,
                            kind: t.kind,
                            keyword: t.keyword,
                            payload: t.payload(),
                        })
                    })?;
                    cur.end(end, inner.pos_gap, inner.offset_gap);
                }
                Ok(())
            },
        )
    }

    /// Collects the terms of `text` (convenience for queries and tests).
    pub fn terms(&self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        self.analyze(text, &mut |t| out.push(t.term.to_string()));
        out
    }

    /// `Analyzer.normalize`: the normalizing char filters and token filters applied to the whole
    /// text as a single token (what multi-term queries such as `prefix` and `wildcard` use).
    pub fn normalize(&self, text: &str) -> String {
        let inner = &*self.inner;
        with_pool(
            &inner.norm_pool,
            || inner.new_norm_chain(),
            |chain| {
                let mut out: Option<String> = None;
                let n = chain.filters.len();
                let _ = chain.run(text, n, &mut |_, _| {}, &mut |t| {
                    if out.is_none() {
                        out = Some(t.term.clone());
                    }
                });
                out.unwrap_or_default()
            },
        )
    }

    /// `_analyze` with `explain: true`: the tokens after every stage (for custom analyzers) or of
    /// the whole analyzer, with the token attributes Lucene exposes. `attributes` limits the
    /// extended attributes reported (case-insensitive; empty means all).
    pub fn explain(&self, texts: &[&str], attributes: &[String]) -> Explain {
        self.explain_limited(texts, attributes, usize::MAX)
            .unwrap_or_else(|e| Explain {
                custom_analyzer: self.inner.custom,
                charfilters: Vec::new(),
                tokenizer: None,
                tokenfilters: Vec::new(),
                analyzer: Some(TokenList {
                    name: Some(e.reason),
                    tokens: Vec::new(),
                }),
            })
    }

    /// [`Analyzer::explain`] failing like Elasticsearch once any stage produces more than
    /// `max_tokens` tokens (`index.analyze.max_token_count`).
    pub fn explain_limited(
        &self,
        texts: &[&str],
        attributes: &[String],
        max_tokens: usize,
    ) -> Result<Explain, AnalysisError> {
        let include: Vec<String> = attributes.iter().map(|a| a.to_lowercase()).collect();
        let inner = &*self.inner;
        with_pool(
            &inner.pool,
            || inner.new_chain(),
            |chain| {
                let nf = chain.filters.len();
                if !inner.custom {
                    let attrs = inner.attrs_upto(nf);
                    let mut list = Vec::new();
                    let mut cur = Cursor::new();
                    for text in texts {
                        let mut err = None;
                        let end = chain.run(text, nf, &mut |_, _| {}, &mut |t| {
                            if list.len() >= max_tokens {
                                err.get_or_insert_with(|| too_many_tokens(max_tokens));
                                return;
                            }
                            list.push(explain_token(t, &mut cur, attrs, &include));
                        })?;
                        if let Some(e) = err {
                            return Err(e);
                        }
                        cur.end(end, inner.pos_gap, inner.offset_gap);
                    }
                    return Ok(Explain {
                        custom_analyzer: false,
                        charfilters: Vec::new(),
                        tokenizer: None,
                        tokenfilters: Vec::new(),
                        analyzer: Some(TokenList {
                            name: inner.explain_name.clone(),
                            tokens: list,
                        }),
                    });
                }
                let mut cf_texts: Vec<Vec<String>> = vec![Vec::new(); inner.char_filters.len()];
                // Stage 0 is the tokenizer, stage i + 1 the i-th filter.
                let mut stages: Vec<(Vec<ExplainToken>, Cursor)> =
                    vec![(Vec::new(), Cursor::new()); nf + 1];
                for text in texts {
                    for (stage, (list, cur)) in stages.iter_mut().enumerate() {
                        let attrs = inner.attrs_upto(stage);
                        let mut err = None;
                        let end = chain.run(
                            text,
                            stage,
                            &mut |i, s| {
                                if stage == 0 {
                                    cf_texts[i].push(s.to_string());
                                }
                            },
                            &mut |t| {
                                if list.len() >= max_tokens {
                                    err.get_or_insert_with(|| too_many_tokens(max_tokens));
                                    return;
                                }
                                list.push(explain_token(t, cur, attrs, &include));
                            },
                        )?;
                        if let Some(e) = err {
                            return Err(e);
                        }
                        cur.end(end, inner.explain_pos_gap, inner.offset_gap);
                    }
                }
                let mut stages = stages.into_iter().map(|(l, _)| l);
                let tokenizer = TokenList {
                    name: Some(inner.tokenizer.name.to_string()),
                    tokens: stages.next().unwrap_or_default(),
                };
                let tokenfilters = inner
                    .filters
                    .iter()
                    .zip(stages)
                    .map(|(f, tokens)| TokenList {
                        name: Some(f.name.to_string()),
                        tokens,
                    })
                    .collect();
                let charfilters = inner
                    .char_filters
                    .iter()
                    .zip(cf_texts)
                    .map(|(c, texts)| CharFilteredText {
                        name: c.name.to_string(),
                        filtered_text: texts,
                    })
                    .collect();
                Ok(Explain {
                    custom_analyzer: true,
                    charfilters,
                    tokenizer: Some(tokenizer),
                    tokenfilters,
                    analyzer: None,
                })
            },
        )
    }

    /// The UTF-16 length of `text` after the char filters (for `index.analyze.max_char_count`).
    pub(crate) fn char_filtered_utf16_len(&self, text: &str) -> Result<usize, AnalysisError> {
        if self.inner.char_filters.is_empty() {
            return Ok(crate::unicode::utf16_len(text));
        }
        let inner = &*self.inner;
        with_pool(
            &inner.pool,
            || inner.new_chain(),
            |chain| chain.filtered_utf16_len(text),
        )
    }

    /// Simple `_analyze` tokens (the `tokens` array), failing past `max_tokens`.
    pub(crate) fn simple_tokens(
        &self,
        texts: &[&str],
        max_tokens: usize,
    ) -> Result<Vec<ExplainToken>, AnalysisError> {
        let inner = &*self.inner;
        with_pool(
            &inner.pool,
            || inner.new_chain(),
            |chain| {
                let nf = chain.filters.len();
                let mut list = Vec::new();
                let mut cur = Cursor::new();
                for text in texts {
                    let mut err = None;
                    let end = chain.run(text, nf, &mut |_, _| {}, &mut |t| {
                        if list.len() >= max_tokens {
                            err.get_or_insert_with(|| too_many_tokens(max_tokens));
                            return;
                        }
                        let (position, start, end) = cur.token(t);
                        list.push(ExplainToken {
                            token: t.term.clone(),
                            start_offset: start,
                            end_offset: end,
                            kind: t.kind.to_string(),
                            position,
                            position_length: t.pos_len,
                            attributes: BTreeMap::new(),
                        });
                    })?;
                    if let Some(e) = err {
                        return Err(e);
                    }
                    cur.end(end, inner.pos_gap, inner.offset_gap);
                }
                Ok(list)
            },
        )
    }
}

fn too_many_tokens(max: usize) -> AnalysisError {
    AnalysisError::other(
        "status_exception",
        400,
        format!(
            "The number of tokens produced by calling _analyze has exceeded the allowed maximum of [{max}]. This limit can be set by changing the [index.analyze.max_token_count] index level setting."
        ),
    )
}

/// `BytesRef.toString()`: `[61 62]`.
pub(crate) fn bytes_ref_string(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + b.len() * 3);
    s.push('[');
    for (i, x) in b.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{x:x}"));
    }
    s.push(']');
    s
}

fn explain_token(t: &Tok, cur: &mut Cursor, attrs: u32, include: &[String]) -> ExplainToken {
    let (position, start, end) = cur.token(t);
    let mut a = BTreeMap::new();
    let want = |k: &str| include.is_empty() || include.iter().any(|i| i == &k.to_lowercase());
    if want("bytes") {
        a.insert(
            "bytes".to_string(),
            Value::String(bytes_ref_string(t.term.as_bytes())),
        );
    }
    if want("positionLength") {
        a.insert("positionLength".to_string(), json!(t.pos_len));
    }
    if attrs & attr::TERM_FREQ != 0 && want("termFrequency") {
        a.insert("termFrequency".to_string(), json!(t.term_freq));
    }
    if attrs & attr::KEYWORD != 0 && want("keyword") {
        a.insert("keyword".to_string(), Value::Bool(t.keyword));
    }
    if attrs & attr::PAYLOAD != 0 && want("payload") {
        a.insert(
            "payload".to_string(),
            t.payload()
                .map_or(Value::Null, |p| Value::String(bytes_ref_string(p))),
        );
    }
    if attrs & attr::FLAGS != 0 && want("flags") {
        a.insert("flags".to_string(), json!(t.flags));
    }
    ExplainToken {
        token: t.term.clone(),
        start_offset: start,
        end_offset: end,
        kind: t.kind.to_string(),
        position,
        position_length: t.pos_len,
        attributes: a,
    }
}

/// A normalizer: a keyword-tokenized chain of per-character filters (`keyword` field values).
#[derive(Clone)]
pub struct Normalizer {
    pub(crate) analyzer: Analyzer,
}

impl std::fmt::Debug for Normalizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Normalizer({})", self.analyzer.inner.name)
    }
}

impl Normalizer {
    pub fn name(&self) -> &str {
        self.analyzer.name()
    }

    /// The normalized form of `text` (one token; empty if a filter removed it).
    pub fn normalize(&self, text: &str) -> String {
        let mut out: Option<String> = None;
        self.analyzer.analyze(text, &mut |t| {
            if out.is_none() {
                out = Some(t.term.to_string());
            }
        });
        out.unwrap_or_default()
    }

    /// The normalizer as an analyzer (for `_analyze` with `normalizer`).
    pub fn analyzer(&self) -> &Analyzer {
        &self.analyzer
    }
}

/// `_analyze` explain output.
#[derive(Clone, Debug, PartialEq)]
pub struct Explain {
    pub custom_analyzer: bool,
    pub charfilters: Vec<CharFilteredText>,
    pub tokenizer: Option<TokenList>,
    pub tokenfilters: Vec<TokenList>,
    /// Set instead of the stages for non-custom analyzers.
    pub analyzer: Option<TokenList>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CharFilteredText {
    pub name: String,
    pub filtered_text: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TokenList {
    /// The component's name (None renders as `null`).
    pub name: Option<String>,
    pub tokens: Vec<ExplainToken>,
}

/// One token as `_analyze` reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct ExplainToken {
    pub token: String,
    pub start_offset: i64,
    pub end_offset: i64,
    pub kind: String,
    pub position: i64,
    pub position_length: u32,
    /// Extended attributes (`bytes`, `keyword`, `positionLength`, `termFrequency` …), only in
    /// explain output.
    pub attributes: BTreeMap<String, Value>,
}

impl ExplainToken {
    /// Elasticsearch's `AnalyzeToken` JSON.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("token".into(), Value::String(self.token.clone()));
        m.insert("start_offset".into(), json!(self.start_offset));
        m.insert("end_offset".into(), json!(self.end_offset));
        m.insert("type".into(), Value::String(self.kind.clone()));
        m.insert("position".into(), json!(self.position));
        if self.position_length > 1 {
            m.insert("positionLength".into(), json!(self.position_length));
        }
        for (k, v) in &self.attributes {
            m.insert(k.clone(), v.clone());
        }
        Value::Object(m)
    }
}

impl TokenList {
    fn to_json(&self) -> Value {
        json!({"name": self.name, "tokens": self.tokens.iter().map(ExplainToken::to_json).collect::<Vec<_>>()})
    }
}

impl Explain {
    /// The `detail` object of the `_analyze` response.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("custom_analyzer".into(), Value::Bool(self.custom_analyzer));
        if let Some(a) = &self.analyzer {
            m.insert("analyzer".into(), a.to_json());
        }
        if self.custom_analyzer {
            m.insert(
                "charfilters".into(),
                Value::Array(
                    self.charfilters
                        .iter()
                        .map(|c| json!({"name": c.name, "filtered_text": c.filtered_text}))
                        .collect(),
                ),
            );
        }
        if let Some(t) = &self.tokenizer {
            m.insert("tokenizer".into(), t.to_json());
        }
        if self.custom_analyzer {
            m.insert(
                "tokenfilters".into(),
                Value::Array(self.tokenfilters.iter().map(TokenList::to_json).collect()),
            );
        }
        Value::Object(m)
    }
}
