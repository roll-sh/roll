//! RediSearch: the `search` module Redis 8.10 bundles (v8.10.1), reimplemented. Indexes over
//! hashes and JSON documents, the query language, aggregations, vectors, suggestions, spelling
//! dictionaries and synonyms, with RediSearch's replies, errors and limits.

pub mod areq;
pub mod args;
pub mod casefold;
pub mod cluster;
pub mod config;
pub mod cursor;
pub mod doctable;
pub mod error;
pub mod eval;
pub mod expr;
pub mod friso;
pub mod gc;
pub mod geo;
pub mod geometry;
pub mod global;
pub mod highlight;
pub mod hnsw;
pub mod hll;
pub mod index;
pub mod info;
pub mod invidx;
pub mod iters;
pub mod jsonpath;
pub mod lalr;
pub mod levenshtein;
pub mod mmheap;
pub mod numtree;
pub mod optimizer;
pub mod phonetic;
pub mod qast;
pub mod qeval;
pub mod qgrammar;
pub mod qlex;
pub mod qparse;
pub mod quantile;
pub mod rdb;
pub mod rsdict;
pub mod scorers;
pub mod spec;
pub mod tagalog;
pub mod strconv;
pub mod tokenize;
pub mod trie;
pub mod triemap;
pub mod vec_iters;
pub mod vecsim;
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
