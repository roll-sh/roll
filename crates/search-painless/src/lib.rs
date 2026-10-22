//! The Painless scripting language (and Mustache templates) for roll search.
//!
//! See `docs/search/architecture.md` and `docs/search/contracts.md`.

// `ScriptError` carries Elasticsearch's whole rendered error (stack, position, causes); it is
// only built on the cold path, so returning it by value costs nothing that matters.
#![allow(clippy::result_large_err)]

mod analyze;
mod ast;
mod bignum;
mod cast;
mod charblocks;
mod compile;
pub mod contexts;
mod defmath;
pub mod dissect;
mod dynamic;
mod engine;
mod fast;
pub mod error;
pub mod es;
mod format;
mod func;
pub mod grok;
mod guard;
pub mod host;
mod ir;
mod jackson;
mod iter;
mod java;
mod lexer;
mod lib_coll;
mod lib_lang;
mod lib_util;
mod locale;
mod lookup;
pub mod mustache;
mod parser;
mod peephole;
mod regex;
mod sort;
mod stream;
mod time;
mod types;
mod unicode;
pub mod text;
pub mod value;
mod vm;
pub mod xcontent;

pub use contexts::Context;
pub use engine::{compile_painless, Compiled, Engine, Instance, Lang, RegexEnabled, Script, Settings, Stats, Vars};
pub use error::{Position, ScriptError};
pub use host::{DocKind, DocValues, Host, HostError, NoHost, TermStats, VectorElement, VectorView};
pub use value::Value;
#[doc(hidden)]
pub use lookup::dump as registry_dump;
