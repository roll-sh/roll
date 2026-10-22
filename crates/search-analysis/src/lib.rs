//! Elasticsearch-compatible text analysis: char filters, tokenizers, token filters, analyzers and
//! normalizers, with Elasticsearch's names, settings, defaults, errors and `_analyze` output.
//!
//! # Design
//!
//! - **Exactness first.** Every component is a port of the Lucene/Elasticsearch class Elasticsearch
//!   9.5.5 instantiates, down to the details that decide index contents: UTF-16 offsets and lengths,
//!   `java.lang.Character` semantics on Unicode 17 ([`unicode`]), the JFlex grammars of the
//!   standard, classic and URL/email tokenizers compiled by our own lexer generator ([`dfa`]),
//!   Java regular expressions translated to the `regex` crate ([`jregex`]), Lucene's `RegExp`
//!   syntax ([`lregexp`]), and Elasticsearch's settings parsing and error texts ([`settings`]).
//!   `tests/fixtures/` holds outputs captured from a real Elasticsearch, which the tests compare
//!   against.
//! - **No allocation per token.** A chain ([`stream::Chain`]) reuses one token record and every
//!   buffer across tokens and documents; analyzers keep a pool of chains so concurrent callers
//!   never share one. Filters do not own their input, which keeps components plain boxed values
//!   and lets `_analyze` explain run any prefix of a chain.
//! - **Two-level structure.** [`Analysis`] is one index's analysis configuration, built from its
//!   `index.analysis` settings with Elasticsearch's resolution rules (configured components, then
//!   pre-built ones, then defaults); [`Analyzer`] and [`Normalizer`] are the cheap handles the
//!   indexing and query code holds.
//!
//! Script-based filters (`condition`, `predicate_token_filter`) and synonym sets stored in the
//! cluster reach outside this crate through [`ScriptHost`] and [`SynonymSets`], which the
//! Elasticsearch layer implements.

mod analysis;
mod analyze;
mod analyzer;
mod analyzers;
mod charfilter;
mod component;
mod dfa;
mod error;
mod filter;
mod jregex;
mod lregexp;
mod registry;
mod settings;
mod stem;
mod stopwords;
mod stream;
mod token;
mod tokenizer;
mod unicode;

use std::sync::Arc;

pub use analysis::Analysis;
pub use analyze::{AnalyzeLimits, AnalyzeRequest, NameOrDefinition};
pub use analyzer::{Analyzer, CharFilteredText, Explain, ExplainToken, Normalizer, TokenList};
pub use component::AnalysisMode;
pub use error::{AnalysisError, is_java_exception};
pub use token::Token;
pub use tokenizer::thai::{ThaiDictionary, set_thai_dictionary};

/// Builds the process-wide tables components otherwise build on first use, so a server can pay
/// for them at startup (on a background thread) instead of in the first request that needs
/// them: the scanners of `uax_url_email` (a few hundred milliseconds: its URL grammar has
/// thousands of states), `standard`, `classic`, `thai` and `html_strip`, the `kstem` dictionary
/// and the Unicode classes of Java regular expressions. Idempotent and cheap once done.
pub fn warm_up() {
    let analysis = Analysis::global();
    let text = "Warm-up: https://example.com/x?y=1 mail@example.org 3.14 \u{0E20}\u{0E32}\u{0E29}\u{0E32} <b>x</b> running";
    for def in [
        serde_json::json!({"tokenizer": "uax_url_email"}),
        serde_json::json!({"tokenizer": "standard", "char_filter": ["html_strip"], "filter": ["kstem"]}),
        serde_json::json!({"tokenizer": "classic"}),
        serde_json::json!({"tokenizer": "thai"}),
        serde_json::json!({"tokenizer": {"type": "pattern", "pattern": "[\\w\\p{L}]+", "flags": "UNICODE_CHARACTER_CLASS", "group": 0}}),
    ] {
        if let Ok(analyzer) = analysis.build_ad_hoc_json(&def) {
            analyzer.analyze(text, &mut |_| {});
        }
    }
}

/// The version an index was created with (`index.version.created`), which a few components
/// consult for backwards-compatible behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexVersion {
    pub major: u8,
    pub minor: u8,
    pub patch: u8,
}

impl IndexVersion {
    pub const CURRENT: IndexVersion = IndexVersion {
        major: 9,
        minor: 5,
        patch: 5,
    };
    pub const V_7_3_0: IndexVersion = IndexVersion {
        major: 7,
        minor: 3,
        patch: 0,
    };
    pub const V_7_6_0: IndexVersion = IndexVersion {
        major: 7,
        minor: 6,
        patch: 0,
    };
    pub const V_8_0_0: IndexVersion = IndexVersion {
        major: 8,
        minor: 0,
        patch: 0,
    };

    pub fn on_or_after(self, other: IndexVersion) -> bool {
        self >= other
    }
}

impl Default for IndexVersion {
    fn default() -> Self {
        IndexVersion::CURRENT
    }
}

/// Index-level settings analysis components read, plus the hooks into the rest of the engine.
#[derive(Clone)]
pub struct IndexOptions {
    pub version: IndexVersion,
    /// `index.max_ngram_diff` (default 1).
    pub max_ngram_diff: i32,
    /// `index.max_shingle_diff` (default 3).
    pub max_shingle_diff: i32,
    /// Compiles `condition` / `predicate_token_filter` scripts. Without it those filters fail to
    /// build, as on a node without scripting.
    pub scripts: Option<Arc<dyn ScriptHost>>,
    /// Resolves `synonyms_set` references (the synonyms API).
    pub synonym_sets: Option<Arc<dyn SynonymSets>>,
    /// `index.analyze.max_token_count` (default 10000), which bounds `min_hash` output.
    pub max_token_count: usize,
    /// The index name, which a few error texts include (None for `_analyze` without an index).
    pub index_name: Option<String>,
}

impl Default for IndexOptions {
    fn default() -> Self {
        IndexOptions {
            version: IndexVersion::CURRENT,
            max_ngram_diff: 1,
            max_shingle_diff: 3,
            scripts: None,
            synonym_sets: None,
            max_token_count: 10_000,
            index_name: None,
        }
    }
}

impl std::fmt::Debug for IndexOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexOptions")
            .field("version", &self.version)
            .field("max_ngram_diff", &self.max_ngram_diff)
            .field("max_shingle_diff", &self.max_shingle_diff)
            .finish_non_exhaustive()
    }
}

/// The token a `condition` or `predicate_token_filter` script sees (Elasticsearch's
/// `AnalysisPredicateScript.Token`).
#[derive(Clone, Copy, Debug)]
pub struct ScriptToken<'a> {
    pub term: &'a str,
    pub position: i64,
    pub position_increment: u32,
    pub position_length: u32,
    pub start_offset: u32,
    pub end_offset: u32,
    pub kind: &'a str,
    pub keyword: bool,
}

/// A compiled analysis predicate script.
pub trait TokenPredicate: Send + Sync {
    /// Runs the script; an `Err` fails the analysis (as a script exception does in Elasticsearch).
    fn test(&self, token: &ScriptToken<'_>) -> Result<bool, AnalysisError>;
}

/// Compiles scripts in the `analysis` context.
pub trait ScriptHost: Send + Sync {
    /// `script` is the filter's `script` setting as JSON (`{"source": …, "lang"?, "params"?}` or
    /// a string).
    fn compile_predicate(
        &self,
        script: &serde_json::Value,
    ) -> Result<Arc<dyn TokenPredicate>, AnalysisError>;
}

/// Synonym sets managed through the synonyms API.
pub trait SynonymSets: Send + Sync {
    /// The rules of a set, in Solr format, or None if it does not exist.
    fn rules(&self, set: &str) -> Option<Vec<String>>;
}
