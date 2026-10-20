//! Constraint exclusion: proving that a scan's restriction contradicts its table's CHECK
//! constraints, so the scan returns nothing (Postgres's predicate_refuted_by, for the
//! btree-comparison cases it proves pairwise).

use crate::sql::expr::Expr;
use crate::types::{DataType, Value};
use std::cmp::Ordering;

/// Whether rows passing every clause of `restriction` must fail (be false for) one of
/// `constraints`. Constraints hold for NULL, so only a definite contradiction counts.
pub fn refuted(restriction: &[Expr], constraints: &[Expr]) -> bool {
    constraints
        .iter()
        .any(|c| restriction.iter().any(|r| refutes(r, c)))
}

/// Self-contradictory restriction clauses (relation_excluded_by_constraints'
/// `predicate_refuted_by(safe_restrictions, safe_restrictions, true)`): weakly, an
/// `x IS NULL` refutes a strict clause over `x` (and `x IS NOT NULL`), which can't then be
/// true; besides the pairwise comparisons `refuted` proves.
pub fn self_refuted(quals: &[Expr]) -> bool {
    let quals: Vec<&Expr> = quals.iter().filter(|q| !q.is_volatile()).collect();
    for q in &quals {
        let Expr::IsNull(x, false) = q else { continue };
        for other in &quals {
            if std::ptr::eq(*q, *other) {
                continue;
            }
            let strict_over = match other {
                Expr::IsNull(y, true) => same_operand(x, y),
                Expr::Call(d, args, _) => {
                    d.strict && d.srf.is_none() && args.iter().any(|a| same_operand(x, a))
                }
                _ => false,
            };
            if strict_over {
                return true;
            }
        }
    }
    false
}

/// A restriction clause that is never true (constant FALSE or NULL).
pub fn always_false(e: &Expr) -> bool {
    matches!(e, Expr::Const(Value::Bool(false) | Value::Null, _))
}

/// Whether clause `r` being true makes constraint `c` false.
fn refutes(r: &Expr, c: &Expr) -> bool {
    match c {
        // Refuting any conjunct refutes the conjunction.
        Expr::And(items) => return items.iter().any(|i| refutes(r, i)),
        // Every arm must be refuted.
        Expr::Or(items) => return items.iter().all(|i| refutes(r, i)),
        _ => {}
    }
    match r {
        Expr::And(items) => return items.iter().any(|i| refutes(i, c)),
        // Each arm of an OR clause must refute the constraint.
        Expr::Or(items) => return items.iter().all(|i| refutes(i, c)),
        _ => {}
    }
    if let Some(arms) = or_arms(c) {
        return arms.iter().all(|a| refutes(r, a));
    }
    if let Some(arms) = or_arms(r) {
        return arms.iter().all(|a| refutes(a, c));
    }
    // `x IS NULL` against `x IS NOT NULL`.
    if let (Expr::IsNull(rx, false), Expr::IsNull(cx, true)) = (r, c) {
        return same_operand(rx, cx);
    }
    // A strict comparison against `x IS NULL`: the clause needs a value.
    if let (Some(rc), Expr::IsNull(cx, false)) = (comparison(r), c) {
        return same_operand(rc.0, cx);
    }
    match (comparison(r), comparison(c)) {
        (Some((rx, rop, rv)), Some((cx, cop, cv))) if same_operand(rx, cx) => {
            disjoint(rop, rv, cop, cv)
        }
        _ => false,
    }
}

/// `x = ANY (constant array)` / `x IN (constants)` as OR arms `x = v`.
pub(crate) fn or_arms(e: &Expr) -> Option<Vec<Expr>> {
    match e {
        Expr::ArrayOp {
            left,
            right,
            op,
            all: false,
        } if op.name == "=" => {
            let Expr::Const(Value::Array(a), _) = &**right else {
                return None;
            };
            if a.items.len() > 100 {
                return None;
            }
            Some(
                a.items
                    .iter()
                    .map(|v| {
                        Expr::Call(
                            op.clone(),
                            vec![(**left).clone(), Expr::Const(v.clone(), a.elem.clone())],
                            DataType::Bool,
                        )
                    })
                    .collect(),
            )
        }
        Expr::InList(x, items, false) => {
            if items
                .iter()
                .any(|(v, f)| !matches!(v, Expr::Const(..)) || f.name != "=")
            {
                return None;
            }
            Some(
                items
                    .iter()
                    .map(|(v, f)| {
                        Expr::Call(f.clone(), vec![(**x).clone(), v.clone()], DataType::Bool)
                    })
                    .collect(),
            )
        }
        _ => None,
    }
}

/// `operand op constant`, flipped so the constant is on the right.
pub(crate) fn comparison(e: &Expr) -> Option<(&Expr, &'static str, &Value)> {
    let Expr::Call(def, args, _) = e else {
        return None;
    };
    if args.len() != 2 {
        return None;
    }
    let op: &'static str = match def.name {
        "=" => "=",
        "<>" => "<>",
        "<" => "<",
        "<=" => "<=",
        ">" => ">",
        ">=" => ">=",
        _ => return None,
    };
    match (&args[0], &args[1]) {
        (x, Expr::Const(v, _)) if !matches!(x, Expr::Const(..)) => Some((x, op, v)),
        (Expr::Const(v, _), x) => Some((
            x,
            match op {
                "<" => ">",
                "<=" => ">=",
                ">" => "<",
                ">=" => "<=",
                o => o,
            },
            v,
        )),
        _ => None,
    }
}

/// The same column, looking through widening numeric casts (`int4 col = int8 const`).
pub(crate) fn same_operand(a: &Expr, b: &Expr) -> bool {
    let strip = |e: &Expr| -> Option<usize> {
        match e {
            Expr::Col(i, _) => Some(*i),
            Expr::Cast(inner, from, to, _) if numeric(from) && numeric(to) => match &**inner {
                Expr::Col(i, _) => Some(*i),
                _ => None,
            },
            _ => None,
        }
    };
    matches!((strip(a), strip(b)), (Some(x), Some(y)) if x == y)
}

fn numeric(t: &DataType) -> bool {
    matches!(
        t,
        DataType::Int2 | DataType::Int4 | DataType::Int8 | DataType::Numeric(_)
    )
}

/// Comparison of constants whose ordering is the btree one (not collation-dependent text).
fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    use Value as V;
    let int = |v: &Value| match v {
        V::Int2(x) => Some(*x as i64),
        V::Int4(x) => Some(*x as i64),
        V::Int8(x) => Some(*x),
        _ => None,
    };
    if let (Some(x), Some(y)) = (int(a), int(b)) {
        return Some(x.cmp(&y));
    }
    match (a, b) {
        (V::Numeric(_), V::Numeric(_))
        | (V::Float4(_), V::Float4(_))
        | (V::Float8(_), V::Float8(_))
        | (V::Date(_), V::Date(_))
        | (V::Timestamp(_), V::Timestamp(_))
        | (V::Timestamptz(_), V::Timestamptz(_))
        | (V::Time(_), V::Time(_))
        | (V::Oid(_), V::Oid(_))
        | (V::Bool(_), V::Bool(_)) => Some(a.cmp_total(b)),
        _ => None,
    }
}

/// Whether no value satisfies both `x rop rv` and `x cop cv`.
fn disjoint(rop: &str, rv: &Value, cop: &str, cv: &Value) -> bool {
    if rv.is_null() || cv.is_null() {
        return false;
    }
    // Equality proofs work for any type whose `=` is plain equality.
    let eq = || -> Option<bool> {
        match compare(rv, cv) {
            Some(o) => Some(o == Ordering::Equal),
            None if std::mem::discriminant(rv) == std::mem::discriminant(cv)
                && matches!(rv, Value::Text(_) | Value::Uuid(_) | Value::Bytea(_)) =>
            {
                Some(rv.cmp_total(cv) == Ordering::Equal)
            }
            None => None,
        }
    };
    match (rop, cop) {
        ("=", "=") => eq() == Some(false),
        ("=", "<>") | ("<>", "=") => eq() == Some(true),
        ("<>", _) | (_, "<>") => false,
        _ => {
            let Some(ord) = compare(rv, cv) else {
                return false;
            };
            // Each side as an interval; empty intersection?
            let below = |op: &str| matches!(op, "<" | "<=");
            match (rop, cop) {
                ("=", c) => !holds(ord, c),
                (r, "=") => !holds(ord.reverse(), r),
                (r, c) if below(r) == below(c) => false,
                // x < rv (or <=) and x > cv (or >=): empty when rv <= cv (or rv < cv with both inclusive).
                (r, c) if below(r) => match ord {
                    Ordering::Less => true,
                    Ordering::Equal => !(r == "<=" && c == ">="),
                    Ordering::Greater => false,
                },
                (r, c) => match ord {
                    Ordering::Greater => true,
                    Ordering::Equal => !(r == ">=" && c == "<="),
                    Ordering::Less => false,
                },
            }
        }
    }
}

/// Whether `v op w` holds when `v` compares to `w` as `ord`.
fn holds(ord: Ordering, op: &str) -> bool {
    match op {
        "<" => ord == Ordering::Less,
        "<=" => ord != Ordering::Greater,
        ">" => ord == Ordering::Greater,
        ">=" => ord != Ordering::Less,
        _ => ord == Ordering::Equal,
    }
}
