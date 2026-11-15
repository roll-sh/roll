//! `kql` against an index (`KqlQueryBuilder.doRewrite`, `KqlAstBuilder`): the parsed KQL tree
//! becomes the queries Elasticsearch builds for it, by each resolved field's type:
//!
//! - `and` / `or` / `not`: `bool` `must` / `should` (one must match) / `must_not`.
//! - `field: value` on each field the name (or `*` pattern) resolves to, one must match:
//!   a wildcard value is a `wildcard` query on keyword fields and a `query_string` elsewhere;
//!   otherwise dates take a `range` from the value to itself, keyword fields a `term`, quoted
//!   values a `match_phrase` and the rest a lenient `match`.
//! - A value without a field: a lenient `multi_match` (`phrase` when quoted), or a
//!   `query_string` when it has wildcards, over `default_field` (the index's default fields
//!   when unset).
//! - `field < v` …: `range`; `field: *`: `exists` (runtime fields left out).
//! - Fields under `nested` objects are wrapped in `nested` queries (`score_mode: none`) for
//!   every nested level below the enclosing `path: { … }`.
//!
//! `case_insensitive` applies to keyword `term` and `wildcard` queries, `time_zone` to date
//! ranges. A name that resolves to no field matches nothing.

use super::query::QueryCtx;
use crate::engine::fields::Kind;
use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::obj;
use crate::query::Query;
use crate::query::kql::{Kql, Node, Text, ValueExpr};

/// Metadata fields a KQL field name can name.
const META: &[&str] = &["_id", "_index", "_routing", "_ignored", "_tier", "_seq_no", "_version"];

/// The query a `kql` query stands for on this shard.
pub fn rewrite(ctx: &QueryCtx, k: &Kql) -> EsResult<Query> {
    let v = match crate::query::kql::parse(&k.query)? {
        None => obj! { "match_all" => obj! {} },
        Some(n) => Rewriter { ctx, k }.node(&n, "")?,
    };
    crate::query::parse_stored(&v)
}

struct Rewriter<'a, 'b> {
    ctx: &'a QueryCtx<'b>,
    k: &'a Kql,
}

fn bool_of(occur: &str, clauses: Vec<Value>) -> Value {
    let mut b = obj! { occur => Value::Arr(clauses) };
    if occur == "should"
        && let Value::Obj(m) = &mut b
    {
        m.insert("minimum_should_match".into(), 1.into());
    }
    obj! { "bool" => b }
}

/// `rewriteDisjunctionQuery`: nothing matches no field, one clause stands alone.
fn any_of(mut clauses: Vec<Value>) -> Value {
    match clauses.len() {
        0 => obj! { "match_none" => obj! {} },
        1 => clauses.remove(0),
        _ => bool_of("should", clauses),
    }
}

fn field_query(kind: &str, field: &str, body: Value) -> Value {
    obj! { kind => obj! { field => body } }
}

impl Rewriter<'_, '_> {
    fn node(&self, n: &Node, scope: &str) -> EsResult<Value> {
        Ok(match n {
            Node::And(xs) => bool_of("must", xs.iter().map(|x| self.node(x, scope)).collect::<EsResult<_>>()?),
            Node::Or(xs) => bool_of("should", xs.iter().map(|x| self.node(x, scope)).collect::<EsResult<_>>()?),
            Node::Not(x) => bool_of("must_not", vec![self.node(x, scope)?]),
            Node::MatchAll => obj! { "match_all" => obj! {} },
            Node::Exists(name) => {
                let mut fields = self.fields(scope, name, false);
                let full = qualified(scope, name);
                if fields.is_empty() && self.ctx.table.objects.contains_key(&full) {
                    fields.push((full, Kind::Other));
                }
                any_of(fields.into_iter().map(|(f, _)| self.nest(&f, scope, obj! { "exists" => obj! { "field" => f.as_str() } })).collect())
            }
            Node::Range { field, op, value } => any_of(
                self.fields(scope, field, true)
                    .into_iter()
                    .map(|(f, kind)| {
                        let mut body = obj! { *op => value.as_str() };
                        self.time_zone(kind, &mut body);
                        self.nest(&f, scope, field_query("range", &f, body))
                    })
                    .collect(),
            ),
            Node::Field { field, value } => {
                let mut out = Vec::new();
                for (f, kind) in self.fields(scope, field, true) {
                    let q = self.value(&f, kind, value);
                    out.push(self.nest(&f, scope, q));
                }
                any_of(out)
            }
            Node::FieldLess(t) => {
                let df = self.k.default_field.as_deref();
                if t.has_wildcard() {
                    let mut body = obj! { "query" => t.lucene_escaped() };
                    if let (Some(df), Value::Obj(m)) = (df, &mut body) {
                        m.insert("default_field".into(), df.into());
                    }
                    obj! { "query_string" => body }
                } else {
                    let ty = if t.quoted { "phrase" } else { "best_fields" };
                    let mut body = obj! { "query" => t.plain(), "type" => ty, "lenient" => true };
                    if let (Some(df), Value::Obj(m)) = (df, &mut body) {
                        m.insert("fields".into(), Value::Arr(vec![df.into()]));
                    }
                    obj! { "multi_match" => body }
                }
            }
            Node::Nested { path, query } => {
                let full = qualified(scope, path);
                if self.ctx.table.nested_of(&full).is_none() {
                    return Err(EsError::parsing(1, 1, format!("[{full}] is not a valid nested field name.")));
                }
                let inner = self.node(query, &full)?;
                let q = obj! { "nested" => obj! { "path" => full.as_str(), "query" => inner, "score_mode" => "none" } };
                self.nest(&full, scope, q)
            }
        })
    }

    /// A field query's value on one field.
    fn value(&self, field: &str, kind: Kind, v: &ValueExpr) -> Value {
        match v {
            ValueExpr::Leaf(t) => self.leaf(field, kind, t),
            ValueExpr::And(xs) => bool_of("must", xs.iter().map(|x| self.value(field, kind, x)).collect()),
            ValueExpr::Or(xs) => bool_of("should", xs.iter().map(|x| self.value(field, kind, x)).collect()),
            ValueExpr::Not(x) => bool_of("must_not", vec![self.value(field, kind, x)]),
        }
    }

    fn leaf(&self, field: &str, kind: Kind, t: &Text) -> Value {
        let keyword = matches!(kind, Kind::Keyword | Kind::ConstantKeyword | Kind::Wildcard);
        let ci = self.k.case_insensitive;
        if t.has_wildcard() && keyword {
            field_query("wildcard", field, obj! { "value" => t.wildcard_pattern(), "case_insensitive" => ci })
        } else if t.has_wildcard() {
            obj! { "query_string" => obj! { "query" => t.lucene_escaped(), "fields" => Value::Arr(vec![field.into()]) } }
        } else if kind.is_date() {
            let v = t.plain();
            let mut body = obj! { "gte" => v.as_str(), "lte" => v.as_str() };
            self.time_zone(kind, &mut body);
            field_query("range", field, body)
        } else if keyword {
            field_query("term", field, obj! { "value" => t.plain(), "case_insensitive" => ci })
        } else if t.quoted {
            field_query("match_phrase", field, obj! { "query" => t.plain() })
        } else {
            field_query("match", field, obj! { "query" => t.plain(), "lenient" => true })
        }
    }

    fn time_zone(&self, kind: Kind, body: &mut Value) {
        if let (Some(tz), true, Value::Obj(m)) = (&self.k.time_zone, kind.is_date(), body) {
            m.insert("time_zone".into(), tz.as_str().into());
        }
    }

    /// The fields a name (relative to the nested scope) resolves to, with their kinds.
    /// Runtime fields only by their exact name, and only when `runtime` is set.
    fn fields(&self, scope: &str, name: &str, runtime: bool) -> Vec<(String, Kind)> {
        let full = qualified(scope, name);
        let mut out: Vec<(String, Kind)> =
            self.ctx.table.matching(&full).into_iter().filter_map(|f| self.ctx.field(&f).map(|i| (f, i.kind))).collect();
        if out.is_empty() && !full.contains('*') {
            if runtime && let Some(rt) = self.ctx.runtime_field(&full) {
                use super::runtime::RtType;
                let kind = match rt.ty {
                    RtType::Keyword => Kind::Keyword,
                    RtType::Date => Kind::Date,
                    RtType::Long => Kind::Long,
                    RtType::Double => Kind::Double,
                    _ => Kind::Other,
                };
                out.push((full, kind));
            } else if META.contains(&full.as_str()) {
                out.push((full, Kind::Other));
            }
        }
        out
    }

    /// `wrapWithNestedQuery`: one `nested` query per nested object above `field` and below
    /// `scope`, innermost first.
    fn nest(&self, field: &str, scope: &str, mut q: Value) -> Value {
        let paths: Vec<&str> = field
            .match_indices('.')
            .map(|(i, _)| &field[..i])
            .filter(|p| p.len() > scope.len() && self.ctx.table.nested_of(p).is_some())
            .collect();
        for p in paths.into_iter().rev() {
            q = obj! { "nested" => obj! { "path" => p, "query" => q, "score_mode" => "none" } };
        }
        q
    }
}

fn qualified(scope: &str, name: &str) -> String {
    if scope.is_empty() { name.to_string() } else { format!("{scope}.{name}") }
}
