//! Full text search functions and operators (Postgres's to_tsany.c, wparser.c, tsvector_op.c,
//! tsquery_op.c, tsquery_rewrite.c, tsrank.c and ts_debug), over `types::tsearch`.

use super::json::{raw_array, raw_object, raw_typeof, unescape_json_string};
use super::{FnCtx, Registry, SrfIter, text, text_arg};
use crate::catalog::tsearch::{TsCatalog, TsKind};
use crate::catalog::{Catalog, Table};
use crate::error::{Error, Result, Severity};
use crate::types::array::ArrayValue;
use crate::types::json::{Jsonb, write_json_string};
use crate::types::tsearch::config::{
    self, Config, DictSubState, HlText, Lexized, ParsedText, TsLookup, add_to_tsvector,
    lookup_config, lookup_dict,
};
use crate::types::tsearch::{
    self as ts, OP_AND, OP_OR, OP_PHRASE, QuerySyntax, TsQuery, TsVector, exec, headline, parser,
    queryops, rank, vecops,
};
use crate::types::{DataType as T, Value};
use crate::{bail, err};
use std::sync::Arc;

// ── argument and result helpers ──

fn tsv(v: &Value) -> &TsVector {
    static EMPTY: TsVector = TsVector {
        lexemes: Vec::new(),
    };
    match v {
        Value::TsVector(t) => t,
        _ => &EMPTY,
    }
}

fn tsq(v: &Value) -> &TsQuery {
    static EMPTY: TsQuery = TsQuery { items: Vec::new() };
    match v {
        Value::TsQuery(q) => q,
        _ => &EMPTY,
    }
}

fn oid_arg(v: &Value) -> u32 {
    match v {
        Value::Oid(o) => *o,
        v => v.as_i64().unwrap_or(0) as u32,
    }
}

fn vector(v: TsVector) -> Value {
    Value::TsVector(Arc::new(v))
}

fn query(q: TsQuery) -> Value {
    Value::TsQuery(Arc::new(q))
}

/// A text[] argument's elements (NULLs as None).
fn text_items(v: &Value) -> Vec<Option<String>> {
    match v {
        Value::Array(a) => a
            .items
            .iter()
            .map(|x| {
                if x.is_null() {
                    None
                } else {
                    Some(text_arg(x).to_string())
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn text_array(items: Vec<Value>) -> Value {
    Value::Array(Arc::new(ArrayValue::from_vec(T::Text, items)))
}

/// The catalog and search path the text search objects are looked up in.
struct Lookup {
    catalog: Arc<Catalog>,
    search_path: Vec<String>,
}

impl Lookup {
    fn new(c: &FnCtx) -> Lookup {
        Lookup {
            catalog: c.exec.catalog(),
            search_path: c.exec.search_path.clone(),
        }
    }

    fn ts(&self) -> TsCatalog<'_> {
        TsCatalog {
            catalog: &self.catalog,
            search_path: &self.search_path,
        }
    }

    fn config(&self, oid: u32) -> Result<Config> {
        lookup_config(&self.ts(), oid)
    }

    /// `getTSCurrentConfig`.
    fn current(&self, c: &FnCtx) -> Result<u32> {
        let setting = c
            .exec
            .sess
            .settings
            .borrow()
            .show("default_text_search_config")
            .unwrap_or_default();
        self.catalog.ts_current_config(&setting, &self.search_path)
    }

    /// A parser by oid, as `lookup_ts_parser_cache` finds it.
    fn parser(&self, oid: u32) -> Result<config::TsParserDef> {
        self.ts().parser(oid).ok_or_else(|| {
            crate::error::internal(format!("cache lookup failed for text search parser {oid}"))
        })
    }

    /// `get_ts_parser_oid(textToQualifiedNameList(name))`.
    fn parser_named(&self, name: &str) -> Result<u32> {
        let parts = text_to_names(name)?;
        Ok(self
            .catalog
            .ts_lookup(TsKind::Parser, &parts, &self.search_path, false)?
            .unwrap_or(0))
    }
}

/// `textToQualifiedNameList`: a dotted name as SQL would read it.
fn text_to_names(s: &str) -> Result<Vec<String>> {
    crate::catalog::names::parse_qualified(s).map_err(|_| err!(INVALID_NAME, "invalid name syntax"))
}

/// Notices of the parsing code (a word too long to index) go out as NOTICEs.
fn notice_sink<'a>(c: &'a FnCtx) -> impl FnMut(Error) + 'a {
    move |mut e: Error| {
        e.severity = Severity::Notice;
        c.exec.notice(e)
    }
}

// ── to_tsvector ──

fn to_tsvector_byid(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cfg = Lookup::new(c).config(oid_arg(&a[0]))?;
    Ok(vector(config::to_tsvector(
        &cfg,
        text_arg(&a[1]),
        &mut notice_sink(c),
    )?))
}

fn to_tsvector_cur(a: &[Value], c: &FnCtx) -> Result<Value> {
    let l = Lookup::new(c);
    let cfg = l.config(l.current(c)?)?;
    Ok(vector(config::to_tsvector(
        &cfg,
        text_arg(&a[0]),
        &mut notice_sink(c),
    )?))
}

// jsonb_to_tsvector / json_to_tsvector flags (jtiKey …).
const JTI_KEY: u32 = 0x01;
const JTI_STRING: u32 = 0x02;
const JTI_NUMERIC: u32 = 0x04;
const JTI_BOOL: u32 = 0x08;
const JTI_ALL: u32 = JTI_KEY | JTI_STRING | JTI_NUMERIC | JTI_BOOL;

const FLAG_HINT: &str =
    "Possible values are: \"string\", \"numeric\", \"boolean\", \"key\", and \"all\".";

/// `parse_jsonb_index_flags`.
fn index_flags(v: &Value) -> Result<u32> {
    let j = jsonb_arg(v)?;
    let items: Vec<&Jsonb> = match &*j {
        Jsonb::Array(items) => items.iter().collect(),
        Jsonb::Object(_) => bail!(
            INVALID_PARAMETER_VALUE,
            "wrong flag type, only arrays and scalars are allowed"
        ),
        scalar => vec![scalar],
    };
    let mut flags = 0;
    for it in items {
        let Jsonb::String(s) = it else {
            return Err(err!(
                INVALID_PARAMETER_VALUE,
                "flag array element is not a string"
            )
            .hint(FLAG_HINT));
        };
        flags |= match s.to_ascii_lowercase().as_str() {
            "all" => JTI_ALL,
            "key" => JTI_KEY,
            "string" => JTI_STRING,
            "numeric" => JTI_NUMERIC,
            "boolean" => JTI_BOOL,
            _ => {
                return Err(
                    err!(INVALID_PARAMETER_VALUE, "wrong flag in flag array: \"{s}\"")
                        .hint(FLAG_HINT),
                );
            }
        };
    }
    Ok(flags)
}

fn jsonb_arg(v: &Value) -> Result<Arc<Jsonb>> {
    match v {
        Value::Jsonb(j) => Ok(j.clone()),
        Value::Json(s) | Value::Text(s) => Ok(Arc::new(crate::types::json::parse_jsonb(s)?)),
        _ => Ok(Arc::new(Jsonb::Null)),
    }
}

/// `iterate_jsonb_values`: keys and the scalars the flags select, in jsonb order.
fn iterate_jsonb(j: &Jsonb, flags: u32, f: &mut dyn FnMut(&str) -> Result<()>) -> Result<()> {
    crate::stack::guard(|| iterate_jsonb_here(j, flags, f))
}

fn iterate_jsonb_here(j: &Jsonb, flags: u32, f: &mut dyn FnMut(&str) -> Result<()>) -> Result<()> {
    match j {
        Jsonb::Object(members) => {
            for (k, v) in members {
                if flags & JTI_KEY != 0 {
                    f(k)?;
                }
                iterate_jsonb(v, flags, f)?;
            }
        }
        Jsonb::Array(items) => {
            for v in items {
                iterate_jsonb(v, flags, f)?;
            }
        }
        Jsonb::String(s) if flags & JTI_STRING != 0 => f(s)?,
        Jsonb::Number(n) if flags & JTI_NUMERIC != 0 => f(&n.to_string())?,
        Jsonb::Bool(b) if flags & JTI_BOOL != 0 => f(if *b { "true" } else { "false" })?,
        _ => {}
    }
    Ok(())
}

/// `iterate_json_values`: the same over json text, in document order (duplicate keys too).
fn iterate_json(s: &str, flags: u32, f: &mut dyn FnMut(&str) -> Result<()>) -> Result<()> {
    match raw_typeof(s) {
        "object" => {
            for (k, v) in raw_object(s).unwrap_or_default() {
                if flags & JTI_KEY != 0 {
                    f(&k)?;
                }
                iterate_json(v, flags, f)?;
            }
        }
        "array" => {
            for v in raw_array(s).unwrap_or_default() {
                iterate_json(v, flags, f)?;
            }
        }
        "string" if flags & JTI_STRING != 0 => f(&unescape_json_string(s.trim()))?,
        "number" if flags & JTI_NUMERIC != 0 => f(s.trim())?,
        "boolean" if flags & JTI_BOOL != 0 => f(s.trim())?,
        _ => {}
    }
    Ok(())
}

/// to_tsvector over a json(b) document's selected values.
fn json_tsvector(cfg_oid: u32, doc: &Value, flags: u32, c: &FnCtx) -> Result<Value> {
    let cfg = Lookup::new(c).config(cfg_oid)?;
    let mut prs = ParsedText::default();
    let mut sink = notice_sink(c);
    let mut add = |s: &str| add_to_tsvector(&cfg, &mut prs, s, &mut sink);
    match doc {
        Value::Jsonb(j) => iterate_jsonb(j, flags, &mut add)?,
        v => iterate_json(text_arg(v), flags, &mut add)?,
    }
    Ok(vector(config::make_tsvector(prs)?))
}

fn json_string_tsvector_byid(a: &[Value], c: &FnCtx) -> Result<Value> {
    json_tsvector(oid_arg(&a[0]), &a[1], JTI_STRING, c)
}

fn json_string_tsvector(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cfg = Lookup::new(c).current(c)?;
    json_tsvector(cfg, &a[0], JTI_STRING, c)
}

fn json_to_tsvector_byid(a: &[Value], c: &FnCtx) -> Result<Value> {
    let flags = index_flags(&a[2])?;
    json_tsvector(oid_arg(&a[0]), &a[1], flags, c)
}

fn json_to_tsvector(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cfg = Lookup::new(c).current(c)?;
    let flags = index_flags(&a[1])?;
    json_tsvector(cfg, &a[0], flags, c)
}

// ── to_tsquery and friends ──

fn tsquery_with(
    cfg_oid: u32,
    input: &str,
    syntax: QuerySyntax,
    qoperator: u8,
    c: &FnCtx,
) -> Result<Value> {
    let cfg = Lookup::new(c).config(cfg_oid)?;
    Ok(query(config::to_tsquery(
        &cfg,
        input,
        syntax,
        qoperator,
        &mut notice_sink(c),
    )?))
}

macro_rules! tsquery_fns {
    ($($byid:ident, $cur:ident, $syntax:expr, $op:expr;)*) => {$(
        fn $byid(a: &[Value], c: &FnCtx) -> Result<Value> {
            tsquery_with(oid_arg(&a[0]), text_arg(&a[1]), $syntax, $op, c)
        }
        fn $cur(a: &[Value], c: &FnCtx) -> Result<Value> {
            let cfg = Lookup::new(c).current(c)?;
            tsquery_with(cfg, text_arg(&a[0]), $syntax, $op, c)
        }
    )*};
}

tsquery_fns! {
    to_tsquery_byid, to_tsquery_cur, QuerySyntax::Standard, OP_PHRASE;
    plainto_tsquery_byid, plainto_tsquery_cur, QuerySyntax::Plain, OP_AND;
    phraseto_tsquery_byid, phraseto_tsquery_cur, QuerySyntax::Plain, OP_PHRASE;
    websearch_to_tsquery_byid, websearch_to_tsquery_cur, QuerySyntax::Web, OP_PHRASE;
}

fn get_current_ts_config(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Oid(Lookup::new(c).current(c)?))
}

// ── matching ──

fn ts_match_vq(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(exec::ts_match(tsv(&a[0]), &tsq(&a[1]).items)))
}

fn ts_match_qv(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(exec::ts_match(tsv(&a[1]), &tsq(&a[0]).items)))
}

fn ts_match_tq(a: &[Value], c: &FnCtx) -> Result<Value> {
    let v = to_tsvector_cur(&a[..1], c)?;
    Ok(Value::Bool(exec::ts_match(tsv(&v), &tsq(&a[1]).items)))
}

fn ts_match_tt(a: &[Value], c: &FnCtx) -> Result<Value> {
    let l = Lookup::new(c);
    let cfg = l.current(c)?;
    let v = to_tsvector_byid(&[Value::Oid(cfg), a[0].clone()], c)?;
    let q = plainto_tsquery_byid(&[Value::Oid(cfg), a[1].clone()], c)?;
    Ok(Value::Bool(exec::ts_match(tsv(&v), &tsq(&q).items)))
}

// ── tsvector functions ──

fn cmp_int(o: std::cmp::Ordering) -> Value {
    Value::Int4(o as i32)
}

fn tsvector_cmp(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(cmp_int(ts::tsvector_cmp(tsv(&a[0]), tsv(&a[1]))))
}

fn tsquery_cmp(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(cmp_int(ts::tsquery_cmp(tsq(&a[0]), tsq(&a[1]))))
}

fn char_arg(v: &Value) -> u8 {
    match v {
        Value::Char(c) => *c,
        v => text_arg(v).bytes().next().unwrap_or(0),
    }
}

fn setweight(a: &[Value], _: &FnCtx) -> Result<Value> {
    let w = vecops::parse_weight(char_arg(&a[1]))?;
    Ok(vector(vecops::setweight(tsv(&a[0]), w)))
}

fn setweight_by_filter(a: &[Value], _: &FnCtx) -> Result<Value> {
    let w = vecops::parse_weight(char_arg(&a[1]))?;
    Ok(vector(vecops::setweight_by_filter(
        tsv(&a[0]),
        w,
        &text_items(&a[2]),
    )))
}

fn strip(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(vector(vecops::strip(tsv(&a[0]))))
}

fn tsvector_length(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(tsv(&a[0]).lexemes.len() as i32))
}

fn tsvector_concat(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(vector(vecops::concat(tsv(&a[0]), tsv(&a[1]))?))
}

fn ts_delete_str(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(vector(vecops::delete(
        tsv(&a[0]),
        &[Some(text_arg(&a[1]).to_string())],
    )))
}

fn ts_delete_arr(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(vector(vecops::delete(tsv(&a[0]), &text_items(&a[1]))))
}

fn ts_filter(a: &[Value], _: &FnCtx) -> Result<Value> {
    let weights: Vec<Option<u8>> = match &a[1] {
        Value::Array(arr) => arr
            .items
            .iter()
            .map(|v| if v.is_null() { None } else { Some(char_arg(v)) })
            .collect(),
        _ => Vec::new(),
    };
    Ok(vector(vecops::filter(tsv(&a[0]), &weights)?))
}

fn tsvector_to_array(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(text_array(
        tsv(&a[0])
            .lexemes
            .iter()
            .map(|l| text(l.word.as_str()))
            .collect(),
    ))
}

fn array_to_tsvector(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(vector(vecops::from_array(&text_items(&a[0]))?))
}

fn tsvector_unnest(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    let rows: Vec<Result<Vec<Value>>> = vecops::unnest(tsv(&a[0]))
        .into_iter()
        .map(|(lex, pos, weights)| {
            Ok(vec![
                text(lex),
                pos.map_or(Value::Null, |p| {
                    Value::Array(Arc::new(ArrayValue::from_vec(
                        T::Int2,
                        p.into_iter().map(Value::Int2).collect(),
                    )))
                }),
                weights.map_or(Value::Null, |w| {
                    text_array(w.into_iter().map(text).collect())
                }),
            ])
        })
        .collect();
    Ok(Box::new(rows.into_iter()))
}

// ── tsquery functions ──

fn tsquery_and(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(query(queryops::and_or(tsq(&a[0]), tsq(&a[1]), OP_AND)))
}

fn tsquery_or(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(query(queryops::and_or(tsq(&a[0]), tsq(&a[1]), OP_OR)))
}

fn tsquery_not(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(query(queryops::not(tsq(&a[0]))))
}

fn tsquery_phrase(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(query(queryops::phrase(tsq(&a[0]), tsq(&a[1]), 1)?))
}

fn tsquery_phrase_distance(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(query(queryops::phrase(
        tsq(&a[0]),
        tsq(&a[1]),
        a[2].as_i64().unwrap_or(0) as i32,
    )?))
}

fn numnode(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(queryops::numnode(tsq(&a[0]))))
}

fn querytree(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(text(queryops::querytree(tsq(&a[0]))))
}

fn tsq_mcontains(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(queryops::contains(tsq(&a[0]), tsq(&a[1]))))
}

fn tsq_mcontained(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(queryops::contains(tsq(&a[1]), tsq(&a[0]))))
}

fn tsquery_rewrite(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(query(queryops::rewrite(tsq(&a[0]), tsq(&a[1]), tsq(&a[2]))))
}

/// `tsquery_rewrite_query`: (target, substitute) rows of a query.
fn tsquery_rewrite_query(a: &[Value], c: &FnCtx) -> Result<Value> {
    let q = tsq(&a[0]);
    if q.items.is_empty() {
        return Ok(a[0].clone());
    }
    let (rows, cols) = crate::plpgsql::spi_query(c.exec, text_arg(&a[1]))?;
    if cols.len() != 2 || cols.iter().any(|c| c.ty != T::TsQuery) {
        bail!(
            INVALID_PARAMETER_VALUE,
            "ts_rewrite query must return two tsquery columns"
        );
    }
    let pairs: Vec<(Option<TsQuery>, Option<TsQuery>)> = rows
        .iter()
        .map(|r| {
            let get = |v: &Value| match v {
                Value::TsQuery(q) => Some((**q).clone()),
                _ => None,
            };
            (get(&r[0]), get(&r[1]))
        })
        .collect();
    Ok(query(queryops::rewrite_rows(q, &pairs)))
}

// ── ranking ──

fn weights_arg(v: &Value) -> Result<[f32; 4]> {
    let Value::Array(arr) = v else {
        return rank::get_weights(0, &[]);
    };
    let items: Vec<Option<f32>> = arr
        .items
        .iter()
        .map(|x| x.as_f64().map(|f| f as f32))
        .collect();
    rank::get_weights(arr.ndims(), &items)
}

fn rank_args(a: &[Value], with_weights: bool) -> Result<([f32; 4], usize)> {
    if with_weights {
        Ok((weights_arg(&a[0])?, 1))
    } else {
        Ok((rank::DEFAULT_WEIGHTS, 0))
    }
}

fn method_arg(a: &[Value], at: usize) -> i32 {
    a.get(at).and_then(Value::as_i64).unwrap_or(0) as i32
}

fn ts_rank_w(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (w, i) = rank_args(a, true)?;
    Ok(Value::Float4(rank::rank(
        &w,
        tsv(&a[i]),
        tsq(&a[i + 1]),
        method_arg(a, i + 2),
    )))
}

fn ts_rank(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (w, i) = rank_args(a, false)?;
    Ok(Value::Float4(rank::rank(
        &w,
        tsv(&a[i]),
        tsq(&a[i + 1]),
        method_arg(a, i + 2),
    )))
}

fn ts_rank_cd_w(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (w, i) = rank_args(a, true)?;
    Ok(Value::Float4(rank::rank_cd(
        &w,
        tsv(&a[i]),
        tsq(&a[i + 1]),
        method_arg(a, i + 2),
    )?))
}

fn ts_rank_cd(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (w, i) = rank_args(a, false)?;
    Ok(Value::Float4(rank::rank_cd(
        &w,
        tsv(&a[i]),
        tsq(&a[i + 1]),
        method_arg(a, i + 2),
    )?))
}

// ── headlines ──

/// The configuration and its parser, which must have a headline function.
fn headline_config(l: &Lookup, cfg_oid: u32) -> Result<Config> {
    let cfg = l.config(cfg_oid)?;
    if l.parser(cfg.parser)?.headline == 0 {
        bail!(
            FEATURE_NOT_SUPPORTED,
            "text search parser does not support headline creation"
        );
    }
    Ok(cfg)
}

/// `ts_headline_byid_opt`.
fn headline_text(
    cfg_oid: u32,
    doc: &str,
    q: &TsQuery,
    opts: Option<&str>,
    c: &FnCtx,
) -> Result<Value> {
    let l = Lookup::new(c);
    let cfg = headline_config(&l, cfg_oid)?;
    let mut prs = HlText::default();
    config::hlparsetext(&cfg, &mut prs, q, doc, &mut notice_sink(c))?;
    let opts = match opts {
        Some(o) => headline::deserialize_deflist(o)?,
        None => Vec::new(),
    };
    headline::prsd_headline(&mut prs, &opts, q)?;
    Ok(text(headline::generate(&prs)))
}

/// One json string value's headline (`headline_json_value`): the parse state's position
/// carries over from value to value.
fn headline_value(
    cfg: &Config,
    prs: &mut HlText,
    q: &TsQuery,
    opts: &[(String, String)],
    s: &str,
    c: &FnCtx,
) -> Result<String> {
    prs.words.clear();
    config::hlparsetext(cfg, prs, q, s, &mut notice_sink(c))?;
    headline::prsd_headline(prs, opts, q)?;
    Ok(headline::generate(prs))
}

fn transform_jsonb(j: &Jsonb, f: &mut dyn FnMut(&str) -> Result<String>) -> Result<Jsonb> {
    crate::stack::guard(|| transform_jsonb_here(j, f))
}

fn transform_jsonb_here(j: &Jsonb, f: &mut dyn FnMut(&str) -> Result<String>) -> Result<Jsonb> {
    Ok(match j {
        Jsonb::String(s) => Jsonb::String(f(s)?),
        Jsonb::Array(items) => Jsonb::Array(
            items
                .iter()
                .map(|v| transform_jsonb(v, f))
                .collect::<Result<_>>()?,
        ),
        Jsonb::Object(m) => Jsonb::Object(
            m.iter()
                .map(|(k, v)| Ok((k.clone(), transform_jsonb(v, f)?)))
                .collect::<Result<_>>()?,
        ),
        other => other.clone(),
    })
}

/// `transform_json_string_values`: the json rewritten without whitespace, strings replaced.
fn transform_json(
    s: &str,
    out: &mut String,
    f: &mut dyn FnMut(&str) -> Result<String>,
) -> Result<()> {
    match raw_typeof(s) {
        "object" => {
            out.push('{');
            for (i, (k, v)) in raw_object(s).unwrap_or_default().into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(out, &k);
                out.push(':');
                transform_json(v, out, f)?;
            }
            out.push('}');
        }
        "array" => {
            out.push('[');
            for (i, v) in raw_array(s).unwrap_or_default().into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                transform_json(v, out, f)?;
            }
            out.push(']');
        }
        "string" => write_json_string(out, &f(&unescape_json_string(s.trim()))?),
        _ => out.push_str(s.trim()),
    }
    Ok(())
}

/// `ts_headline_json(b)_byid_opt`.
fn headline_json(
    cfg_oid: u32,
    doc: &Value,
    q: &TsQuery,
    opts: Option<&str>,
    c: &FnCtx,
) -> Result<Value> {
    let l = Lookup::new(c);
    let cfg = l.config(cfg_oid)?;
    let opts = match opts {
        Some(o) => headline::deserialize_deflist(o)?,
        None => Vec::new(),
    };
    if l.parser(cfg.parser)?.headline == 0 {
        bail!(
            FEATURE_NOT_SUPPORTED,
            "text search parser does not support headline creation"
        );
    }
    let mut prs = HlText::default();
    let mut f = |s: &str| headline_value(&cfg, &mut prs, q, &opts, s, c);
    match doc {
        Value::Jsonb(j) => Ok(Value::Jsonb(Arc::new(transform_jsonb(j, &mut f)?))),
        v => {
            let mut out = String::new();
            transform_json(text_arg(v), &mut out, &mut f)?;
            Ok(Value::Json(out.into()))
        }
    }
}

fn headline_dispatch(cfg: u32, a: &[Value], c: &FnCtx) -> Result<Value> {
    let opts = a.get(2).map(text_arg);
    match &a[0] {
        Value::Jsonb(_) | Value::Json(_) => headline_json(cfg, &a[0], tsq(&a[1]), opts, c),
        v => headline_text(cfg, text_arg(v), tsq(&a[1]), opts, c),
    }
}

fn ts_headline_byid(a: &[Value], c: &FnCtx) -> Result<Value> {
    headline_dispatch(oid_arg(&a[0]), &a[1..], c)
}

fn ts_headline_cur(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cfg = Lookup::new(c).current(c)?;
    headline_dispatch(cfg, a, c)
}

// ── dictionaries and parsers ──

/// `ts_lexize`'s call: a second one with `isend` set when the dictionary asked for more.
fn lexize_alone(d: &config::Dict, word: &str) -> Result<Lexized> {
    let mut st = DictSubState::default();
    let mut res = d.lexize(word, Some(&mut st))?;
    if st.getnext {
        st.isend = true;
        if let Some(r) = d.lexize(word, Some(&mut st))? {
            res = Some(r);
        }
    }
    Ok(res)
}

fn lexemes_value(r: Lexized) -> Value {
    match r {
        None => Value::Null,
        Some(ls) => text_array(ls.into_iter().map(|l| text(l.lexeme)).collect()),
    }
}

fn ts_lexize(a: &[Value], c: &FnCtx) -> Result<Value> {
    let l = Lookup::new(c);
    let d = lookup_dict(&l.ts(), oid_arg(&a[0]))?;
    Ok(lexemes_value(lexize_alone(&d, text_arg(&a[1]))?))
}

fn token_type_rows() -> SrfIter {
    let rows: Vec<Result<Vec<Value>>> = (1..=parser::LASTNUM as usize)
        .map(|i| {
            Ok(vec![
                Value::Int4(i as i32),
                text(parser::TOK_ALIAS[i]),
                text(parser::LEX_DESCR[i]),
            ])
        })
        .collect();
    Box::new(rows.into_iter())
}

fn ts_token_type_byid(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    Lookup::new(c).parser(oid_arg(&a[0]))?;
    Ok(token_type_rows())
}

fn ts_token_type_byname(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let l = Lookup::new(c);
    l.parser(l.parser_named(text_arg(&a[0]))?)?;
    Ok(token_type_rows())
}

fn parse_rows(txt: &str) -> SrfIter {
    let rows: Vec<Result<Vec<Value>>> = parser::tokens(txt)
        .into_iter()
        .map(|(t, s)| Ok(vec![Value::Int4(t as i32), text(s)]))
        .collect();
    Box::new(rows.into_iter())
}

fn ts_parse_byid(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    Lookup::new(c).parser(oid_arg(&a[0]))?;
    Ok(parse_rows(text_arg(&a[1])))
}

fn ts_parse_byname(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let l = Lookup::new(c);
    l.parser(l.parser_named(text_arg(&a[0]))?)?;
    Ok(parse_rows(text_arg(&a[1])))
}

/// `ts_debug`: each token with the dictionaries mapped for its type, the first that knows
/// it and its lexemes.
fn debug_rows(cfg_oid: u32, doc: &str, c: &FnCtx) -> Result<SrfIter> {
    let l = Lookup::new(c);
    let ts = l.ts();
    let Some(def) = ts.config(cfg_oid) else {
        return Ok(Box::new(std::iter::empty()));
    };
    l.parser(def.parser)?;
    let mut rows = Vec::new();
    for (ty, tok) in parser::tokens(doc) {
        let dicts: &[u32] = def.map.get(ty as usize).map_or(&[], |v| v.as_slice());
        let mut results = Vec::with_capacity(dicts.len());
        for &d in dicts {
            let dict = lookup_dict(&ts, d)?;
            results.push(lexize_alone(&dict, tok)?);
        }
        let pick = results
            .iter()
            .position(Option::is_some)
            .or(if dicts.is_empty() { None } else { Some(0) });
        let regdict = |o: u32| Value::Oid(o);
        rows.push(Ok(vec![
            text(parser::TOK_ALIAS[ty as usize]),
            text(parser::LEX_DESCR[ty as usize]),
            text(tok),
            Value::Array(Arc::new(ArrayValue::from_vec(
                T::Regdictionary,
                dicts.iter().map(|d| regdict(*d)).collect(),
            ))),
            pick.map_or(Value::Null, |i| regdict(dicts[i])),
            pick.map_or(Value::Null, |i| lexemes_value(results[i].clone())),
        ]));
    }
    Ok(Box::new(rows.into_iter()))
}

fn ts_debug_byid(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    debug_rows(oid_arg(&a[0]), text_arg(&a[1]), c)
}

fn ts_debug_cur(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let cfg = Lookup::new(c).current(c)?;
    debug_rows(cfg, text_arg(&a[0]), c)
}

/// `ts_stat`: lexeme statistics over a query's tsvector column.
fn ts_stat(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let (rows, cols) = crate::plpgsql::spi_query(c.exec, text_arg(&a[0]))?;
    if cols.len() != 1 || cols[0].ty != T::TsVector {
        bail!(
            INVALID_PARAMETER_VALUE,
            "ts_stat query must return one tsvector column"
        );
    }
    let mut stat = vecops::Stat::new(a.get(1).map(text_arg));
    for r in &rows {
        if let Value::TsVector(v) = &r[0] {
            stat.add(v);
        }
    }
    let out: Vec<Result<Vec<Value>>> = stat
        .rows()
        .into_iter()
        .map(|(w, ndoc, nentry)| Ok(vec![text(w), Value::Int4(ndoc), Value::Int4(nentry)]))
        .collect();
    Ok(Box::new(out.into_iter()))
}

// ── tsvector_update_trigger ──

/// `tsvector_update_trigger` / `tsvector_update_trigger_column`: NEW's tsvector column made
/// from its text columns. `set_cols`: the columns an UPDATE assigns.
#[allow(clippy::too_many_arguments)]
pub fn update_trigger(
    by_column: bool,
    t: &Table,
    args: &[String],
    before: bool,
    row: bool,
    event: u16,
    new: Option<crate::exec::Row>,
    set_cols: &[usize],
    ctx: &crate::exec::ExecCtx,
) -> Result<Option<crate::exec::Row>> {
    use crate::catalog::trigger_type as tt;
    let internal = |m: &str| {
        Err(crate::error::internal(format!(
            "tsvector_update_trigger: {m}"
        )))
    };
    if !row {
        return internal("must be fired for row");
    }
    if !before {
        return internal("must be fired BEFORE event");
    }
    let (mut new, mut update_needed) = if event & tt::INSERT != 0 {
        (new.unwrap_or_default(), true)
    } else if event & tt::UPDATE != 0 {
        (new.unwrap_or_default(), false)
    } else {
        return internal("must be fired for INSERT or UPDATE");
    };
    if args.len() < 3 {
        return internal("arguments must be tsvector_field, ts_config, text_field1, ...)");
    }
    let col = |name: &str| {
        t.live_columns()
            .find(|(_, c)| c.name == name)
            .map(|(i, c)| (i, c.ty.clone()))
    };
    let Some((target, ty)) = col(&args[0]) else {
        bail!(
            UNDEFINED_COLUMN,
            "tsvector column \"{}\" does not exist",
            args[0]
        );
    };
    if ty != T::TsVector {
        bail!(
            DATATYPE_MISMATCH,
            "column \"{}\" is not of tsvector type",
            args[0]
        );
    }
    let cat = ctx.catalog();
    let cfg_oid = if by_column {
        let Some((i, ty)) = col(&args[1]) else {
            bail!(
                UNDEFINED_COLUMN,
                "configuration column \"{}\" does not exist",
                args[1]
            );
        };
        if ty != T::Regconfig {
            bail!(
                DATATYPE_MISMATCH,
                "column \"{}\" is not of regconfig type",
                args[1]
            );
        }
        match &new[i] {
            Value::Null => bail!(
                NULL_VALUE_NOT_ALLOWED,
                "configuration column \"{}\" must not be null",
                args[1]
            ),
            v => oid_arg(v),
        }
    } else {
        let names = text_to_names(&args[1])?;
        if names.len() < 2 {
            bail!(
                INVALID_PARAMETER_VALUE,
                "text search configuration name \"{}\" must be schema-qualified",
                args[1]
            );
        }
        cat.ts_lookup(TsKind::Config, &names, &ctx.search_path, false)?
            .unwrap_or(0)
    };
    let lookup = TsCatalog {
        catalog: &cat,
        search_path: &ctx.search_path,
    };
    let cfg = lookup_config(&lookup, cfg_oid)?;
    let mut prs = ParsedText::default();
    let mut sink = |mut e: Error| {
        e.severity = Severity::Notice;
        ctx.notice(e)
    };
    for name in &args[2..] {
        let Some((i, ty)) = col(name) else {
            bail!(UNDEFINED_COLUMN, "column \"{name}\" does not exist");
        };
        if !matches!(ty, T::Text | T::Varchar(_)) {
            bail!(
                DATATYPE_MISMATCH,
                "column \"{name}\" is not of a character type"
            );
        }
        if set_cols.contains(&i) {
            update_needed = true;
        }
        if new[i].is_null() {
            continue;
        }
        config::parsetext(&cfg, &mut prs, text_arg(&new[i]), &mut sink)?;
    }
    if update_needed {
        new[target] = vector(config::make_tsvector(prs)?);
    }
    Ok(Some(new))
}

pub fn register(r: &mut Registry) {
    use T::{
        Bool, Char, Float4, Int2, Int4, Json, Jsonb as Jb, Oid, Regconfig, Regdictionary, Text,
        TsQuery as Q, TsVector as V,
    };
    let text_arr = T::array_of(Text);
    let char_arr = T::array_of(Char);
    let real_arr = T::array_of(Float4);

    r.func("to_tsvector", vec![Regconfig, Text], V, to_tsvector_byid);
    r.func("to_tsvector", vec![Text], V, to_tsvector_cur)
        .stable();
    r.func("to_tsvector", vec![Jb], V, json_string_tsvector)
        .stable();
    r.func("to_tsvector", vec![Json], V, json_string_tsvector)
        .stable();
    r.func(
        "to_tsvector",
        vec![Regconfig, Jb],
        V,
        json_string_tsvector_byid,
    );
    r.func(
        "to_tsvector",
        vec![Regconfig, Json],
        V,
        json_string_tsvector_byid,
    );
    r.func("jsonb_to_tsvector", vec![Jb, Jb], V, json_to_tsvector)
        .stable();
    r.func(
        "jsonb_to_tsvector",
        vec![Regconfig, Jb, Jb],
        V,
        json_to_tsvector_byid,
    );
    r.func("json_to_tsvector", vec![Json, Jb], V, json_to_tsvector)
        .stable();
    r.func(
        "json_to_tsvector",
        vec![Regconfig, Json, Jb],
        V,
        json_to_tsvector_byid,
    );

    r.func("to_tsquery", vec![Regconfig, Text], Q, to_tsquery_byid);
    r.func("to_tsquery", vec![Text], Q, to_tsquery_cur).stable();
    r.func(
        "plainto_tsquery",
        vec![Regconfig, Text],
        Q,
        plainto_tsquery_byid,
    );
    r.func("plainto_tsquery", vec![Text], Q, plainto_tsquery_cur)
        .stable();
    r.func(
        "phraseto_tsquery",
        vec![Regconfig, Text],
        Q,
        phraseto_tsquery_byid,
    );
    r.func("phraseto_tsquery", vec![Text], Q, phraseto_tsquery_cur)
        .stable();
    r.func(
        "websearch_to_tsquery",
        vec![Regconfig, Text],
        Q,
        websearch_to_tsquery_byid,
    );
    r.func(
        "websearch_to_tsquery",
        vec![Text],
        Q,
        websearch_to_tsquery_cur,
    )
    .stable();
    r.func(
        "get_current_ts_config",
        vec![],
        Regconfig,
        get_current_ts_config,
    )
    .stable();

    r.binop("@@", V, Q, Bool, ts_match_vq);
    r.binop("@@", Q, V, Bool, ts_match_qv);
    r.binop("@@@", V, Q, Bool, ts_match_vq);
    r.binop("@@@", Q, V, Bool, ts_match_qv);
    r.binop("@@", Text, Q, Bool, ts_match_tq);
    r.binop("@@", Text, Text, Bool, ts_match_tt);
    r.func("ts_match_tq", vec![Text, Q], Bool, ts_match_tq)
        .stable();
    r.func("ts_match_tt", vec![Text, Text], Bool, ts_match_tt)
        .stable();

    super::ops::register_cmp(r, V, V);
    super::ops::register_cmp(r, Q, Q);
    r.func("tsvector_cmp", vec![V, V], Int4, tsvector_cmp);
    r.func("tsquery_cmp", vec![Q, Q], Int4, tsquery_cmp);

    r.func("setweight", vec![V, Char], V, setweight);
    r.func(
        "setweight",
        vec![V, Char, text_arr.clone()],
        V,
        setweight_by_filter,
    );
    r.func("strip", vec![V], V, strip);
    r.func("length", vec![V], Int4, tsvector_length);
    r.binop("||", V, V, V, tsvector_concat);
    r.func("ts_delete", vec![V, Text], V, ts_delete_str);
    r.func("ts_delete", vec![V, text_arr.clone()], V, ts_delete_arr);
    r.func("ts_filter", vec![V, char_arr], V, ts_filter);
    r.func(
        "tsvector_to_array",
        vec![V],
        text_arr.clone(),
        tsvector_to_array,
    );
    r.func(
        "array_to_tsvector",
        vec![text_arr.clone()],
        V,
        array_to_tsvector,
    );
    r.srf_cols(
        "unnest",
        vec![V],
        vec![
            ("lexeme", Text),
            ("positions", T::array_of(Int2)),
            ("weights", text_arr.clone()),
        ],
        tsvector_unnest,
    )
    .names(&["tsvector"]);

    r.binop("&&", Q, Q, Q, tsquery_and);
    r.binop("||", Q, Q, Q, tsquery_or);
    r.prefix_op("!!", Q, Q, tsquery_not);
    r.binop("<->", Q, Q, Q, tsquery_phrase);
    r.func("tsquery_phrase", vec![Q, Q], Q, tsquery_phrase);
    r.func(
        "tsquery_phrase",
        vec![Q, Q, Int4],
        Q,
        tsquery_phrase_distance,
    );
    r.func("numnode", vec![Q], Int4, numnode);
    r.func("querytree", vec![Q], Text, querytree);
    r.binop("@>", Q, Q, Bool, tsq_mcontains);
    r.binop("<@", Q, Q, Bool, tsq_mcontained);
    r.func("tsq_mcontains", vec![Q, Q], Bool, tsq_mcontains);
    r.func("tsq_mcontained", vec![Q, Q], Bool, tsq_mcontained);
    r.func("ts_rewrite", vec![Q, Q, Q], Q, tsquery_rewrite);
    r.func("ts_rewrite", vec![Q, Text], Q, tsquery_rewrite_query)
        .volatile();

    r.func(
        "ts_rank",
        vec![real_arr.clone(), V, Q, Int4],
        Float4,
        ts_rank_w,
    );
    r.func("ts_rank", vec![real_arr.clone(), V, Q], Float4, ts_rank_w);
    r.func("ts_rank", vec![V, Q, Int4], Float4, ts_rank);
    r.func("ts_rank", vec![V, Q], Float4, ts_rank);
    r.func(
        "ts_rank_cd",
        vec![real_arr.clone(), V, Q, Int4],
        Float4,
        ts_rank_cd_w,
    );
    r.func("ts_rank_cd", vec![real_arr, V, Q], Float4, ts_rank_cd_w);
    r.func("ts_rank_cd", vec![V, Q, Int4], Float4, ts_rank_cd);
    r.func("ts_rank_cd", vec![V, Q], Float4, ts_rank_cd);

    for doc in [Text, Jb, Json] {
        r.func(
            "ts_headline",
            vec![Regconfig, doc.clone(), Q, Text],
            doc.clone(),
            ts_headline_byid,
        );
        r.func(
            "ts_headline",
            vec![Regconfig, doc.clone(), Q],
            doc.clone(),
            ts_headline_byid,
        );
        r.func(
            "ts_headline",
            vec![doc.clone(), Q, Text],
            doc.clone(),
            ts_headline_cur,
        )
        .stable();
        r.func("ts_headline", vec![doc.clone(), Q], doc, ts_headline_cur)
            .stable();
    }

    r.func(
        "ts_lexize",
        vec![Regdictionary, Text],
        text_arr.clone(),
        ts_lexize,
    );
    let tt_cols = vec![("tokid", Int4), ("alias", Text), ("description", Text)];
    r.srf_cols(
        "ts_token_type",
        vec![Oid],
        tt_cols.clone(),
        ts_token_type_byid,
    )
    .names(&["parser_oid"]);
    r.srf_cols("ts_token_type", vec![Text], tt_cols, ts_token_type_byname)
        .names(&["parser_name"])
        .stable();
    let parse_cols = vec![("tokid", Int4), ("token", Text)];
    r.srf_cols(
        "ts_parse",
        vec![Oid, Text],
        parse_cols.clone(),
        ts_parse_byid,
    )
    .names(&["parser_oid", "txt"]);
    r.srf_cols("ts_parse", vec![Text, Text], parse_cols, ts_parse_byname)
        .names(&["parser_name", "txt"])
        .stable();
    let debug_cols = vec![
        ("alias", Text),
        ("description", Text),
        ("token", Text),
        ("dictionaries", T::array_of(Regdictionary)),
        ("dictionary", Regdictionary),
        ("lexemes", text_arr),
    ];
    r.srf_cols(
        "ts_debug",
        vec![Regconfig, Text],
        debug_cols.clone(),
        ts_debug_byid,
    )
    .names(&["config", "document"])
    .stable();
    r.srf_cols("ts_debug", vec![Text], debug_cols, ts_debug_cur)
        .names(&["document"])
        .stable();
    let stat_cols = vec![("word", Text), ("ndoc", Int4), ("nentry", Int4)];
    r.srf_cols("ts_stat", vec![Text], stat_cols.clone(), ts_stat)
        .names(&["query"])
        .volatile();
    r.srf_cols("ts_stat", vec![Text, Text], stat_cols, ts_stat)
        .names(&["query", "weights"])
        .volatile();

    // The I/O functions of gtsvector (the GiST signature) and the BRIN summary types. Their
    // input functions reject every value, so the strict output and send functions only
    // ever see NULLs; the receive functions take `internal` and can't be called from SQL.
    for oid in [3646, 4596, 4638] {
        r.from_pg(oid, Some(reject_input), None);
    }
    for oid in [3647, 4597, 4599, 4639, 4641] {
        r.from_pg(oid, Some(|_, _| Ok(Value::Null)), None);
    }
}

/// gtsvectorin, brin_bloom_summary_in, brin_minmax_multi_summary_in.
fn reject_input(a: &[Value], c: &FnCtx) -> Result<Value> {
    crate::types::text::parse_value(text_arg(&a[0]), c.ret, c.exec.fmt())
}
