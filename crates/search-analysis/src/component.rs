//! Component factories: what a configured tokenizer, token filter or char filter is.
//!
//! A factory is built once from its settings (parsing word lists, compiling patterns, building
//! synonym maps) and shared by every chain instance; `create` makes the cheap per-chain state.
//! Factories also carry the metadata Elasticsearch derives from the factory's Java class: whether
//! a filter may run in a normalizer, which attributes it adds (for `_analyze` explain), its
//! analysis mode (search-time-only filters), and how it behaves inside a synonym-parsing chain.

use std::sync::Arc;

use crate::error::Result;
use crate::stream::{CharFilter, Filter, Tokenizer};

/// Where an analyzer may run (`AnalysisMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnalysisMode {
    All,
    IndexTime,
    SearchTime,
}

impl AnalysisMode {
    pub fn readable_name(self) -> &'static str {
        match self {
            AnalysisMode::All => "all",
            AnalysisMode::IndexTime => "index time",
            AnalysisMode::SearchTime => "search time",
        }
    }

    pub(crate) fn merge(self, other: AnalysisMode) -> std::result::Result<AnalysisMode, ()> {
        match (self, other) {
            (AnalysisMode::All, o) => Ok(o),
            (s, AnalysisMode::All) => Ok(s),
            (a, b) if a == b => Ok(a),
            _ => Err(()),
        }
    }
}

pub(crate) trait TokenizerFactory: Send + Sync {
    fn create(&self) -> Box<dyn Tokenizer>;

    /// Fails when the component cannot be used (Lucene throws when creating the stream, which
    /// Elasticsearch surfaces when it builds the analyzer).
    fn check(&self) -> Result<()> {
        Ok(())
    }

    /// Attributes the tokenizer's attribute source has (see [`crate::token::attr`]).
    fn attrs(&self) -> u32 {
        crate::token::attr::TERM_FREQ
    }
}

/// How a filter takes part when synonym rules are analyzed by the chain before a synonym filter
/// (`TokenFilterFactory.getSynonymFilter`).
#[derive(Clone, Debug)]
pub(crate) enum SynonymRole {
    /// Applied to the rules as to text.
    Same,
    /// Skipped.
    Identity,
    /// Not allowed: `Token filter [name] cannot be used to parse synonyms`.
    Forbidden,
    /// Replaced by another filter (e.g. `asciifolding` without `preserve_original`).
    Replace(Arc<dyn FilterFactory>),
}

impl std::fmt::Debug for dyn FilterFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FilterFactory")
    }
}

/// The analysis chain preceding a filter, for filters whose behaviour depends on it.
pub(crate) struct ChainParts<'a> {
    pub char_filters: &'a [NamedCharFilter],
    pub tokenizer: &'a NamedTokenizer,
    pub filters: &'a [NamedFilter],
    /// The referring filters (`multiplexer`, `condition`) being expanded, outermost first, to
    /// detect reference cycles.
    pub referring: &'a [Arc<str>],
}

/// Resolves a token filter by name in the current scope: Ok(None) when an index has no such
/// filter (the referring filter reports it), an error where Elasticsearch's lookup throws
/// (inline `_analyze` definitions).
pub(crate) type FilterResolver<'a> = &'a dyn Fn(&str) -> Result<Option<NamedFilter>>;

pub(crate) trait FilterFactory: Send + Sync {
    fn create(&self) -> Box<dyn Filter>;

    /// Fails when the filter cannot be used (see [`TokenizerFactory::check`]).
    fn check(&self) -> Result<()> {
        Ok(())
    }

    /// Allowed in normalizers and applied by `Analyzer.normalize` (`NormalizingTokenFilterFactory`).
    fn normalizing(&self) -> bool {
        false
    }

    /// The filter `Analyzer.normalize` applies (`TokenFilterFactory.normalize`), for normalizing
    /// filters whose normalization differs from their regular behaviour.
    fn create_normalizer(&self) -> Box<dyn Filter> {
        self.create()
    }

    /// The optional attributes it adds ([`crate::token::attr`]).
    fn attrs(&self) -> u32 {
        0
    }

    fn mode(&self) -> AnalysisMode {
        AnalysisMode::All
    }

    fn synonym_role(&self) -> SynonymRole {
        SynonymRole::Same
    }

    /// `getChainAwareTokenFilterFactory`: a replacement built with knowledge of the preceding
    /// chain (synonym filters analyze their rules with it; `multiplexer` and `condition` resolve
    /// the filters they wrap). None keeps `self`.
    fn chain_aware(
        &self,
        _prev: &ChainParts<'_>,
        _resolve: FilterResolver<'_>,
    ) -> Result<Option<Arc<dyn FilterFactory>>> {
        Ok(None)
    }
}

pub(crate) trait CharFilterFactory: Send + Sync {
    fn create(&self) -> Box<dyn CharFilter>;

    /// Allowed in normalizers (`NormalizingCharFilterFactory`).
    fn normalizing(&self) -> bool {
        false
    }
}

#[derive(Clone)]
pub(crate) struct NamedTokenizer {
    pub name: Arc<str>,
    pub factory: Arc<dyn TokenizerFactory>,
}

#[derive(Clone)]
pub(crate) struct NamedFilter {
    pub name: Arc<str>,
    pub factory: Arc<dyn FilterFactory>,
}

#[derive(Clone)]
pub(crate) struct NamedCharFilter {
    pub name: Arc<str>,
    pub factory: Arc<dyn CharFilterFactory>,
}

/// Marks a filter as search-time only (`updateable` synonyms and keyword markers).
pub(crate) struct SearchTimeOnly(pub Arc<dyn FilterFactory>);

impl FilterFactory for SearchTimeOnly {
    fn create(&self) -> Box<dyn Filter> {
        self.0.create()
    }
    fn check(&self) -> Result<()> {
        self.0.check()
    }
    fn normalizing(&self) -> bool {
        self.0.normalizing()
    }
    fn create_normalizer(&self) -> Box<dyn Filter> {
        self.0.create_normalizer()
    }
    fn attrs(&self) -> u32 {
        self.0.attrs()
    }
    fn mode(&self) -> AnalysisMode {
        AnalysisMode::SearchTime
    }
    fn synonym_role(&self) -> SynonymRole {
        self.0.synonym_role()
    }
    fn chain_aware(
        &self,
        prev: &ChainParts<'_>,
        resolve: FilterResolver<'_>,
    ) -> Result<Option<Arc<dyn FilterFactory>>> {
        Ok(self
            .0
            .chain_aware(prev, resolve)?
            .map(|f| Arc::new(SearchTimeOnly(f)) as Arc<dyn FilterFactory>))
    }
}

/// A filter that fails whenever an analyzer uses it (deprecated names).
pub(crate) struct Unusable(pub crate::error::AnalysisError);

impl FilterFactory for Unusable {
    fn create(&self) -> Box<dyn Filter> {
        struct Nothing;
        impl Filter for Nothing {
            fn next(
                &mut self,
                input: &mut dyn crate::stream::TokenStream,
                t: &mut crate::token::Tok,
            ) -> bool {
                input.next(t)
            }
        }
        Box::new(Nothing)
    }
    fn check(&self) -> Result<()> {
        Err(self.0.clone())
    }
}

/// A component whose error Lucene raises only when the stream is created: Elasticsearch
/// accepts its definition and reports the error once an analyzer uses it.
pub(crate) struct LazyError {
    pub error: crate::error::AnalysisError,
    /// Allowed in normalizers (the error then comes from normalizing).
    pub normalizing: bool,
}

impl FilterFactory for LazyError {
    fn create(&self) -> Box<dyn Filter> {
        Unusable(self.error.clone()).create()
    }
    fn check(&self) -> Result<()> {
        Err(self.error.clone())
    }
    fn normalizing(&self) -> bool {
        self.normalizing
    }
}

impl TokenizerFactory for LazyError {
    fn create(&self) -> Box<dyn Tokenizer> {
        struct Nothing;
        impl Tokenizer for Nothing {
            fn reset(&mut self, _text: &str) {}
            fn next(&mut self, _text: &str, _t: &mut crate::token::Tok) -> bool {
                false
            }
        }
        Box::new(Nothing)
    }
    fn check(&self) -> Result<()> {
        Err(self.error.clone())
    }
}

/// A tokenizer factory that fails when used (see [`LazyError`]).
pub(crate) fn lazy_tokenizer_error(
    error: crate::error::AnalysisError,
) -> Arc<dyn TokenizerFactory> {
    Arc::new(LazyError {
        error,
        normalizing: false,
    })
}

/// A filter factory that fails when used (see [`LazyError`]).
pub(crate) fn lazy_filter_error(
    error: crate::error::AnalysisError,
    normalizing: bool,
) -> Arc<dyn FilterFactory> {
    Arc::new(LazyError { error, normalizing })
}

/// A tokenizer factory from a closure.
pub(crate) struct FnTokenizer<F>(pub F);

impl<F: Fn() -> Box<dyn Tokenizer> + Send + Sync> TokenizerFactory for FnTokenizer<F> {
    fn create(&self) -> Box<dyn Tokenizer> {
        (self.0)()
    }
}

pub(crate) fn tokenizer_fn<F: Fn() -> Box<dyn Tokenizer> + Send + Sync + 'static>(
    f: F,
) -> Arc<dyn TokenizerFactory> {
    Arc::new(FnTokenizer(f))
}

/// A tokenizer factory whose attribute source has only `attrs` among the optional attributes.
struct AttrsTokenizer<F>(F, u32);

impl<F: Fn() -> Box<dyn Tokenizer> + Send + Sync> TokenizerFactory for AttrsTokenizer<F> {
    fn create(&self) -> Box<dyn Tokenizer> {
        (self.0)()
    }
    fn attrs(&self) -> u32 {
        self.1
    }
}

pub(crate) fn tokenizer_fn_attrs<F: Fn() -> Box<dyn Tokenizer> + Send + Sync + 'static>(
    attrs: u32,
    f: F,
) -> Arc<dyn TokenizerFactory> {
    Arc::new(AttrsTokenizer(f, attrs))
}

/// A filter factory from a closure plus its metadata.
pub(crate) struct FnFilter<F> {
    pub make: F,
    pub normalizing: bool,
    pub attrs: u32,
    pub synonym_role: SynonymRole,
}

impl<F: Fn() -> Box<dyn Filter> + Send + Sync> FilterFactory for FnFilter<F> {
    fn create(&self) -> Box<dyn Filter> {
        (self.make)()
    }
    fn normalizing(&self) -> bool {
        self.normalizing
    }
    fn attrs(&self) -> u32 {
        self.attrs
    }
    fn synonym_role(&self) -> SynonymRole {
        self.synonym_role.clone()
    }
}

/// A plain filter.
pub(crate) fn filter_fn<F: Fn() -> Box<dyn Filter> + Send + Sync + 'static>(
    f: F,
) -> Arc<dyn FilterFactory> {
    Arc::new(FnFilter {
        make: f,
        normalizing: false,
        attrs: 0,
        synonym_role: SynonymRole::Same,
    })
}

/// A filter allowed in normalizers.
pub(crate) fn normalizing_fn<F: Fn() -> Box<dyn Filter> + Send + Sync + 'static>(
    f: F,
) -> Arc<dyn FilterFactory> {
    Arc::new(FnFilter {
        make: f,
        normalizing: true,
        attrs: 0,
        synonym_role: SynonymRole::Same,
    })
}

/// A filter that adds the keyword attribute (stemmers and keyword markers).
pub(crate) fn keyword_aware_fn<F: Fn() -> Box<dyn Filter> + Send + Sync + 'static>(
    f: F,
) -> Arc<dyn FilterFactory> {
    Arc::new(FnFilter {
        make: f,
        normalizing: false,
        attrs: crate::token::attr::KEYWORD,
        synonym_role: SynonymRole::Same,
    })
}

/// A filter with explicit metadata.
pub(crate) fn filter_with<F: Fn() -> Box<dyn Filter> + Send + Sync + 'static>(
    normalizing: bool,
    attrs: u32,
    synonym_role: SynonymRole,
    f: F,
) -> Arc<dyn FilterFactory> {
    Arc::new(FnFilter {
        make: f,
        normalizing,
        attrs,
        synonym_role,
    })
}
