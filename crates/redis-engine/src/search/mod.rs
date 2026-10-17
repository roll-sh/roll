//! RediSearch: the `search` module Redis 8.10 bundles (v8.10.1), reimplemented. Indexes over
//! hashes and JSON documents, the query language, aggregations, vectors, suggestions, spelling
//! dictionaries and synonyms, with RediSearch's replies, errors and limits.

pub mod areq;
pub mod args;
pub mod casefold;
pub mod cluster;
pub mod config;
pub mod doctable;
pub mod error;
pub mod eval;
pub mod expr;
pub mod global;
pub mod index;
pub mod info;
pub mod invidx;
pub mod jsonpath;
pub mod lalr;
pub mod levenshtein;
pub mod numtree;
pub mod phonetic;
pub mod qast;
pub mod qgrammar;
pub mod qlex;
pub mod qparse;
pub mod rsdict;
pub mod spec;
pub mod strconv;
pub mod tokenize;
pub mod trie;
pub mod triemap;
pub mod vector;

/// `MODULE LIST`'s version (8.10.1).
pub const VERSION: i64 = 81001;
pub const VERSION_TEXT: &str = "8.10.1";

/// The scorers RediSearch registers (`DefaultExtensionInit`); names are case-sensitive.
pub const SCORERS: &[&str] = &["TFIDF", "DISMAX", "BM25", "BM25STD", "BM25STD.TANH", "BM25STD.NORM", "HAMMING", "TFIDF.DOCNORM", "DOCSCORE"];

/// `Extensions_GetScoringFunction(NULL, name) != NULL`.
pub fn scorer_exists(name: &str) -> bool {
    SCORERS.contains(&name)
}
