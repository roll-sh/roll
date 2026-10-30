//! pg_trgm (`contrib/pg_trgm`): trigrams of text, similarity and word similarity, the
//! threshold operators and distances. Index scans with its GIN and GiST operator classes
//! read the rows the operators select (the executor rechecks every index condition), so the
//! classes' support functions are never called with keys; called from SQL they fail as
//! PostgreSQL's do.

pub(crate) mod trgm;
mod word;

use super::native::Native;
use crate::Result;
use crate::sql::func::FnCtx;
use crate::types::Value;
use std::sync::Arc;

fn text_of(v: &Value) -> &str {
    crate::sql::func::text_arg(v)
}

/// A threshold parameter's value (`pg_trgm.*`; the library is loaded by the call).
fn threshold(c: &FnCtx, name: &str, default: f64) -> f64 {
    c.exec
        .sess
        .settings
        .borrow()
        .get_raw(name)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

const SIMILARITY: &str = "pg_trgm.similarity_threshold";
const WORD: &str = "pg_trgm.word_similarity_threshold";
const STRICT: &str = "pg_trgm.strict_word_similarity_threshold";

fn similarity_of(a: &[Value]) -> f32 {
    let t1 = trgm::generate(text_of(&a[0]));
    let t2 = trgm::generate(text_of(&a[1]));
    trgm::cnt_sml(&t1, &t2)
}

/// word_similarity(a, b) with `flags`, or with the arguments swapped (the commutators).
fn word(a: &[Value], c: &FnCtx, swap: bool, flags: u8) -> f32 {
    let (x, y) = if swap { (&a[1], &a[0]) } else { (&a[0], &a[1]) };
    let limit = if flags & trgm::STRICT_BOUNDS != 0 { threshold(c, STRICT, 0.5) } else { threshold(c, WORD, 0.6) };
    trgm::word_similarity(text_of(x), text_of(y), flags, limit)
}

fn f4(f: f32) -> Result<Value> {
    Ok(Value::Float4(f))
}

fn show_trgm(a: &[Value], _: &FnCtx) -> Result<Value> {
    let items: Vec<Value> = trgm::generate(text_of(&a[0]))
        .iter()
        .map(|t| Value::Text(Arc::from(trgm::shown(t))))
        .collect();
    Ok(Value::Array(Arc::new(crate::types::array::ArrayValue::from_vec(
        crate::types::DataType::Text,
        items,
    ))))
}

/// set_limit: `SET pg_trgm.similarity_threshold` to the float4's output (checked as SET
/// checks it), then the threshold.
fn set_limit(a: &[Value], c: &FnCtx) -> Result<Value> {
    let shown = crate::types::text::format_value(&a[0], &crate::types::DataType::Float4, c.exec.fmt());
    c.exec.sess.settings.borrow_mut().set(SIMILARITY, Some(&shown), false, true)?;
    f4(threshold(c, SIMILARITY, 0.3) as f32)
}

fn not_accepted(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(crate::Error::new(crate::error::code::FEATURE_NOT_SUPPORTED, "cannot accept a value of type gtrgm"))
}

fn not_displayed(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(crate::Error::new(crate::error::code::FEATURE_NOT_SUPPORTED, "cannot display a value of type gtrgm"))
}

/// An index support function: only the access method calls it, with its own keys.
fn support_only(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(crate::error::internal("pg_trgm index support function called outside an index"))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    use trgm::{CHECK_ONLY, STRICT_BOUNDS};
    let f: crate::sql::func::FnImpl = match sym {
        "show_trgm" => show_trgm,
        "set_limit" => set_limit,
        "show_limit" => |_, c| f4(threshold(c, SIMILARITY, 0.3) as f32),
        "similarity" => |a, _| f4(similarity_of(a)),
        "similarity_dist" => |a, _| f4(1.0 - similarity_of(a)),
        "similarity_op" => |a, c| Ok(Value::Bool(similarity_of(a) as f64 >= threshold(c, SIMILARITY, 0.3))),
        "word_similarity" => |a, c| f4(word(a, c, false, 0)),
        "strict_word_similarity" => |a, c| f4(word(a, c, false, STRICT_BOUNDS)),
        "word_similarity_op" => |a, c| Ok(Value::Bool(word(a, c, false, CHECK_ONLY) as f64 >= threshold(c, WORD, 0.6))),
        "word_similarity_commutator_op" => {
            |a, c| Ok(Value::Bool(word(a, c, true, CHECK_ONLY) as f64 >= threshold(c, WORD, 0.6)))
        }
        "word_similarity_dist_op" => |a, c| f4(1.0 - word(a, c, false, 0)),
        "word_similarity_dist_commutator_op" => |a, c| f4(1.0 - word(a, c, true, 0)),
        "strict_word_similarity_op" => {
            |a, c| Ok(Value::Bool(word(a, c, false, CHECK_ONLY | STRICT_BOUNDS) as f64 >= threshold(c, STRICT, 0.5)))
        }
        "strict_word_similarity_commutator_op" => {
            |a, c| Ok(Value::Bool(word(a, c, true, CHECK_ONLY | STRICT_BOUNDS) as f64 >= threshold(c, STRICT, 0.5)))
        }
        "strict_word_similarity_dist_op" => |a, c| f4(1.0 - word(a, c, false, STRICT_BOUNDS)),
        "strict_word_similarity_dist_commutator_op" => |a, c| f4(1.0 - word(a, c, true, STRICT_BOUNDS)),
        "gtrgm_in" => not_accepted,
        "gtrgm_out" => not_displayed,
        "gtrgm_consistent" | "gtrgm_distance" | "gtrgm_compress" | "gtrgm_decompress" | "gtrgm_penalty"
        | "gtrgm_picksplit" | "gtrgm_union" | "gtrgm_same" | "gtrgm_options" | "gin_extract_value_trgm"
        | "gin_extract_query_trgm" | "gin_trgm_consistent" | "gin_trgm_triconsistent" | "gin_extract_trgm" => {
            support_only
        }
        _ => return None,
    };
    Some(Native::scalar(f))
}
