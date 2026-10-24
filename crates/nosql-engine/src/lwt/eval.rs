//! Evaluating `IF` conditions against the current state of a row.
//!
//! Cassandra: `ColumnCondition.Bound` and its subclasses (`SimpleBound`, `ElementAccessBound`,
//! `MultiCellCollectionBound`, `UDTFieldAccessBound`, `MultiCellUdtBound`; Apache-2.0), including
//! their quirks: a map element is compared with the map's *key* type, an empty non-frozen
//! collection literal only supports `=` / `!=`, unset `IN` items are dropped. Conditions of a row
//! are kept in a Guava `HashMultimap` keyed by (column name, element), so they run in Java hash
//! order and stop at the first false one ([`java_hash_order`]).
//!
//! ScyllaDB: conditions run left to right and stop at the first false one. A null subscript
//! (map key, list index, set element) makes the condition false, an empty non-frozen collection
//! literal is null, `IN` against a null list is false and unset values are errors.

use std::cmp::Ordering;

use bytes::Bytes;
use nosql_cql::ast::Operator;
use nosql_storage::Row;
use nosql_types::{CqlType, Dialect};

use crate::error::{Error, Result};
use crate::messages as msg;
use crate::prepare::restrictions::InValues;
use crate::prepare::terms::{Bound, EvalCtx, Term};
use crate::schema::Table;
use crate::statements::modify::{CondTarget, CondValue, Condition};
use crate::storage;

/// The row a condition is checked against: the regular row (if any) and the static row (if any).
#[derive(Clone, Copy)]
pub struct View<'a> {
    pub table: &'a Table,
    pub row: Option<&'a Row>,
    pub static_row: Option<&'a Row>,
    pub now: i64,
}

impl View<'_> {
    /// The live value of column `ci`.
    pub fn value(&self, ci: usize) -> Option<Bytes> {
        let col = &self.table.columns[ci];
        let row = if col.is_static() {
            self.static_row
        } else {
            self.row
        }?;
        storage::column_value(self.table, col, row, self.now, None).filter(|v| {
            // A non-frozen collection with no live cell is null.
            !(col.ty.is_multicell() && col.ty.is_collection() && v.is_empty())
        })
    }
}

/// A condition with its values bound for one execution.
pub struct BoundCondition<'a> {
    pub cond: &'a Condition,
    /// The element (`m[k]`, `l[i]`): `None` when the target is not an element.
    pub element: Option<Bound>,
    pub values: Values,
}

/// The bound right-hand side.
pub enum Values {
    One(Bound),
    /// `IN`: `None` is a null (or, for ScyllaDB, unset) list.
    List(Option<Vec<Option<Bytes>>>),
}

fn op_text(op: Operator) -> &'static str {
    match op {
        Operator::In => "IN",
        o => o.as_str(),
    }
}

/// Binds a condition's terms (`ColumnCondition.bind`).
pub fn bind<'a>(cond: &'a Condition, cx: &EvalCtx) -> Result<BoundCondition<'a>> {
    let d = cx.dialect;
    let element = match &cond.target {
        CondTarget::Element(k) => Some(k.eval(cx)?),
        _ => None,
    };
    let values = match &cond.value {
        CondValue::One(t) => Values::One(t.eval(cx)?),
        CondValue::In(InValues::Marker(m)) => match in_list(m, cx)? {
            Bound::Val(b) => Values::List(Some(
                nosql_types::collection_elements(&b)
                    .map_err(|e| Error::invalid(e.message))?
                    .into_iter()
                    .map(|e| e.map(Bytes::copy_from_slice))
                    .collect(),
            )),
            Bound::Null if d.is_scylla() => Values::List(None),
            Bound::Null => return Err(Error::invalid("Invalid null list in IN condition")),
            Bound::Unset => return Err(msg::lwt_unset_value(d, bind_index(m))),
        },
        CondValue::In(InValues::Terms(ts)) => {
            let mut out = Vec::with_capacity(ts.len());
            for t in ts {
                match t.eval(cx)? {
                    Bound::Val(b) => out.push(Some(b)),
                    Bound::Null => out.push(None),
                    // Cassandra drops unset `IN` items (`filterUnsetValuesIfNeeded`).
                    Bound::Unset if !d.is_scylla() => {}
                    Bound::Unset => return Err(msg::lwt_unset_value(d, bind_index(t))),
                }
            }
            Values::List(Some(out))
        }
    };
    if d.is_scylla()
        && let Values::One(Bound::Unset) = values
        && let CondValue::One(t) = &cond.value
    {
        return Err(msg::lwt_unset_value(d, bind_index(t)));
    }
    Ok(BoundCondition {
        cond,
        element,
        values,
    })
}

fn bind_index(t: &Term) -> usize {
    match t {
        Term::Bind { index, .. } => *index,
        _ => 0,
    }
}

/// Evaluates conditions in the dialect's order; `Ok(false)` at the first that does not hold.
pub fn applies(d: Dialect, conds: &[&BoundCondition], view: &View) -> Result<bool> {
    let order: Vec<usize> = match d {
        Dialect::Scylla => (0..conds.len()).collect(),
        Dialect::Cassandra => java_hash_order(view.table, conds),
    };
    for i in order {
        let ok = match d {
            Dialect::Scylla => scylla(conds[i], view)?,
            Dialect::Cassandra => cassandra(conds[i], view)?,
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Java's `ByteBuffer.hashCode`.
fn buffer_hash(b: &[u8]) -> i32 {
    let mut h: i32 = 1;
    for x in b.iter().rev() {
        h = h.wrapping_mul(31).wrapping_add(*x as i8 as i32);
    }
    h
}

/// The iteration order of a Guava `HashMultimap<Pair<ColumnIdentifier, ByteBuffer>, Bound>`
/// (`HashMultimap.create()`: a `HashMap` of 16 buckets, doubling past 3/4 load; verified on the
/// reference), values of one key in insertion order (Cassandra's is identity-hash order).
pub fn java_hash_order(t: &Table, conds: &[&BoundCondition]) -> Vec<usize> {
    let mut keys: Vec<(i32, Vec<u8>, Option<Vec<u8>>)> = Vec::new();
    let mut members: Vec<Vec<usize>> = Vec::new();
    for (i, c) in conds.iter().enumerate() {
        let name = t.columns[c.cond.column].name.as_bytes().to_vec();
        let elem = match &c.element {
            Some(Bound::Val(b)) => Some(b.to_vec()),
            Some(Bound::Unset) => Some(Vec::new()),
            _ => None,
        };
        let hash = 31i32
            .wrapping_mul(31i32.wrapping_add(buffer_hash(&name)))
            .wrapping_add(elem.as_deref().map(buffer_hash).unwrap_or(0));
        match keys.iter().position(|(_, n, e)| *n == name && *e == elem) {
            Some(k) => members[k].push(i),
            None => {
                keys.push((hash, name, elem));
                members.push(vec![i]);
            }
        }
    }
    let mut buckets = 16usize;
    while keys.len() > buckets * 3 / 4 {
        buckets *= 2;
    }
    let mut order: Vec<usize> = (0..keys.len()).collect();
    // Stable: equal buckets keep insertion order.
    order.sort_by_key(|&k| {
        let h = keys[k].0 as u32;
        ((h ^ (h >> 16)) as usize) & (buckets - 1)
    });
    order.into_iter().flat_map(|k| members[k].clone()).collect()
}

fn invalid_null(op: Operator) -> Error {
    Error::invalid(format!(
        "Invalid comparison with null for operator \"{}\"",
        op_text(op)
    ))
}

/// A bound `IN ?` list: its elements may be null (they match a null column), so they are
/// validated one by one rather than as a `list<T>`.
fn in_list(m: &Term, cx: &EvalCtx) -> Result<Bound> {
    let Term::Bind {
        index,
        ty: CqlType::List { elem, .. },
        receiver,
    } = m
    else {
        return m.eval(cx);
    };
    let Some(crate::response::Param::Set(b)) = cx.params.get(*index) else {
        return m.eval(cx);
    };
    let elems = nosql_types::collection_elements(b).map_err(|e| Error::invalid(e.message))?;
    for e in elems.into_iter().flatten() {
        crate::prepare::terms::validate_bound(elem, e, receiver, cx.dialect)?;
    }
    Ok(Bound::Val(b.clone()))
}

/// `Operator.isSatisfiedBy` for a comparison (`IN` compares with `=`).
fn compare_op(op: Operator, ord: Ordering) -> bool {
    match op {
        Operator::Eq | Operator::In => ord == Ordering::Equal,
        Operator::Neq => ord != Ordering::Equal,
        Operator::Lt => ord == Ordering::Less,
        Operator::Lte => ord != Ordering::Greater,
        Operator::Gt => ord == Ordering::Greater,
        Operator::Gte => ord != Ordering::Less,
        _ => false,
    }
}

/// The frozen form used to compare whole collection / UDT values.
fn frozen(ty: &CqlType) -> CqlType {
    match ty {
        CqlType::Udt(def, _) => CqlType::Udt(def.clone(), true),
        t if t.is_collection() => t.freeze(),
        t => t.clone(),
    }
}

/// A set's or map's condition value in the type's order (bound values may be unsorted).
fn normalise(ty: &CqlType, v: &[u8]) -> Option<Bytes> {
    match ty {
        CqlType::Set { elem, .. } => {
            let mut es: Vec<&[u8]> = nosql_types::collection_elements(v)
                .ok()?
                .into_iter()
                .flatten()
                .collect();
            es.sort_by(|a, b| nosql_types::compare(elem, a, b));
            es.dedup_by(|a, b| nosql_types::compare(elem, a, b) == Ordering::Equal);
            Some(nosql_types::pack_collection(es))
        }
        CqlType::Map { key, .. } => {
            let mut es: Vec<(&[u8], &[u8])> = nosql_types::map_entries(v).ok()?;
            es.sort_by(|a, b| nosql_types::compare(key, a.0, b.0));
            es.dedup_by(|a, b| nosql_types::compare(key, a.0, b.0) == Ordering::Equal);
            Some(nosql_types::pack_map(es))
        }
        _ => Some(Bytes::copy_from_slice(v)),
    }
}

/// Elements (or map keys / values) of a serialised collection.
fn parts(ty: &CqlType, v: &[u8], keys: bool) -> Vec<Bytes> {
    match ty {
        CqlType::Map { .. } => nosql_types::map_entries(v)
            .map(|es| {
                es.into_iter()
                    .map(|(k, x)| Bytes::copy_from_slice(if keys { k } else { x }))
                    .collect()
            })
            .unwrap_or_default(),
        _ => nosql_types::collection_elements(v)
            .map(|es| {
                es.into_iter()
                    .flatten()
                    .map(Bytes::copy_from_slice)
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// `CONTAINS` / `CONTAINS KEY` on a collection value.
fn contains(ty: &CqlType, collection: &[u8], value: &[u8], key: bool) -> bool {
    let et = match (ty, key) {
        (CqlType::List { elem, .. } | CqlType::Set { elem, .. }, _) => elem.as_ref(),
        (CqlType::Map { key: k, .. }, true) => k.as_ref(),
        (CqlType::Map { value: v, .. }, false) => v.as_ref(),
        _ => return false,
    };
    parts(ty, collection, key)
        .iter()
        .any(|e| nosql_types::compare(et, e, value) == Ordering::Equal)
}

/// The value at a list index / map key / set element of a collection value.
fn element_of(ty: &CqlType, collection: &[u8], key: &[u8]) -> Option<Bytes> {
    match ty {
        CqlType::List { .. } => {
            let i = i32::from_be_bytes(key.try_into().ok()?);
            if i < 0 {
                return None;
            }
            nosql_types::collection_elements(collection)
                .ok()?
                .get(i as usize)
                .copied()
                .flatten()
                .map(Bytes::copy_from_slice)
        }
        CqlType::Map { key: kt, .. } => nosql_types::map_entries(collection)
            .ok()?
            .into_iter()
            .find(|(k, _)| nosql_types::compare(kt, k, key) == Ordering::Equal)
            .map(|(_, v)| Bytes::copy_from_slice(v)),
        CqlType::Set { elem, .. } => nosql_types::collection_elements(collection)
            .ok()?
            .into_iter()
            .flatten()
            .find(|e| nosql_types::compare(elem, e, key) == Ordering::Equal)
            .map(Bytes::copy_from_slice),
        _ => None,
    }
}

fn field_of(collection: &[u8], i: usize) -> Option<Bytes> {
    nosql_types::tuple_fields(collection)
        .ok()?
        .get(i)
        .copied()
        .flatten()
        .map(Bytes::copy_from_slice)
}

fn kind_name(ty: &CqlType) -> &'static str {
    match ty {
        CqlType::List { .. } => "list",
        CqlType::Set { .. } => "set",
        _ => "map",
    }
}

fn is_empty_collection(v: &[u8]) -> bool {
    v.len() == 4 && v == [0, 0, 0, 0]
}

/// `ColumnCondition.Bound.compareWithOperator`.
fn cassandra_compare(
    op: Operator,
    ty: &CqlType,
    value: &Bound,
    other: Option<&[u8]>,
) -> Result<bool> {
    let value = match value {
        Bound::Unset => return Err(Error::invalid("Invalid 'unset' value in condition")),
        Bound::Null => {
            return match op {
                Operator::Eq | Operator::In => Ok(other.is_none()),
                Operator::Neq => Ok(other.is_some()),
                _ => Err(invalid_null(op)),
            };
        }
        Bound::Val(v) => v,
    };
    let Some(other) = other else {
        return Ok(op == Operator::Neq);
    };
    Ok(match op {
        Operator::Contains => contains(ty, other, value, false),
        Operator::ContainsKey => contains(ty, other, value, true),
        _ => compare_op(op, nosql_types::compare(&frozen(ty), other, value)),
    })
}

fn bound_values(values: &Values) -> Vec<Bound> {
    match values {
        Values::One(b) => vec![b.clone()],
        Values::List(Some(vs)) => vs
            .iter()
            .map(|v| v.clone().map(Bound::Val).unwrap_or(Bound::Null))
            .collect(),
        Values::List(None) => Vec::new(),
    }
}

fn cassandra(c: &BoundCondition, view: &View) -> Result<bool> {
    let t = view.table;
    let col = &t.columns[c.cond.column];
    let op = c.cond.op;
    let current = view.value(c.cond.column);
    let values = bound_values(&c.values);
    match &c.cond.target {
        CondTarget::Column if col.ty.is_collection() && col.ty.is_multicell() => {
            // `MultiCellCollectionBound`.
            for v in &values {
                let empty = matches!(v, Bound::Val(b) if is_empty_collection(b));
                if matches!(v, Bound::Null)
                    || (empty && !matches!(op, Operator::Contains | Operator::ContainsKey))
                {
                    match op {
                        Operator::Eq | Operator::In => {
                            if current.is_none() {
                                return Ok(true);
                            }
                            continue;
                        }
                        Operator::Neq => return Ok(current.is_some()),
                        _ if matches!(v, Bound::Null) => return Err(invalid_null(op)),
                        _ => {
                            return Err(Error::invalid(format!(
                                "Invalid comparison with an empty {} for operator \"{}\"",
                                kind_name(&col.ty),
                                op_text(op)
                            )));
                        }
                    }
                }
                let v = match v {
                    Bound::Val(v) => v.clone(),
                    // `CONTAINS` compares the unset value's empty buffer with the elements.
                    Bound::Unset if matches!(op, Operator::Contains | Operator::ContainsKey) => {
                        Bytes::new()
                    }
                    _ => return Err(Error::invalid("Invalid 'unset' value in condition")),
                };
                let v = &v;
                let Some(cur) = &current else {
                    if op == Operator::Neq {
                        return Ok(true);
                    }
                    continue;
                };
                let ok = match op {
                    Operator::Contains => contains(&col.ty, cur, v, false),
                    Operator::ContainsKey => contains(&col.ty, cur, v, true),
                    _ => {
                        let v = normalise(&col.ty, v).unwrap_or_else(|| v.clone());
                        compare_op(op, nosql_types::compare(&frozen(&col.ty), cur, &v))
                    }
                };
                if ok {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        CondTarget::Column => {
            for v in &values {
                if cassandra_compare(op, &col.ty, v, current.as_deref())? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        CondTarget::Element(_) => {
            let is_map = matches!(col.ty, CqlType::Map { .. });
            let key = match &c.element {
                Some(Bound::Val(k)) => k.clone(),
                Some(Bound::Unset) if is_map => Bytes::new(),
                Some(Bound::Unset) => {
                    return Err(Error::server("java.lang.IndexOutOfBoundsException"));
                }
                _ => {
                    return Err(Error::invalid(format!(
                        "Invalid null value for {} element access",
                        if is_map { "map" } else { "list" }
                    )));
                }
            };
            if !is_map {
                let i = <[u8; 4]>::try_from(&key[..])
                    .map(i32::from_be_bytes)
                    .map_err(|_| Error::server("java.lang.IndexOutOfBoundsException"))?;
                if i < 0 {
                    return Err(Error::invalid(format!("Invalid negative list index {i}")));
                }
            }
            let row_value = current
                .as_deref()
                .and_then(|cur| element_of(&col.ty, cur, &key));
            // `ElementAccessBound.appliesTo` compares map values with the map's key type.
            let vt = match &col.ty {
                CqlType::Map { key, .. } => key.as_ref(),
                CqlType::List { elem, .. } => elem.as_ref(),
                _ => &col.ty,
            };
            for v in &values {
                if cassandra_compare(op, vt, v, row_value.as_deref())? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        CondTarget::Field(i) => {
            let CqlType::Udt(def, _) = &col.ty else {
                return Ok(false);
            };
            let ft = &def.fields[*i].1;
            let row_value = current.as_deref().and_then(|cur| field_of(cur, *i));
            for v in &values {
                if cassandra_compare(op, ft, v, row_value.as_deref())? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

/// ScyllaDB: `col op value` with SQL-ish nulls except that `x = NULL` holds for a missing `x`.
fn scylla_compare(
    op: Operator,
    ty: &CqlType,
    value: Option<&[u8]>,
    current: Option<&[u8]>,
) -> Result<bool> {
    match op {
        Operator::Eq => Ok(match (current, value) {
            (None, None) => true,
            (Some(a), Some(b)) => nosql_types::compare(&frozen(ty), a, b) == Ordering::Equal,
            _ => false,
        }),
        Operator::Neq => scylla_compare(Operator::Eq, ty, value, current).map(|b| !b),
        Operator::Like => Ok(match (current, value) {
            (Some(s), Some(p)) => crate::prepare::restrictions::like(
                &String::from_utf8_lossy(p),
                &String::from_utf8_lossy(s),
            ),
            _ => false,
        }),
        _ => {
            let Some(value) = value else {
                return Err(invalid_null(op));
            };
            let Some(current) = current else {
                return Ok(false);
            };
            Ok(compare_op(
                op,
                nosql_types::compare(&frozen(ty), current, value),
            ))
        }
    }
}

fn scylla(c: &BoundCondition, view: &View) -> Result<bool> {
    let t = view.table;
    let col = &t.columns[c.cond.column];
    let op = c.cond.op;
    let current = view.value(c.cond.column);
    let (cur, ty): (Option<Bytes>, &CqlType) = match &c.cond.target {
        CondTarget::Column => (current, &col.ty),
        CondTarget::Element(k) => {
            let et = match &col.ty {
                CqlType::List { elem, .. } | CqlType::Set { elem, .. } => elem.as_ref(),
                CqlType::Map { value, .. } => value.as_ref(),
                _ => &col.ty,
            };
            // The subscript is only looked at in a non-null collection; a null one (or a
            // negative index) gives a null element.
            let element = match (&current, &c.element) {
                (None, _) => None,
                (Some(_), Some(Bound::Unset)) => {
                    return Err(msg::lwt_unset_value(Dialect::Scylla, bind_index(k)));
                }
                (Some(v), Some(Bound::Val(key))) => element_of(&col.ty, v, key),
                _ => None,
            };
            (element, et)
        }
        CondTarget::Field(i) => {
            let CqlType::Udt(def, _) = &col.ty else {
                return Ok(false);
            };
            (
                current.as_deref().and_then(|v| field_of(v, *i)),
                &def.fields[*i].1,
            )
        }
    };
    // An empty literal of a non-frozen collection is null.
    let as_value = |b: &Bound| -> Option<Bytes> {
        match b {
            Bound::Val(v)
                if matches!(c.cond.target, CondTarget::Column)
                    && col.ty.is_multicell()
                    && col.ty.is_collection()
                    && is_empty_collection(v) =>
            {
                None
            }
            Bound::Val(v) => Some(normalise(ty, v).unwrap_or_else(|| v.clone())),
            _ => None,
        }
    };
    match (&c.values, op) {
        (Values::List(list), Operator::In | Operator::NotIn) => {
            // A null list holds nothing: `IN` is false, `NOT IN` true.
            let Some(list) = list else {
                return Ok(op == Operator::NotIn);
            };
            let mut found = false;
            for v in list {
                let v = v.as_ref().and_then(|v| as_value(&Bound::Val(v.clone())));
                if scylla_compare(Operator::Eq, ty, v.as_deref(), cur.as_deref())? {
                    found = true;
                    break;
                }
            }
            Ok(found == (op == Operator::In))
        }
        (Values::One(b), _) => scylla_compare(op, ty, as_value(b).as_deref(), cur.as_deref()),
        (Values::List(_), _) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_hashes() {
        // `ByteBuffer.wrap("v".getBytes()).hashCode()` = 31 + 'v'.
        assert_eq!(buffer_hash(b"v"), 31 + b'v' as i32);
        assert_eq!(buffer_hash(b""), 1);
        assert_eq!(buffer_hash(&[0xff]), 31 - 1);
    }

    #[test]
    fn comparisons() {
        let i = |v: i32| Bytes::copy_from_slice(&v.to_be_bytes());
        assert!(scylla_compare(Operator::Eq, &CqlType::Int, None, None).unwrap());
        assert!(!scylla_compare(Operator::Gt, &CqlType::Int, Some(&i(1)), None).unwrap());
        assert_eq!(
            scylla_compare(Operator::Gt, &CqlType::Int, None, Some(&i(1)))
                .unwrap_err()
                .message,
            "Invalid comparison with null for operator \">\""
        );
        assert!(cassandra_compare(Operator::Neq, &CqlType::Int, &Bound::Val(i(1)), None).unwrap());
        assert!(
            cassandra_compare(Operator::Lt, &CqlType::Int, &Bound::Val(i(2)), Some(&i(1))).unwrap()
        );
    }
}
