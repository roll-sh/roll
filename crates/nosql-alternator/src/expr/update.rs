//! UpdateExpression: resolution and application to an item.
//!
//! Right-hand sides are evaluated against the item as it was before the update, and list
//! indexes refer to the list before the update: `SET l[i]` past the end appends (in index
//! order) and `REMOVE l[i]` removes the original element `i`. Removing a map member moves the
//! map's last member into its slot, as RapidJSON's `RemoveMember` does (member order is only
//! visible in the stored JSON). Deleting the last element of a set removes the attribute.

use std::collections::BTreeMap;

use super::ast::{Action, Call, Operand, SetRhs, Update};
use super::eval::lookup;
use super::resolve::{Bindings, PathSet, RPath, Step};
use crate::error::{Error, Result};
use crate::json::Json;
use crate::number::Number;
use crate::storage::single_member;
use crate::value::{self, Item};

/// A resolved SET operand.
#[derive(Clone, Debug, PartialEq)]
pub enum RValue {
    Path(RPath),
    Value(Json),
    IfNotExists(RPath, Box<RValue>),
    ListAppend(Box<RValue>, Box<RValue>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum RSetRhs {
    Value(RValue),
    Plus(RValue, RValue),
    Minus(RValue, RValue),
}

#[derive(Clone, Debug, PartialEq)]
pub enum RAction {
    Set(RPath, RSetRhs),
    Remove(RPath),
    Add(RPath, Json),
    Delete(RPath, Json),
}

impl RAction {
    pub fn path(&self) -> &RPath {
        match self {
            RAction::Set(p, _)
            | RAction::Remove(p)
            | RAction::Add(p, _)
            | RAction::Delete(p, _) => p,
        }
    }

    /// Whether the action needs the item's current value (read-before-write).
    pub fn reads_item(&self) -> bool {
        fn reads(v: &RValue) -> bool {
            match v {
                RValue::Path(_) | RValue::IfNotExists(..) => true,
                RValue::Value(_) => false,
                RValue::ListAppend(a, b) => reads(a) || reads(b),
            }
        }
        match self {
            RAction::Set(p, rhs) => {
                !p.is_top()
                    || match rhs {
                        RSetRhs::Value(v) => reads(v),
                        RSetRhs::Plus(a, b) | RSetRhs::Minus(a, b) => reads(a) || reads(b),
                    }
            }
            RAction::Remove(p) => !p.is_top(),
            RAction::Add(..) | RAction::Delete(..) => true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RUpdate {
    pub actions: Vec<RAction>,
}

const K: &str = "UpdateExpression";

fn resolve_value(o: &Operand, b: &mut Bindings) -> Result<RValue> {
    Ok(match o {
        Operand::Path(p) => RValue::Path(b.path(p)?),
        Operand::Value(v) => {
            let v = b.value(v)?;
            value::validate_payload(&v)?;
            RValue::Value(v)
        }
        Operand::Call(c) => resolve_call(c, b)?,
    })
}

fn arity(c: &Call, n: usize) -> Result<()> {
    if c.args.len() != n {
        return Err(Error::validation(format!(
            "{K}: {}() accepts {n} parameters, got {}",
            c.name,
            c.args.len()
        )));
    }
    Ok(())
}

fn resolve_call(c: &Call, b: &mut Bindings) -> Result<RValue> {
    // Arguments are resolved first, so a missing name or value is reported before the call.
    let mut args = Vec::with_capacity(c.args.len());
    for a in &c.args {
        args.push(match a {
            Operand::Path(p) => Ok(RValue::Path(b.path(p)?)),
            other => resolve_value(other, b),
        }?);
    }
    match c.name.as_str() {
        "if_not_exists" => {
            arity(c, 2)?;
            let RValue::Path(p) = args.remove(0) else {
                return Err(Error::validation(format!(
                    "{K}: if_not_exists() must include path as its first argument"
                )));
            };
            Ok(RValue::IfNotExists(p, Box::new(args.remove(0))))
        }
        "list_append" => {
            arity(c, 2)?;
            let x = args.remove(0);
            Ok(RValue::ListAppend(Box::new(x), Box::new(args.remove(0))))
        }
        n @ ("size"
        | "attribute_exists"
        | "attribute_not_exists"
        | "attribute_type"
        | "begins_with"
        | "contains") => Err(Error::validation(format!("{K}: {n}() not allowed here"))),
        n => Err(Error::validation(format!(
            "{K}: unknown function '{n}' called."
        ))),
    }
}

/// Resolves an UpdateExpression and checks that its target paths don't overlap.
pub fn resolve(u: &Update, b: &mut Bindings) -> Result<RUpdate> {
    let mut actions = Vec::with_capacity(u.actions.len());
    for a in &u.actions {
        actions.push(match a {
            Action::Set(p, rhs) => {
                let p = b.path(p)?;
                let rhs = match rhs {
                    SetRhs::Value(v) => RSetRhs::Value(resolve_value(v, b)?),
                    SetRhs::Plus(x, y) => {
                        let x = resolve_value(x, b)?;
                        RSetRhs::Plus(x, resolve_value(y, b)?)
                    }
                    SetRhs::Minus(x, y) => {
                        let x = resolve_value(x, b)?;
                        RSetRhs::Minus(x, resolve_value(y, b)?)
                    }
                };
                RAction::Set(p, rhs)
            }
            Action::Remove(p) => RAction::Remove(b.path(p)?),
            Action::Add(p, v) => {
                let p = b.path(p)?;
                let v = b.value(v)?;
                value::validate_payload(&v)?;
                RAction::Add(p, v)
            }
            Action::Delete(p, v) => {
                let p = b.path(p)?;
                let v = b.value(v)?;
                value::validate_payload(&v)?;
                RAction::Delete(p, v)
            }
        });
    }
    Ok(RUpdate { actions })
}

/// The overlap/conflict check of an update's target paths.
pub fn check_paths(u: &RUpdate) -> Result<()> {
    let mut set = PathSet::default();
    for a in &u.actions {
        if let Err((overlap, at)) = set.insert(a.path()) {
            let what = if overlap { "overlap" } else { "conflict" };
            return Err(Error::validation(format!(
                "Invalid UpdateExpression: two document paths {what} at {at}"
            )));
        }
    }
    Ok(())
}

fn eval_value(v: &RValue, old: Option<&Item>) -> Result<Option<Json>> {
    Ok(match v {
        RValue::Path(p) => lookup(old, p).cloned(),
        RValue::Value(j) => Some(j.clone()),
        RValue::IfNotExists(p, dflt) => match lookup(old, p) {
            Some(j) => Some(j.clone()),
            None => eval_value(dflt, old)?,
        },
        RValue::ListAppend(a, b) => {
            let a = eval_value(a, old)?;
            let b = eval_value(b, old)?;
            let list = |v: &Option<Json>| match v.as_ref().and_then(single_member) {
                Some(("L", Json::Array(l))) => Some(l.clone()),
                _ => None,
            };
            let (Some(mut x), Some(y)) = (list(&a), list(&b)) else {
                return Err(Error::validation(format!(
                    "{K}: list_append() given a non-list"
                )));
            };
            x.extend(y);
            Some(value::tagged("L", Json::Array(x)))
        }
    })
}

fn number_of(v: Option<Json>) -> Result<Number> {
    let Some(v) = v else {
        return Err(Error::validation(format!("{K}: invalid number object")));
    };
    match single_member(&v) {
        Some(("N", Json::String(s))) => Number::parse(s),
        Some((t, _)) => Err(Error::validation(format!(
            "{K}: expected number, found type '\"{t}\"'"
        ))),
        None => Err(Error::validation(format!("{K}: invalid number object"))),
    }
}

fn eval_rhs(rhs: &RSetRhs, old: Option<&Item>) -> Result<Json> {
    match rhs {
        RSetRhs::Value(v) => eval_value(v, old)?.ok_or_else(|| {
            Error::validation("An item can contain only one attribute definition: null")
        }),
        RSetRhs::Plus(a, b) | RSetRhs::Minus(a, b) => {
            let x = number_of(eval_value(a, old)?)?;
            let y = number_of(eval_value(b, old)?)?;
            let r = if matches!(rhs, RSetRhs::Plus(..)) {
                x.add(&y)?
            } else {
                x.sub(&y)?
            };
            Ok(value::n(&r))
        }
    }
}

/// Edits deferred until every action has been applied, so list indexes stay those of the
/// original lists.
#[derive(Default)]
struct Deferred {
    /// List path → appended values by index.
    appends: Vec<(RPath, BTreeMap<u32, Json>)>,
    /// List path → indexes to remove.
    removals: Vec<(RPath, Vec<u32>)>,
}

fn not_valid(p: &RPath) -> Error {
    Error::validation(format!("{K}: document paths not valid for this item: {p}"))
}

/// The container value holding the last step of `p` (navigating all but the last step). A
/// missing attribute is `missing attribute <name>` (or, without an item, `non-existent
/// item`); a missing or mistyped value deeper down makes the path invalid.
fn parent_mut<'a>(item: &'a mut Item, p: &RPath, existed: bool) -> Result<&'a mut Json> {
    let Some(mut v) = item.get_mut(&p.root) else {
        return Err(if existed {
            Error::validation(format!(
                "UpdateItem cannot update document path: missing attribute {}",
                p.root
            ))
        } else {
            Error::validation("UpdateItem cannot update nested document path on non-existent item")
        });
    };
    for s in &p.steps[..p.steps.len() - 1] {
        let Json::Object(m) = v else {
            return Err(not_valid(p));
        };
        let [(tag, payload)] = m.as_mut_slice() else {
            return Err(not_valid(p));
        };
        v = match (s, tag.as_str(), payload) {
            (Step::Field(f), "M", Json::Object(members)) => {
                match members.iter_mut().find(|(k, _)| k == f) {
                    Some((_, x)) => x,
                    None => return Err(not_valid(p)),
                }
            }
            (Step::Index(i), "L", Json::Array(l)) => match l.get_mut(*i as usize) {
                Some(x) => x,
                None => return Err(not_valid(p)),
            },
            _ => return Err(not_valid(p)),
        };
    }
    Ok(v)
}

fn set_path(
    item: &mut Item,
    p: &RPath,
    value: Json,
    existed: bool,
    d: &mut Deferred,
) -> Result<()> {
    let Some(last) = p.steps.last() else {
        item.insert(p.root.clone(), value);
        return Ok(());
    };
    let parent = parent_mut(item, p, existed)?;
    let Json::Object(m) = parent else {
        return Err(not_valid(p));
    };
    let [(tag, payload)] = m.as_mut_slice() else {
        return Err(not_valid(p));
    };
    match (last, tag.as_str(), payload) {
        (Step::Field(f), "M", Json::Object(members)) => {
            match members.iter_mut().find(|(k, _)| k == f) {
                Some((_, x)) => *x = value,
                None => members.push((f.clone(), value)),
            }
            Ok(())
        }
        (Step::Index(i), "L", Json::Array(l)) => {
            if (*i as usize) < l.len() {
                l[*i as usize] = value;
            } else {
                let lp = RPath {
                    root: p.root.clone(),
                    steps: p.steps[..p.steps.len() - 1].to_vec(),
                };
                match d.appends.iter_mut().find(|(q, _)| *q == lp) {
                    Some((_, m)) => {
                        m.insert(*i, value);
                    }
                    None => d.appends.push((lp, BTreeMap::from([(*i, value)]))),
                }
            }
            Ok(())
        }
        _ => Err(not_valid(p)),
    }
}

fn remove_path(item: &mut Item, p: &RPath, existed: bool, d: &mut Deferred) -> Result<()> {
    let Some(last) = p.steps.last() else {
        item.remove(&p.root);
        return Ok(());
    };
    let parent = parent_mut(item, p, existed)?;
    let Json::Object(m) = parent else {
        return Err(not_valid(p));
    };
    let [(tag, payload)] = m.as_mut_slice() else {
        return Err(not_valid(p));
    };
    match (last, tag.as_str(), payload) {
        (Step::Field(f), "M", Json::Object(members)) => {
            if let Some(pos) = members.iter().position(|(k, _)| k == f) {
                members.swap_remove(pos);
            }
            Ok(())
        }
        (Step::Index(i), "L", Json::Array(_)) => {
            let lp = RPath {
                root: p.root.clone(),
                steps: p.steps[..p.steps.len() - 1].to_vec(),
            };
            match d.removals.iter_mut().find(|(q, _)| *q == lp) {
                Some((_, v)) => v.push(*i),
                None => d.removals.push((lp, vec![*i])),
            }
            Ok(())
        }
        _ => Err(not_valid(p)),
    }
}

fn list_at<'a>(item: &'a mut Item, p: &RPath) -> Option<&'a mut Vec<Json>> {
    let mut v = item.get_mut(&p.root)?;
    for s in &p.steps {
        let Json::Object(m) = v else { return None };
        let [(tag, payload)] = m.as_mut_slice() else {
            return None;
        };
        v = match (s, tag.as_str(), payload) {
            (Step::Field(f), "M", Json::Object(members)) => {
                &mut members.iter_mut().find(|(k, _)| k == f)?.1
            }
            (Step::Index(i), "L", Json::Array(l)) => l.get_mut(*i as usize)?,
            _ => return None,
        };
    }
    let Json::Object(m) = v else { return None };
    match m.as_mut_slice() {
        [(tag, Json::Array(l))] if tag == "L" => Some(l),
        _ => None,
    }
}

/// Elements of a set compared as ScyllaDB stores them (by their JSON text).
fn same_element(a: &Json, b: &Json) -> bool {
    a.to_json_string() == b.to_json_string()
}

fn add_value(cur: Option<&Json>, v: &Json) -> Result<Json> {
    let (vt, vp) = single_member(v).unwrap_or(("", &Json::Null));
    let Some(cur) = cur else {
        if !matches!(vt, "N" | "SS" | "NS" | "BS") {
            return Err(Error::validation(format!(
                "An operand in the update expression has an incorrect data type: {}",
                v.to_json_string()
            )));
        }
        return Ok(v.clone());
    };
    let (ct, cp) = single_member(cur).unwrap_or(("", &Json::Null));
    match ct {
        "N" => {
            if vt != "N" {
                return Err(Error::validation(format!(
                    "Incorrect operand type for operator or function. Expected N: {}",
                    v.to_json_string()
                )));
            }
            let x = Number::parse(cp.as_str().unwrap_or_default())?;
            let y = Number::parse(vp.as_str().unwrap_or_default())?;
            Ok(value::n(&x.add(&y)?))
        }
        "SS" | "NS" | "BS" => {
            if vt != ct {
                return Err(Error::validation(format!(
                    "Incorrect operand type for operator or function. Expected {ct}: {}",
                    v.to_json_string()
                )));
            }
            let mut out = cp.as_array().map(<[Json]>::to_vec).unwrap_or_default();
            for e in vp.as_array().unwrap_or_default() {
                if !out.iter().any(|x| same_element(x, e)) {
                    out.push(e.clone());
                }
            }
            Ok(value::tagged(ct, Json::Array(out)))
        }
        _ => Err(Error::validation(format!(
            "An operand in the update expression has an incorrect data type: {}",
            cur.to_json_string()
        ))),
    }
}

/// `DELETE`: the remaining set, or `None` when it becomes empty.
fn delete_value(cur: &Json, v: &Json) -> Result<Option<Json>> {
    let (ct, cp) = single_member(cur).unwrap_or(("", &Json::Null));
    let (vt, vp) = single_member(v).unwrap_or(("", &Json::Null));
    if ct != vt || !matches!(ct, "SS" | "NS" | "BS") {
        return Err(Error::validation(format!(
            "Set DELETE type mismatch: {ct} and {vt}"
        )));
    }
    let remove = vp.as_array().unwrap_or_default();
    let out: Vec<Json> = cp
        .as_array()
        .unwrap_or_default()
        .iter()
        .filter(|e| !remove.iter().any(|r| same_element(e, r)))
        .cloned()
        .collect();
    Ok((!out.is_empty()).then(|| value::tagged(ct, Json::Array(out))))
}

/// Applies an update to `item` (the old item, or a key-only item when there was none).
/// `existed` tells whether there was an old item (for the error messages).
pub fn apply(u: &RUpdate, old: Option<&Item>, item: &mut Item) -> Result<()> {
    let existed = old.is_some();
    let mut d = Deferred::default();
    for a in &u.actions {
        match a {
            RAction::Set(p, rhs) => {
                let v = eval_rhs(rhs, old)?;
                set_path(item, p, v, existed, &mut d)?;
            }
            RAction::Remove(p) => remove_path(item, p, existed, &mut d)?,
            RAction::Add(p, v) => {
                let cur = lookup(old, p);
                if !p.is_top() && cur.is_none() {
                    // ADD below a path must find its parent; without an item, the path is invalid.
                    if !existed {
                        return Err(not_valid(p));
                    }
                    parent_mut(item, p, existed)?;
                }
                let new = add_value(cur, v)?;
                set_path(item, p, new, existed, &mut d)?;
            }
            RAction::Delete(p, v) => {
                if !p.is_top() && lookup(old, p).is_none() {
                    if !existed {
                        return Err(not_valid(p));
                    }
                    parent_mut(item, p, existed)?;
                }
                let Some(cur) = lookup(old, p) else { continue };
                match delete_value(cur, v)? {
                    Some(rest) => set_path(item, p, rest, existed, &mut d)?,
                    None => remove_path(item, p, existed, &mut d)?,
                }
            }
        }
    }
    for (lp, values) in d.appends {
        if let Some(l) = list_at(item, &lp) {
            l.extend(values.into_values());
        }
    }
    for (lp, mut idx) in d.removals {
        idx.sort_unstable();
        idx.dedup();
        if let Some(l) = list_at(item, &lp) {
            for i in idx.into_iter().rev() {
                if (i as usize) < l.len() {
                    l.remove(i as usize);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::parse::parse_update;
    use crate::json::parse;

    fn run(expr: &str, values: &str, item: &str) -> Result<String> {
        let body =
            parse(format!(r#"{{"ExpressionAttributeValues":{values}}}"#).as_bytes()).unwrap();
        let mut b = Bindings::new(&body);
        let u = resolve(&parse_update(expr).unwrap(), &mut b)?;
        check_paths(&u)?;
        let old: Option<Item> = (!item.is_empty()).then(|| {
            parse(item.as_bytes())
                .unwrap()
                .as_object()
                .unwrap()
                .iter()
                .cloned()
                .collect()
        });
        let mut new = old.clone().unwrap_or_default();
        apply(&u, old.as_ref(), &mut new)?;
        Ok(Json::Object(new.into_iter().collect()).to_json_string())
    }

    #[test]
    fn updates() {
        let v =
            r#"{":one":{"N":"1"},":x":{"S":"x"},":l":{"L":[{"N":"9"}]},":ss":{"SS":["b","c"]}}"#;
        let item = r#"{"a":{"N":"5"},"l":{"L":[{"N":"0"},{"N":"1"},{"N":"2"}]},"m":{"M":{"p":{"S":"q"},"r":{"S":"s"},"t":{"S":"u"}}},"s":{"SS":["a","b"]}}"#;
        assert_eq!(
            run("SET a = a + :one", v, item).unwrap(),
            r#"{"a":{"N":"6"},"l":{"L":[{"N":"0"},{"N":"1"},{"N":"2"}]},"m":{"M":{"p":{"S":"q"},"r":{"S":"s"},"t":{"S":"u"}}},"s":{"SS":["a","b"]}}"#
        );
        let r = run(
            "SET l[5] = :x, l[1] = :one REMOVE l[0], m.p ADD s :ss",
            v,
            item,
        )
        .unwrap();
        assert_eq!(
            r,
            r#"{"a":{"N":"5"},"l":{"L":[{"N":"1"},{"N":"2"},{"S":"x"}]},"m":{"M":{"t":{"S":"u"},"r":{"S":"s"}}},"s":{"SS":["a","b","c"]}}"#
        );
        assert_eq!(
            run("SET z = list_append(l, :l)", v, r#"{"l":{"L":[]}}"#).unwrap(),
            r#"{"l":{"L":[]},"z":{"L":[{"N":"9"}]}}"#
        );
        assert_eq!(
            run("SET z = if_not_exists(q, :one)", v, "").unwrap(),
            r#"{"z":{"N":"1"}}"#
        );
        assert_eq!(
            run("DELETE s :ss", v, r#"{"s":{"SS":["b"]}}"#).unwrap(),
            "{}"
        );
        assert_eq!(
            run("SET z = nope", v, item).unwrap_err().message,
            "An item can contain only one attribute definition: null"
        );
        assert_eq!(
            run("SET z = nope + :one", v, item).unwrap_err().message,
            "UpdateExpression: invalid number object"
        );
        assert_eq!(
            run("SET z = :x + :one", v, item).unwrap_err().message,
            "UpdateExpression: expected number, found type '\"S\"'"
        );
        assert_eq!(
            run("SET q.r = :one", v, item).unwrap_err().message,
            "UpdateItem cannot update document path: missing attribute q"
        );
        assert_eq!(
            run("SET q.r = :one", v, "").unwrap_err().message,
            "UpdateItem cannot update nested document path on non-existent item"
        );
        assert_eq!(
            run("SET a.b.c = :one", v, item).unwrap_err().message,
            "UpdateExpression: document paths not valid for this item: a.b.c"
        );
        assert_eq!(
            run("SET a = :one, a = :one", v, item).unwrap_err().message,
            "Invalid UpdateExpression: two document paths overlap at a"
        );
        assert_eq!(
            run("SET z = size(a)", v, item).unwrap_err().message,
            "UpdateExpression: size() not allowed here"
        );
        assert_eq!(
            run("SET z = foo(a)", v, item).unwrap_err().message,
            "UpdateExpression: unknown function 'foo' called."
        );
        assert_eq!(
            run("DELETE a :ss", v, item).unwrap_err().message,
            "Set DELETE type mismatch: N and SS"
        );
        assert_eq!(
            run("ADD m :one", v, item)
                .unwrap_err()
                .message
                .split(':')
                .next(),
            Some("An operand in the update expression has an incorrect data type")
        );
    }
}
