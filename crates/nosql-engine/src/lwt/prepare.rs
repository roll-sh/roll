//! Preparing `IF` clauses: `ColumnCondition.Raw.prepare`, `ColumnConditions` and the
//! condition checks of `ModificationStatement` / `DeleteStatement` (Apache-2.0), with ScyllaDB's
//! differences (subscripting sets, `LIKE`, `NOT IN`, its bind variable names and messages).
//!
//! The order of preparation is the reference's: Cassandra prepares the `IF` clause before the
//! assignments and the `WHERE` clause, ScyllaDB after the assignments but before `WHERE`, so the
//! first error a statement reports matches.

use nosql_cql::ast;
use nosql_types::{CqlType, Dialect};

use crate::error::{Error, Result};
use crate::messages as msg;
use crate::prepare::restrictions::{InValues, Pred, Restrictions};
use crate::prepare::terms::{self, Term};
use crate::prepare::{PrepareCtx, Receiver};
use crate::schema::Table;
use crate::statements::modify::{
    CondTarget, CondValue, Condition, Conditions, ModKind, ModifyPlan,
};

/// `ColumnCondition.Raw.prepare` for every condition of an `IF` clause.
pub fn conditions(t: &Table, conds: &ast::Conditions, cx: &mut PrepareCtx) -> Result<Conditions> {
    let list = match conds {
        ast::Conditions::None => return Ok(Conditions::None),
        ast::Conditions::Exists => return Ok(Conditions::Exists),
        ast::Conditions::List(l) => l,
    };
    let mut out = Vec::with_capacity(list.len());
    for c in list {
        out.push(condition(t, c, cx)?);
    }
    Ok(Conditions::List(out))
}

fn condition(t: &Table, c: &ast::Condition, cx: &mut PrepareCtx) -> Result<Condition> {
    let d = cx.dialect;
    let name = c.column.name();
    let ci = t
        .column_index(&name)
        .ok_or_else(|| msg::lwt_unknown_column(d, &t.keyspace, &t.name, &name))?;
    let col = &t.columns[ci];
    if col.ty.is_counter() && !d.is_scylla() {
        return Err(Error::invalid("Conditions on counters are not supported"));
    }
    let column = Receiver::column(t, &col.name, &col.ty);
    let (target, value_recv) = match &c.target {
        ast::ConditionTarget::Column => (CondTarget::Column, column.clone()),
        ast::ConditionTarget::Element(k) => {
            let (key, value) = match (&col.ty, d) {
                (CqlType::List { elem, .. }, _) => (
                    column.derive(msg::lwt_list_index_name(d, &col.name), CqlType::Int),
                    column.derive(format!("value({})", col.name), (**elem).clone()),
                ),
                (CqlType::Map { key, value, .. }, _) => (
                    column.derive(format!("key({})", col.name), (**key).clone()),
                    column.derive(format!("value({})", col.name), (**value).clone()),
                ),
                // ScyllaDB subscripts a set by element: the element if present, else null.
                (CqlType::Set { elem, .. }, Dialect::Scylla) => (
                    column.derive(format!("value({})", col.name), (**elem).clone()),
                    column.derive(format!("value({})", col.name), (**elem).clone()),
                ),
                (CqlType::Set { .. }, Dialect::Cassandra) => {
                    return Err(Error::invalid(format!(
                        "Invalid element access syntax for set column {}",
                        col.name
                    )));
                }
                _ => return Err(msg::lwt_not_subscriptable(d, &col.name)),
            };
            if !d.is_scylla() {
                check_durations(c.op, &value.ty)?;
            }
            (CondTarget::Element(terms::prepare(k, &key, cx)?), value)
        }
        ast::ConditionTarget::Field(f) => {
            let CqlType::Udt(def, _) = &col.ty else {
                return Err(msg::lwt_not_a_udt(d, &col.name, &col.ty));
            };
            let fname = f.name();
            let Some(i) = def.field_index(&fname) else {
                return Err(msg::lwt_unknown_field(d, &col.name, &def.name, &fname));
            };
            // ScyllaDB names the value after the field alone.
            let name = match d {
                Dialect::Scylla => fname.to_string(),
                Dialect::Cassandra => format!("{}.{fname}", col.name),
            };
            let recv = column.derive(name, def.fields[i].1.clone());
            if !d.is_scylla() {
                check_durations(c.op, &recv.ty)?;
            }
            (CondTarget::Field(i), recv)
        }
    };
    if matches!(c.target, ast::ConditionTarget::Column) && !d.is_scylla() {
        check_durations(c.op, &col.ty)?;
    }
    if d.is_scylla() {
        scylla_checks(c.op, &value_recv, &col.name)?;
    }
    let value = prepare_value(c, &value_recv, cx)?;
    if col.is_primary_key() {
        return Err(pk_condition(&col.name));
    }
    Ok(Condition {
        column: ci,
        target,
        op: c.op,
        value,
    })
}

fn pk_condition(name: &str) -> Error {
    Error::invalid(format!(
        "PRIMARY KEY column '{name}' cannot have IF conditions"
    ))
}

/// `ColumnCondition.Raw.prepareTerms`: the receivers of the condition value(s).
fn prepare_value(c: &ast::Condition, recv: &Receiver, cx: &mut PrepareCtx) -> Result<CondValue> {
    let d = cx.dialect;
    let op = c.op;
    if !d.is_scylla() {
        if op == ast::Operator::ContainsKey && !matches!(recv.ty, CqlType::Map { .. }) {
            return Err(Error::invalid(format!(
                "Cannot use CONTAINS KEY on non-map column {}",
                recv.name
            )));
        }
        if op == ast::Operator::Contains && !recv.ty.is_collection() {
            return Err(Error::invalid(format!(
                "Cannot use CONTAINS on non-collection column {}",
                recv.name
            )));
        }
    }
    let negated = op == ast::Operator::NotIn;
    match &c.value {
        ast::Value::List(items) => {
            let item = match d {
                Dialect::Scylla => recv.derive(
                    msg::lwt_in_name(d, &recv.name, negated, true),
                    recv.ty.clone(),
                ),
                Dialect::Cassandra => recv.clone(),
            };
            let mut v = Vec::with_capacity(items.len());
            for it in items {
                v.push(terms::prepare(it, &item, cx)?);
            }
            Ok(CondValue::In(InValues::Terms(v)))
        }
        ast::Value::ListMarker(m) => {
            let rr = recv.derive(
                msg::lwt_in_name(d, &recv.name, negated, false),
                CqlType::list(recv.ty.clone(), false),
            );
            let index = cx.bind(m, &rr);
            Ok(CondValue::In(InValues::Marker(Term::Bind {
                index,
                ty: rr.ty.clone(),
                receiver: rr.name.clone(),
            })))
        }
        ast::Value::Term(v) if matches!(op, ast::Operator::In | ast::Operator::NotIn) => Ok(
            CondValue::In(InValues::Terms(vec![terms::prepare(v, recv, cx)?])),
        ),
        ast::Value::Term(v) => {
            let r = match (op, &recv.ty) {
                (
                    ast::Operator::Contains,
                    CqlType::List { elem, .. } | CqlType::Set { elem, .. },
                ) => recv.derive(format!("value({})", recv.name), (**elem).clone()),
                (ast::Operator::Contains, CqlType::Map { value, .. }) => {
                    recv.derive(format!("value({})", recv.name), (**value).clone())
                }
                (ast::Operator::ContainsKey, CqlType::Map { key, .. }) => {
                    recv.derive(format!("key({})", recv.name), (**key).clone())
                }
                _ => recv.clone(),
            };
            Ok(CondValue::One(terms::prepare(v, &r, cx)?))
        }
    }
}

/// `ColumnCondition.Raw.validateOperationOnDurations` (Cassandra).
fn check_durations(op: ast::Operator, ty: &CqlType) -> Result<()> {
    if !is_slice(op) || !ty.references_duration() {
        return Ok(());
    }
    Err(Error::invalid(if ty.is_collection() {
        "Slice conditions are not supported on collections containing durations".to_string()
    } else if ty.is_tuple() {
        "Slice conditions are not supported on tuples containing durations".to_string()
    } else if ty.is_udt() {
        "Slice conditions are not supported on UDTs containing durations".to_string()
    } else {
        format!(
            "Slice conditions ( {} ) are not supported on durations",
            op.as_str()
        )
    }))
}

/// ScyllaDB's checks before typing the value: `LIKE` needs a string column, nothing holding a
/// duration has an order.
fn scylla_checks(op: ast::Operator, recv: &Receiver, column: &str) -> Result<()> {
    if op == ast::Operator::Like && !matches!(recv.ty, CqlType::Text | CqlType::Ascii) {
        return Err(Error::invalid(format!(
            "LIKE is allowed only on string types, which {column} is not"
        )));
    }
    if is_slice(op) && recv.ty.references_duration() {
        return Err(Error::invalid(format!(
            "Duration type is unordered for {}",
            recv.name
        )));
    }
    Ok(())
}

pub fn is_slice(op: ast::Operator) -> bool {
    matches!(
        op,
        ast::Operator::Lt | ast::Operator::Lte | ast::Operator::Gt | ast::Operator::Gte
    )
}

/// `ModificationStatement.appliesOnlyToStaticColumns`: operations and conditions only touch
/// static columns (a DELETE of whole rows touches regular columns).
pub fn applies_only_to_static(
    t: &Table,
    kind: ModKind,
    op_columns: &[usize],
    conditions: &Conditions,
) -> bool {
    let op_regular = op_columns.iter().any(|&c| !t.columns[c].is_static())
        || (kind == ModKind::Delete && op_columns.is_empty());
    let op_static = op_columns.iter().any(|&c| t.columns[c].is_static());
    let (cond_regular, cond_static) = match conditions {
        Conditions::List(cs) => (
            cs.iter().any(|c| !t.columns[c.column].is_static()),
            cs.iter().any(|c| t.columns[c.column].is_static()),
        ),
        _ => (false, false),
    };
    !op_regular && !cond_regular && (op_static || cond_static)
}

/// The checks a conditional statement gets once its `WHERE` clause is analysed
/// (`DeleteStatement.Parsed.prepareInternal`, ScyllaDB's IN checks at PREPARE).
pub fn check_restrictions(d: Dialect, plan: &ModifyPlan, r: &Restrictions) -> Result<()> {
    if !plan.conditional {
        return Ok(());
    }
    if plan.kind == ModKind::Delete && !all_pk_by_equality(plan.table.as_ref(), r) {
        let op_regular = plan.deletions.is_empty()
            || plan
                .deletions
                .iter()
                .any(|x| !plan.table.columns[x.column()].is_static());
        if op_regular {
            return Err(Error::invalid(
                "DELETE statements must restrict all PRIMARY KEY columns with equality relations in order to delete non static columns",
            ));
        }
        if let Conditions::List(cs) = &plan.conditions
            && cs.iter().any(|c| !plan.table.columns[c.column].is_static())
        {
            return Err(Error::invalid(
                "DELETE statements must restrict all PRIMARY KEY columns with equality relations in order to use IF condition on non static columns",
            ));
        }
    }
    if d.is_scylla() {
        check_in(plan, r)?;
    }
    Ok(())
}

/// `StatementRestrictions.hasAllPKColumnsRestrictedByEqualities` (`=` or `IN`).
fn all_pk_by_equality(t: &Table, r: &Restrictions) -> bool {
    if r.token.is_some() || r.ck_slice || r.ck_prefix < t.clustering.len() {
        return false;
    }
    r.pk.iter()
        .all(|ps| !ps.is_empty() && ps.iter().all(|p| matches!(p, Pred::Eq(_) | Pred::In(_))))
}

/// IN on the key of a conditional statement (`makeCasRequest`).
pub fn check_in(plan: &ModifyPlan, r: &Restrictions) -> Result<()> {
    let what = if plan.kind == ModKind::Update {
        "updates"
    } else {
        "deletions"
    };
    if pk_has_in(r) {
        return Err(Error::invalid(format!(
            "IN on the partition key is not supported with conditional {what}"
        )));
    }
    if ck_has_in(r) {
        return Err(Error::invalid(format!(
            "IN on the clustering key columns is not supported with conditional {what}"
        )));
    }
    Ok(())
}

/// `IN` with a marker or several values (`IN (x)` is an equality).
fn is_in(p: &Pred) -> bool {
    match p {
        Pred::In(InValues::Terms(ts)) => ts.len() != 1,
        Pred::In(InValues::Marker(_)) => true,
        _ => false,
    }
}

pub fn pk_has_in(r: &Restrictions) -> bool {
    r.pk.iter().any(|ps| ps.iter().any(is_in))
}

pub fn ck_has_in(r: &Restrictions) -> bool {
    r.ck.iter().any(|ps| ps.iter().any(is_in))
        || r.multi
            .iter()
            .any(|m| matches!(m.kind, crate::prepare::restrictions::MultiKind::In(_)))
}
