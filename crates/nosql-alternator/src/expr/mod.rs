//! The DynamoDB expression languages: condition, filter, key condition, update and projection
//! expressions.
//!
//! [`parse`] turns text into the syntax tree of [`ast`] (failing exactly where ScyllaDB's ANTLR
//! grammar does), [`resolve`] binds `#name` / `:value` references, [`eval`] evaluates
//! conditions, [`update`] applies updates and [`project`] selects document paths; [`cache`]
//! keeps parsed expressions (ScyllaDB's expression cache).

pub mod ast;
pub mod cache;
pub mod eval;
pub mod parse;
pub mod project;
pub mod resolve;
pub mod update;
