//! ltree (`contrib/ltree`): labels of a tree-like hierarchy (`ltree`), the patterns over
//! them (`lquery`, `ltxtquery`), their operators and functions, also over arrays of them.

pub mod data;
pub mod lquery;
mod ops;
pub mod txt;

use super::native::Native;
use super::types::{ExtType, LQUERY, LTREE, LTREE_GIST, LTXTQUERY};
use crate::Result;
use crate::error::{Error, code};
use crate::sql::func::FnCtx;
use crate::types::Value;
use std::borrow::Cow;

pub(super) const LTREE_TYPE: ExtType = ExtType {
    kind: LTREE,
    extension: "ltree",
    name: "ltree",
    input: data::parse,
    output: |b, _| data::output(b),
    cmp: data::compare,
    canon: |b| Cow::Borrowed(b),
    key: data::key,
    to_builtin: None,
    from_builtin: None,
    hash: true,
};

pub(super) const LQUERY_TYPE: ExtType = ExtType {
    kind: LQUERY,
    extension: "ltree",
    name: "lquery",
    input: |s| Ok(lquery::encode(&lquery::parse(s)?)),
    output: |b, _| lquery::output(&lquery::decode(b)),
    cmp: |a, b| a.cmp(b),
    canon: |b| Cow::Borrowed(b),
    key: super::types::key_bytes,
    to_builtin: None,
    from_builtin: None,
    hash: false,
};

pub(super) const LTXTQUERY_TYPE: ExtType = ExtType {
    kind: LTXTQUERY,
    extension: "ltree",
    name: "ltxtquery",
    input: |s| Ok(txt::encode(&txt::parse(s)?)),
    output: |b, _| txt::output(&txt::decode(b)),
    cmp: |a, b| a.cmp(b),
    canon: |b| Cow::Borrowed(b),
    key: super::types::key_bytes,
    to_builtin: None,
    from_builtin: None,
    hash: false,
};

pub(super) const GIST_TYPE: ExtType = ExtType {
    kind: LTREE_GIST,
    extension: "ltree",
    name: "ltree_gist",
    input: |_| Err(Error::new(code::FEATURE_NOT_SUPPORTED, "cannot accept a value of type ltree_gist")),
    output: |_, _| String::new(),
    cmp: |a, b| a.cmp(b),
    canon: |b| Cow::Borrowed(b),
    key: super::types::key_bytes,
    to_builtin: None,
    from_builtin: None,
    hash: false,
};

fn text_in(kind: u16, a: &[Value]) -> Result<Value> {
    super::types::input(kind, crate::sql::func::text_arg(&a[0]))
}

fn text_out(kind: u16, a: &[Value]) -> Result<Value> {
    let b: &[u8] = match &a[0] {
        Value::Ext(_, b) => b,
        _ => &[],
    };
    Ok(Value::text(super::types::output_plain(kind, b)))
}

fn ltree_in(a: &[Value], _: &FnCtx) -> Result<Value> {
    text_in(LTREE, a)
}
fn ltree_out(a: &[Value], _: &FnCtx) -> Result<Value> {
    text_out(LTREE, a)
}
fn lquery_in(a: &[Value], _: &FnCtx) -> Result<Value> {
    text_in(LQUERY, a)
}
fn lquery_out(a: &[Value], _: &FnCtx) -> Result<Value> {
    text_out(LQUERY, a)
}
fn ltxtq_in(a: &[Value], _: &FnCtx) -> Result<Value> {
    text_in(LTXTQUERY, a)
}
fn ltxtq_out(a: &[Value], _: &FnCtx) -> Result<Value> {
    text_out(LTXTQUERY, a)
}
fn gist_in(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(Error::new(code::FEATURE_NOT_SUPPORTED, "cannot accept a value of type ltree_gist"))
}
fn gist_out(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(Error::new(code::FEATURE_NOT_SUPPORTED, "cannot display a value of type ltree_gist"))
}

/// Index support, selectivity and binary receive functions: `internal` arguments, never
/// called from SQL.
fn support(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(crate::error::internal("an index support function cannot be called from SQL"))
}

const SUPPORT: &[&str] = &[
    "ltreeparentsel",
    "ltree_recv",
    "lquery_recv",
    "ltxtq_recv",
    "ltree_consistent",
    "ltree_compress",
    "ltree_decompress",
    "ltree_penalty",
    "ltree_picksplit",
    "ltree_union",
    "ltree_same",
    "ltree_gist_options",
    "_ltree_consistent",
    "_ltree_compress",
    "_ltree_penalty",
    "_ltree_picksplit",
    "_ltree_union",
    "_ltree_same",
    "_ltree_gist_options",
];

/// The options of ltree's GiST operator classes: (name, default, min, max, multiple of).
pub(super) fn options(sym: &str) -> Option<Vec<(&'static str, i64, i64, i64, i64)>> {
    Some(match sym {
        "ltree_gist_options" => vec![("siglen", 8, 4, 2024, 4)],
        "_ltree_gist_options" => vec![("siglen", 28, 1, 2024, 1)],
        _ => return None,
    })
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    use ops::*;
    if SUPPORT.contains(&sym) {
        return Some(Native::scalar(support));
    }
    Some(Native::scalar(match sym {
        "ltree_in" => ltree_in,
        "ltree_out" => ltree_out,
        "lquery_in" => lquery_in,
        "lquery_out" => lquery_out,
        "ltxtq_in" => ltxtq_in,
        "ltxtq_out" => ltxtq_out,
        "ltree_gist_in" => gist_in,
        "ltree_gist_out" => gist_out,
        "ltree_send" => ltree_send,
        "lquery_send" => lquery_send,
        "ltxtq_send" => ltxtq_send,
        "ltree_cmp" => ltree_cmp,
        "ltree_lt" => lt,
        "ltree_le" => le,
        "ltree_eq" => eq,
        "ltree_ge" => ge,
        "ltree_gt" => gt,
        "ltree_ne" => ne,
        "hash_ltree" => hash_ltree,
        "hash_ltree_extended" => hash_ltree_extended,
        "nlevel" => nlevel,
        "subltree" => subltree,
        "subpath" => subpath,
        "ltree_index" => ltree_index,
        "ltree2text" => ltree2text,
        "text2ltree" => text2ltree,
        "lca" => lca,
        "_lca" => a_lca,
        "ltree_isparent" => ltree_isparent,
        "ltree_risparent" => ltree_risparent,
        "ltree_addltree" => addltree,
        "ltree_addtext" => addtext,
        "ltree_textadd" => textadd,
        "ltq_regex" => ltq_regex,
        "ltq_rregex" => ltq_rregex,
        "lt_q_regex" => lt_q_regex,
        "lt_q_rregex" => lt_q_rregex,
        "ltxtq_exec" => ltxtq_exec,
        "ltxtq_rexec" => ltxtq_rexec,
        "_ltree_isparent" => a_isparent,
        "_ltree_r_isparent" => a_r_isparent,
        "_ltree_risparent" => a_risparent,
        "_ltree_r_risparent" => a_r_risparent,
        "_ltq_regex" => a_ltq_regex,
        "_ltq_rregex" => a_ltq_rregex,
        "_lt_q_regex" => a_lt_q_regex,
        "_lt_q_rregex" => a_lt_q_rregex,
        "_ltxtq_exec" => a_ltxtq_exec,
        "_ltxtq_rexec" => a_ltxtq_rexec,
        "_ltree_extract_isparent" => extract_isparent,
        "_ltree_extract_risparent" => extract_risparent,
        "_ltq_extract_regex" => extract_regex,
        "_ltxtq_extract_exec" => extract_exec,
        _ => return None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io() {
        let t = data::parse("Top.Science.Astronomy").unwrap();
        assert_eq!(data::output(&t), "Top.Science.Astronomy");
        assert_eq!(data::output(&data::parse("").unwrap()), "");
        assert_eq!(data::parse(".a").unwrap_err().message, "ltree syntax error at character 1");
        assert_eq!(data::parse("a.").unwrap_err().message, "ltree syntax error");
        for (q, out) in [
            ("*.!Astronomy.*", "*.!Astronomy.*"),
            ("Top.*{1,2}.Astronomy", "Top.*{1,2}.Astronomy"),
            ("a*%@|b{,3}", "a%@*|b{,3}"),
            ("*{0}", "*{0}"),
            ("a{2,}", "a{2,}"),
        ] {
            assert_eq!(lquery::output(&lquery::parse(q).unwrap()), out, "{q}");
        }
        let m = |q: &str| ops::lquery_matches(&t, &lquery::encode(&lquery::parse(q).unwrap())).unwrap();
        assert!(m("*.Astronomy"));
        assert!(!m("Top.*{0,1}"));
        for (q, out) in [
            ("Europe & Russia*@ & !Transportation", "Europe & Russia@* & !Transportation"),
            ("a|b&c", "a | b & c"),
            ("!(a|b)", "!( a | b )"),
        ] {
            assert_eq!(txt::output(&txt::parse(q).unwrap()), out);
        }
        let q = txt::parse("science & !astro*@").unwrap();
        let tr = data::parse("Top.Science.Physics").unwrap();
        assert!(!txt::execute(&q, 0, &data::labels(&tr)));
    }
}
