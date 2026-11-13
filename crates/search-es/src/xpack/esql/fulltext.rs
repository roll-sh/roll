//! Full-text functions: `MATCH`, `MATCH_PHRASE`, `MULTI_MATCH`, `QSTR`, `KQL` and the
//! `field : query` operator.
//!
//! Elasticsearch evaluates them inside Lucene, so they see analyzed text and can be combined
//! with any other condition (`OR`, `NOT`, after `SORT`). roll does the same through the search
//! the `FROM` scan already runs: every distinct full-text call becomes a named `should` clause
//! of the scan's query, and the call itself is rewritten to a hidden boolean column
//! (`$ft<i>`) filled from each hit's `matched_queries`. Row-level evaluation then just reads
//! the column, so any boolean structure around the call is evaluated exactly; the scan's
//! scores (`METADATA _score`) come from the same clauses. Calls are only allowed where
//! Elasticsearch allows them (`WHERE`, `STATS`), with its error otherwise.

use super::parser::{Cmd, Expr, ExprKind};
use super::value::V;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;

/// The prefix of the hidden columns (not a valid unquoted ES|QL identifier).
pub const HIDDEN: &str = "$ft";

fn is_full_text(name: &str) -> bool {
    matches!(name, "match" | "match_operator" | "match_phrase" | "multi_match" | "qstr" | "kql")
}

fn display(name: &str) -> &'static str {
    match name {
        "match" => "MATCH",
        "match_operator" => ":",
        "match_phrase" => "MATCH_PHRASE",
        "multi_match" => "MULTI_MATCH",
        "qstr" => "QSTR",
        _ => "KQL",
    }
}

fn verification(src: &str, at: usize, msg: &str) -> EsError {
    let (l, c) = super::parser::location(src, at);
    EsError::new(400, "verification_exception", format!("Found 1 problem\nline {l}:{c}: {msg}"))
}

/// Rewrites the full-text calls of `cmds` (after the source command) into hidden columns and
/// returns the query of each, in column order.
pub fn extract(src: &str, cmds: &mut [Cmd]) -> EsResult<Vec<Value>> {
    let mut queries: Vec<(String, Value)> = Vec::new();
    for c in cmds.iter_mut() {
        match c {
            Cmd::Where(e) => rewrite(src, e, &mut queries)?,
            Cmd::Stats { aggs, by } => {
                for n in aggs.iter_mut().chain(by.iter_mut()) {
                    rewrite(src, &mut n.expr, &mut queries)?;
                }
            }
            Cmd::Eval(list) => {
                for n in list.iter() {
                    reject(src, &n.expr, "is only supported in WHERE and STATS commands or in EVAL within score(.) function")?;
                }
            }
            Cmd::Sort(keys) => {
                for k in keys.iter() {
                    reject(src, &k.expr, "is only supported in WHERE and STATS commands")?;
                }
            }
            _ => {}
        }
    }
    Ok(queries.into_iter().map(|(_, q)| q).collect())
}

fn reject(src: &str, e: &Expr, what: &str) -> EsResult<()> {
    let mut found = None;
    walk(e, &mut |x| {
        if found.is_none()
            && let ExprKind::Call(n, _) = &x.kind
            && is_full_text(n)
        {
            found = Some((display(n), x.start));
        }
    });
    match found {
        Some((":", at)) => Err(verification(src, at, &format!("[:] operator {what}"))),
        Some((name, at)) => Err(verification(src, at, &format!("[{name}] function {what}"))),
        None => Ok(()),
    }
}

fn walk(e: &Expr, f: &mut impl FnMut(&Expr)) {
    f(e);
    match &e.kind {
        ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::IsNull(x, _) | ExprKind::Like(x, ..) | ExprKind::Cast(x, _) => walk(x, f),
        ExprKind::Bin(_, a, b) => {
            walk(a, f);
            walk(b, f);
        }
        ExprKind::In(x, list, _) => {
            walk(x, f);
            list.iter().for_each(|y| walk(y, f));
        }
        ExprKind::Call(_, args) => args.iter().for_each(|y| walk(y, f)),
        _ => {}
    }
}

fn rewrite(src: &str, e: &mut Expr, queries: &mut Vec<(String, Value)>) -> EsResult<()> {
    let replace = match &mut e.kind {
        ExprKind::Call(n, args) if is_full_text(n) => {
            let text = src[e.start..e.end].to_string();
            let i = match queries.iter().position(|(t, _)| *t == text) {
                Some(i) => i,
                None => {
                    let q = query_of(src, n, args, e.start, &src[e.start..e.end])?;
                    queries.push((text, q));
                    queries.len() - 1
                }
            };
            Some(format!("{HIDDEN}{i}"))
        }
        ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::IsNull(x, _) | ExprKind::Like(x, ..) | ExprKind::Cast(x, _) => {
            rewrite(src, x, queries)?;
            None
        }
        ExprKind::Bin(_, a, b) => {
            rewrite(src, a, queries)?;
            rewrite(src, b, queries)?;
            None
        }
        ExprKind::In(x, list, _) => {
            rewrite(src, x, queries)?;
            for y in list.iter_mut() {
                rewrite(src, y, queries)?;
            }
            None
        }
        ExprKind::Call(_, args) => {
            for y in args.iter_mut() {
                rewrite(src, y, queries)?;
            }
            None
        }
        _ => None,
    };
    if let Some(col) = replace {
        e.kind = ExprKind::Col(col);
    }
    Ok(())
}

fn literal(src: &str, e: &Expr, func: &str, which: &str) -> EsResult<Value> {
    match &e.kind {
        ExprKind::Lit(v) if !v.is_null() => Ok(v.to_json(v.type_of())),
        _ => Err(verification(src, e.start, &format!("{which} argument of [{func}] must be a constant, received [{}]", &src[e.start..e.end]))),
    }
}

/// A query argument: a constant string, folded when it is a constant expression
/// (`MATCH(name, CONCAT("Bob ", "x"))`).
fn query_text(src: &str, e: &Expr, call: &str) -> EsResult<Value> {
    if let ExprKind::Lit(v) = &e.kind
        && !v.is_null()
    {
        return Ok(v.to_json(v.type_of()));
    }
    match super::exec::fold(src, e) {
        Some(v) if !v.is_null() => Ok(v.to_json(v.type_of())),
        _ => Err(verification(src, e.start, &format!("Query must be a constant string in [{call}], found [{}]", &src[e.start..e.end]))),
    }
}

fn field(src: &str, e: &Expr, func: &str) -> EsResult<String> {
    match &e.kind {
        ExprKind::Col(c) => Ok(c.clone()),
        _ => Err(verification(src, e.start, &format!("[{func}] cannot operate on [{}], which is not a field from an index mapping", &src[e.start..e.end]))),
    }
}

/// The options map (`{"operator": "AND"}`) as query parameters.
fn options(src: &str, e: Option<&Expr>, func: &str) -> EsResult<Map> {
    let mut out = Map::new();
    let Some(e) = e else { return Ok(out) };
    let pairs = match &e.kind {
        ExprKind::Call(n, pairs) if n == "$map" => pairs,
        _ => return Err(verification(src, e.start, &format!("third argument of [{func}] must be a map expression, received [{}]", &src[e.start..e.end]))),
    };
    for kv in pairs.chunks(2) {
        if let [k, v] = kv
            && let ExprKind::Lit(V::Str(key)) = &k.kind
        {
            out.insert(key.clone(), literal(src, v, func, "option")?);
        }
    }
    Ok(out)
}

fn query_of(src: &str, name: &str, args: &[Expr], at: usize, call: &str) -> EsResult<Value> {
    let func = display(name);
    let arity = |n: std::ops::RangeInclusive<usize>| -> EsResult<()> {
        if n.contains(&args.len()) {
            Ok(())
        } else {
            Err(verification(src, at, &format!("error building [{}]: expects {} arguments", name.trim_end_matches("_operator"), n.start())))
        }
    };
    Ok(match name {
        "match" | "match_operator" | "match_phrase" => {
            arity(2..=3)?;
            let f = field(src, &args[0], func)?;
            let mut body = options(src, args.get(2), func)?;
            body.shift_insert(0, "query".into(), query_text(src, &args[1], call)?);
            let kind = if name == "match_phrase" { "match_phrase" } else { "match" };
            obj! { kind => obj! { f => Value::Obj(body) } }
        }
        "multi_match" => {
            if args.len() < 2 {
                return Err(verification(src, at, "error building [multi_match]: expects at least two arguments"));
            }
            let q = query_text(src, &args[0], call)?;
            let mut fields = Vec::new();
            let mut opts = None;
            for a in &args[1..] {
                match &a.kind {
                    ExprKind::Call(n, _) if n == "$map" => opts = Some(a),
                    _ => fields.push(Value::from(field(src, a, func)?)),
                }
            }
            let mut body = options(src, opts, func)?;
            body.shift_insert(0, "query".into(), q);
            body.shift_insert(1, "fields".into(), Value::Arr(fields));
            obj! { "multi_match" => Value::Obj(body) }
        }
        "qstr" => {
            arity(1..=2)?;
            let mut body = options(src, args.get(1), func)?;
            body.shift_insert(0, "query".into(), query_text(src, &args[0], call)?);
            obj! { "query_string" => Value::Obj(body) }
        }
        _ => {
            arity(1..=1)?;
            // KQL `field:value` terms read as a query string (KQL's common subset).
            obj! { "query_string" => obj! { "query" => query_text(src, &args[0], call)? } }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_calls_to_hidden_columns() {
        let src = "FROM i | WHERE MATCH(t, \"fox\") OR n > 1 | WHERE t : \"dog\" AND MATCH(t, \"fox\")";
        let q = super::super::parser::parse(src, &[], &[]).unwrap();
        let mut cmds = q.cmds;
        let qs = extract(src, &mut cmds[1..]).unwrap();
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].to_json(), r#"{"match":{"t":{"query":"fox"}}}"#);
        assert_eq!(qs[1].to_json(), r#"{"match":{"t":{"query":"dog"}}}"#);
    }

    #[test]
    fn options_and_errors() {
        let src = "FROM i | WHERE MATCH(t, \"a b\", {\"operator\": \"AND\"})";
        let q = super::super::parser::parse(src, &[], &[]).unwrap();
        let mut cmds = q.cmds;
        let qs = extract(src, &mut cmds[1..]).unwrap();
        assert_eq!(qs[0].to_json(), r#"{"match":{"t":{"query":"a b","operator":"AND"}}}"#);
        let src = "FROM i | EVAL m = MATCH(t, \"fox\")";
        let q = super::super::parser::parse(src, &[], &[]).unwrap();
        let mut cmds = q.cmds;
        assert_eq!(
            extract(src, &mut cmds[1..]).unwrap_err().reason(),
            "Found 1 problem\nline 1:19: [MATCH] function is only supported in WHERE and STATS commands or in EVAL within score(.) function"
        );
    }
}
