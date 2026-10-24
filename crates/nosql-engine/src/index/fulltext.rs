//! ScyllaDB full-text search: `SELECT ... WHERE BM25(col, 'term') > 0 ORDER BY BM25(col, 'term')
//! LIMIT n` over a `fulltext_index` (ScyllaDB 2026.3; `docs/features/fulltext-search.rst`).
//!
//! # Served like the reference without a Vector Store
//!
//! ScyllaDB answers full-text searches from its external Vector Store (a Tantivy index fed from
//! the CDC log). The reference image runs without one: a query that passes validation fails with
//! "Vector Store is disabled", and so does ours. Scoring natively would be possible (BM25 over
//! the base rows), but would answer where the reference errors, so the index is kept as DDL and
//! validation only; the CDC log it needs is created and maintained as on the reference.
//!
//! # Validation
//!
//! Learned from the reference, in its order where it shows one:
//! - `BM25()` is refused in the selection and outside SELECT; other functions in `ORDER BY`
//!   and function relations other than `token()` are refused;
//! - each call takes a column of a text type and a non-column text term;
//! - `WHERE BM25(...) > 0` (only `>`, only the literal 0), once, and `ORDER BY BM25(...)` alone
//!   (no direction, no other ordering, no ANN), on the same column with the same term, a
//!   `LIMIT` of at most 1000, no aggregation, `PER PARTITION LIMIT` or other restrictions, and a
//!   `fulltext_index` on the column;
//! - at execution, bound terms must be non-null and equal.

use nosql_cql::ast::{self, FunctionName, Literal, LiteralKind, Operator, Relation, Selectable};
use nosql_types::CqlType;

use super::{IndexClass, defs};
use crate::error::{Error, Result};
use crate::prepare::terms::{self, Bound, EvalCtx, Term};
use crate::prepare::{PrepareCtx, Receiver};
use crate::schema::Table;

/// Whether `f` names the built-in `bm25` (unqualified or `system.bm25`), without looking at user
/// functions (outside SELECT).
pub fn is_bm25(f: &FunctionName) -> bool {
    f.name.name() == "bm25" && f.keyspace.as_ref().is_none_or(|k| k.name() == "system")
}

/// Whether a call names the built-in `bm25`: `system.bm25`, or `bm25` unless a user function
/// `bm25(text, text)` of the keyspace makes the name ambiguous (an error).
pub fn resolves_to_bm25(f: &FunctionName, cx: &PrepareCtx) -> Result<bool> {
    if f.name.name() != "bm25" {
        return Ok(false);
    }
    if let Some(k) = &f.keyspace {
        return Ok(k.name() == "system");
    }
    let clash: Vec<String> = cx
        .user_functions
        .map(|u| u.get(&cx.keyspace, "bm25"))
        .unwrap_or_default()
        .into_iter()
        .filter(|u| u.arg_types() == [CqlType::Text, CqlType::Text])
        .map(|u| u.signature(nosql_types::Dialect::Cassandra))
        .collect();
    if !clash.is_empty() {
        return Err(Error::invalid(format!(
            "Ambiguous call to function bm25 (can be matched by following signatures: system.bm25 : (text, text) -> float, {}): use type casts to disambiguate",
            clash.join(", ")
        )));
    }
    Ok(true)
}

/// A `BM25(column, term)` call, prepared: the column (table index), the term as written (for
/// comparing literal terms) and prepared.
#[derive(Clone, Debug)]
pub struct Call {
    pub column: usize,
    pub written: ast::Term<'static>,
    pub term: Term,
}

/// The full-text search of a prepared SELECT.
#[derive(Clone, Debug)]
pub struct Bm25Plan {
    pub column: usize,
    pub filter: Term,
    pub order: Term,
}

/// The selection may not call `BM25()`.
pub fn check_selectors(selectors: &[ast::Selector], cx: &PrepareCtx) -> Result<()> {
    fn has_bm25(s: &Selectable, cx: &PrepareCtx) -> Result<bool> {
        Ok(match s {
            Selectable::Function(f, args) => {
                if resolves_to_bm25(f, cx)? {
                    return Ok(true);
                }
                for a in args {
                    if has_bm25(a, cx)? {
                        return Ok(true);
                    }
                }
                false
            }
            Selectable::Cast(c) => has_bm25(&c.arg, cx)?,
            _ => false,
        })
    }
    for s in selectors {
        if has_bm25(&s.expr, cx)? {
            return Err(Error::invalid(
                "BM25() is not supported in the SELECT clause",
            ));
        }
    }
    Ok(())
}

/// `err`, or "Unknown function" when `f` names no native function (checked first).
fn unknown_or(f: &FunctionName, cx: &PrepareCtx, err: Error) -> Error {
    let native = f.keyspace.as_ref().is_none_or(|k| k.name() == "system");
    if native && !cx.functions.exists(&f.name.name()) {
        let ks = f.keyspace.as_ref().map(|k| k.name().into_owned());
        return terms::unknown_function(cx.dialect, ks.as_deref(), &f.name.name());
    }
    err
}

/// The error for an `ORDER BY` function other than `BM25()`.
pub fn other_order_function(f: &FunctionName, cx: &PrepareCtx) -> Error {
    unknown_or(
        f,
        cx,
        Error::invalid("Only BM25 scoring function is supported in ORDER BY"),
    )
}

/// The error for a function relation outside a SELECT's full-text search.
pub fn function_relation_error(r: &ast::FunctionRelation) -> Error {
    if is_bm25(&r.function) {
        Error::invalid("BM25() is only supported in SELECT statements")
    } else {
        Error::invalid(
            "Only the token function and scoring functions are supported in function-call restrictions",
        )
    }
}

/// Prepares the arguments of a `BM25()` call.
pub fn call(table: &Table, args: &[Selectable], cx: &mut PrepareCtx) -> Result<Call> {
    if args.len() != 2 {
        return Err(Error::invalid(format!(
            "Invalid number of arguments in call to function system.bm25: 2 required but {} provided",
            args.len()
        )));
    }
    let Selectable::Column(id) = &args[0] else {
        return Err(Error::invalid(
            "First argument to BM25 must be a column reference",
        ));
    };
    let name = id.name();
    let Some(column) = table.column_index(&name) else {
        return Err(Error::invalid(format!("Unrecognized name {name}")));
    };
    if !matches!(table.columns[column].ty, CqlType::Text | CqlType::Ascii) {
        return Err(Error::invalid(format!(
            "Type error: {id} cannot be passed as argument 0 of function system.bm25 of type text"
        )));
    }
    let written = match &args[1] {
        Selectable::Column(_) => {
            return Err(Error::invalid(
                "Second argument to BM25() must not be a column reference",
            ));
        }
        Selectable::Term(t) => t.clone(),
        other => {
            return Err(Error::invalid(format!(
                "Type error: {other} cannot be passed as argument 1 of function system.bm25 of type text"
            )));
        }
    };
    if let ast::Term::Literal(l) = &written
        && l.kind != LiteralKind::String
    {
        return Err(Error::invalid(format!(
            "Type error: {written} cannot be passed as argument 1 of function system.bm25 of type text"
        )));
    }
    let receiver = Receiver::new(
        &table.keyspace,
        &table.name,
        "arg1(system.bm25)",
        CqlType::Text,
    );
    let term = terms::prepare(&written, &receiver, cx)?;
    Ok(Call {
        column,
        written: nosql_cql::IntoStatic::into_static(written),
        term,
    })
}

/// Whether `t` is the literal 0 (`0`, `0.0`, `-0`).
fn literal_zero(t: &ast::Term) -> bool {
    match t {
        ast::Term::Literal(Literal {
            kind: LiteralKind::Integer | LiteralKind::Float,
            text,
            ..
        }) => text.parse::<f64>().is_ok_and(|v| v == 0.0),
        ast::Term::Negate(t) => literal_zero(t),
        _ => false,
    }
}

/// Splits a SELECT's WHERE clause into its `BM25()` restrictions (prepared and checked) and the
/// other relations; refuses other function relations.
pub fn split_where<'r, 'a>(
    table: &Table,
    rels: &'r [Relation<'a>],
    cx: &mut PrepareCtx,
) -> Result<(Vec<Call>, Vec<&'r Relation<'a>>)> {
    let mut calls = Vec::new();
    let mut rest = Vec::new();
    for rel in rels {
        let Relation::Function(f) = rel else {
            rest.push(rel);
            continue;
        };
        if !resolves_to_bm25(&f.function, cx)? {
            return Err(unknown_or(&f.function, cx, function_relation_error(f)));
        }
        let shown = format!(
            "{}({})",
            f.function.name.name(),
            f.args
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        match f.op {
            Operator::Gt => {}
            Operator::Neq => {
                return Err(Error::invalid(format!(
                    "Unsupported \"!=\" relation: {shown} != {}",
                    f.value
                )));
            }
            Operator::Like => {
                return Err(Error::invalid(format!(
                    "LIKE is allowed only on string types, which system.{shown} is not"
                )));
            }
            op => {
                return Err(Error::invalid(format!(
                    "Unsupported \"{}\" relation for BM25 function restriction, only \">\" is supported",
                    op.as_str()
                )));
            }
        }
        if !literal_zero(&f.value) {
            return Err(Error::invalid(
                "BM25 function comparison value must be the literal 0",
            ));
        }
        calls.push(call(table, &f.args, cx)?);
    }
    if calls.len() > 1 {
        return Err(Error::invalid(
            "Full-text search queries support only one WHERE BM25() restriction",
        ));
    }
    Ok((calls, rest))
}

/// What the rest of a SELECT says about a full-text search.
pub struct Query<'q> {
    pub table: &'q Table,
    pub filter: Option<Call>,
    pub order: Option<Call>,
    pub ann: bool,
    pub aggregate: bool,
    pub limit: Option<&'q ast::Term<'q>>,
    pub per_partition_limit: bool,
    /// Restrictions other than `BM25()`.
    pub others: usize,
}

/// The structural checks of a full-text search (`None`: the query isn't one).
pub fn plan(q: Query) -> Result<Option<Bm25Plan>> {
    if q.filter.is_none() && q.order.is_none() {
        return Ok(None);
    }
    if q.aggregate {
        return Err(Error::invalid(
            "Full-text search queries cannot be run with aggregation",
        ));
    }
    let Some(limit) = q.limit else {
        return Err(Error::invalid("Full-text search queries require a LIMIT"));
    };
    if let ast::Term::Literal(Literal {
        kind: LiteralKind::Integer,
        text,
        ..
    }) = limit
        && let Ok(n) = text.parse::<i64>()
    {
        check_limit(n)?;
    }
    let (filter, order) = match (q.filter, q.order) {
        (Some(_), None) if q.ann => {
            return Err(Error::invalid(
                "BM25 and ANN cannot be combined in the same query",
            ));
        }
        (Some(_), None) => {
            return Err(Error::invalid(
                "Full-text search queries require an ORDER BY BM25() clause",
            ));
        }
        (None, Some(_)) => {
            return Err(Error::invalid(
                "Full-text search queries require a WHERE BM25() > 0 clause",
            ));
        }
        (Some(f), Some(o)) => (f, o),
        (None, None) => return Ok(None),
    };
    if filter.column != order.column {
        return Err(Error::invalid(
            "Full-text search queries must reference the same column in both WHERE and ORDER BY clauses",
        ));
    }
    let bound = |t: &ast::Term| matches!(t, ast::Term::Bind(_));
    if !bound(&filter.written) && !bound(&order.written) && filter.written != order.written {
        return Err(same_term());
    }
    if q.per_partition_limit {
        return Err(Error::invalid(
            "Full-text search queries do not support per-partition limits",
        ));
    }
    if q.others > 0 {
        return Err(Error::invalid(
            "Full-text search queries do not support additional WHERE restrictions",
        ));
    }
    let indexed = defs(q.table)
        .iter()
        .any(|d| d.class == IndexClass::Fulltext && d.column == filter.column);
    if !indexed {
        return Err(Error::invalid(
            "No fulltext index found for full-text search query",
        ));
    }
    Ok(Some(Bm25Plan {
        column: filter.column,
        filter: filter.term,
        order: order.term,
    }))
}

fn same_term() -> Error {
    Error::invalid(
        "Full-text search queries must use the same search term in both WHERE and ORDER BY clauses",
    )
}

fn check_limit(n: i64) -> Result<()> {
    if n > 1000 {
        return Err(Error::invalid(format!(
            "Full-text search queries require a LIMIT that is not greater than 1000. LIMIT was {n}"
        )));
    }
    Ok(())
}

/// Execution: the bound terms (non-null, equal) and LIMIT, then the reference's answer without a
/// Vector Store.
pub fn execute(p: &Bm25Plan, cx: &EvalCtx<'_>, limit: Option<u64>) -> Result<()> {
    let value = |t: &Term| -> Result<Option<bytes::Bytes>> {
        Ok(match t.eval(cx)? {
            Bound::Val(v) => Some(v),
            Bound::Null | Bound::Unset => None,
        })
    };
    let (Some(f), Some(o)) = (value(&p.filter)?, value(&p.order)?) else {
        return Err(Error::invalid(
            "Full-text search query term must not be null",
        ));
    };
    if f != o {
        return Err(same_term());
    }
    if let Some(n) = limit {
        check_limit(i64::try_from(n).unwrap_or(i64::MAX))?;
    }
    Err(Error::invalid("Vector Store is disabled"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_zeros() {
        let lit = |kind, text: &'static str| {
            ast::Term::Literal(Literal {
                kind,
                text: text.into(),
                dollar: false,
            })
        };
        assert!(literal_zero(&lit(LiteralKind::Integer, "0")));
        assert!(literal_zero(&lit(LiteralKind::Float, "0.0")));
        assert!(literal_zero(&ast::Term::Negate(Box::new(lit(
            LiteralKind::Integer,
            "0"
        )))));
        assert!(!literal_zero(&lit(LiteralKind::Integer, "5")));
        assert!(!literal_zero(&lit(LiteralKind::String, "0")));
        assert!(!literal_zero(&ast::Term::Null));
    }
}
