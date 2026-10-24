//! The legacy (pre-expression) parameters: `Expected`, `QueryFilter` / `ScanFilter`,
//! `KeyConditions`, `AttributeUpdates`, `AttributesToGet` and `ConditionalOperator`.
//!
//! Conditions are validated for shape when parsed and for value types when evaluated (so,
//! like expressions, a filter over an empty result never fails).

use crate::error::{Error, Result};
use crate::expr::eval::{begins_with, compare_scalars, contains, json_eq};
use crate::json::Json;
use crate::number::Number;
use crate::storage::single_member;
use crate::value::{self, Item, json_assert};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Le,
    Lt,
    Ge,
    Gt,
    NotNull,
    Null,
    Contains,
    NotContains,
    BeginsWith,
    In,
    Between,
}

impl Op {
    pub fn parse(s: &str) -> Option<Op> {
        Some(match s {
            "EQ" => Op::Eq,
            "NE" => Op::Ne,
            "LE" => Op::Le,
            "LT" => Op::Lt,
            "GE" => Op::Ge,
            "GT" => Op::Gt,
            "NOT_NULL" => Op::NotNull,
            "NULL" => Op::Null,
            "CONTAINS" => Op::Contains,
            "NOT_CONTAINS" => Op::NotContains,
            "BEGINS_WITH" => Op::BeginsWith,
            "IN" => Op::In,
            "BETWEEN" => Op::Between,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Op::Eq => "EQ",
            Op::Ne => "NE",
            Op::Le => "LE",
            Op::Lt => "LT",
            Op::Ge => "GE",
            Op::Gt => "GT",
            Op::NotNull => "NOT_NULL",
            Op::Null => "NULL",
            Op::Contains => "CONTAINS",
            Op::NotContains => "NOT_CONTAINS",
            Op::BeginsWith => "BEGINS_WITH",
            Op::In => "IN",
            Op::Between => "BETWEEN",
        }
    }
}

/// One legacy condition on an attribute.
#[derive(Clone, Debug, PartialEq)]
pub enum Check {
    /// `Exists: false`.
    Absent,
    /// `ComparisonOperator` with its `AttributeValueList`.
    Cmp(Op, Vec<Json>),
}

/// A set of legacy conditions joined by `ConditionalOperator`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Conditions {
    pub checks: Vec<(String, Check)>,
    pub or: bool,
}

/// Parses `ComparisonOperator` + `AttributeValueList` of one condition.
pub fn parse_cmp(spec: &Json) -> Result<Check> {
    let op_v = spec.get("ComparisonOperator").unwrap_or(&Json::Null);
    let Some(op_s) = op_v.as_str() else {
        return Err(Error::validation(format!(
            "Invalid comparison operator definition {}",
            op_v.to_json_string()
        )));
    };
    let Some(op) = Op::parse(op_s) else {
        return Err(Error::validation(format!(
            "Unsupported comparison operator {op_s}"
        )));
    };
    let list = match spec.get("AttributeValueList") {
        None if matches!(op, Op::Null | Op::NotNull) => Vec::new(),
        Some(Json::Array(a)) => a.clone(),
        _ => {
            return Err(Error::validation(
                "With ComparisonOperator, AttributeValueList must be given and an array",
            ));
        }
    };
    let n = list.len();
    let bad = |want: &str| {
        Err(Error::validation(format!(
            "\"{}\" operator requires AttributeValueList {want}, instead found list size {n}",
            op.name()
        )))
    };
    match op {
        Op::Null | Op::NotNull if n != 0 => return bad("to be empty"),
        Op::In if n == 0 => return bad("to be non-empty"),
        Op::Between if n != 2 => return bad("of size 2"),
        Op::Eq
        | Op::Ne
        | Op::Le
        | Op::Lt
        | Op::Ge
        | Op::Gt
        | Op::Contains
        | Op::NotContains
        | Op::BeginsWith
            if n != 1 =>
        {
            return bad("of size 1");
        }
        _ => {}
    }
    Ok(Check::Cmp(op, list))
}

/// `ConditionalOperator`: `true` for OR. `what` names the parameter it goes with.
pub fn conditional_operator(body: &Json, have_conditions: bool, what: &str) -> Result<bool> {
    let Some(v) = body.get("ConditionalOperator") else {
        return Ok(false);
    };
    let Some(s) = v.as_str() else {
        return Err(Error::validation(
            "'ConditionalOperator' parameter, if given, must be a string",
        ));
    };
    let or = match s {
        "AND" => false,
        "OR" => true,
        _ => {
            return Err(Error::validation(format!(
                "'ConditionalOperator' parameter must be AND, OR or missing. Found {s}."
            )));
        }
    };
    if !have_conditions {
        return Err(Error::validation(format!(
            "'ConditionalOperator' parameter cannot be specified for missing or empty {what}"
        )));
    }
    Ok(or)
}

/// One `Expected` / `QueryFilter` / `ScanFilter` entry.
pub fn parse_condition(spec: &Json) -> Result<Check> {
    if !spec.is_object() {
        return Err(json_assert("IsObject()"));
    }
    let value = spec.get("Value");
    let exists = spec.get("Exists");
    if spec.get("ComparisonOperator").is_some() {
        if value.is_some() {
            return Err(Error::validation(
                "Cannot combine Value with ComparisonOperator",
            ));
        }
        if exists.is_some() {
            return Err(Error::validation(
                "Cannot combine Exists with ComparisonOperator",
            ));
        }
        return parse_cmp(spec);
    }
    match (value, exists) {
        (None, None) => Err(Error::validation(
            "Missing ComparisonOperator, Value or Exists",
        )),
        (Some(v), None | Some(Json::Bool(true))) => Ok(Check::Cmp(Op::Eq, vec![v.clone()])),
        (Some(_), Some(_)) => Err(Error::validation("Cannot combine Value with Exists!=true")),
        (None, Some(Json::Bool(false))) => Ok(Check::Absent),
        (None, Some(_)) => Err(Error::validation("Exists!=false requires Value")),
    }
}

/// Parses `Expected`.
pub fn parse_expected(body: &Json) -> Result<Option<Conditions>> {
    let expected = match body.get("Expected") {
        None => None,
        Some(Json::Object(m)) => Some(m),
        Some(_) => {
            return Err(Error::validation(
                "'Expected' parameter, if given, must be an object",
            ));
        }
    };
    let have = expected.is_some_and(|m| !m.is_empty());
    let or = conditional_operator(body, have, "Expression")?;
    let Some(m) = expected else { return Ok(None) };
    let mut checks = Vec::with_capacity(m.len());
    for (name, spec) in m {
        checks.push((name.clone(), parse_condition(spec)?));
    }
    Ok(Some(Conditions { checks, or }))
}

/// Parses a `QueryFilter` / `ScanFilter` / `KeyConditions` object: name → condition.
pub fn parse_filter(v: &Json) -> Result<Vec<(String, Check)>> {
    let Some(m) = v.as_object() else {
        return Err(json_assert("IsObject()"));
    };
    let mut out = Vec::with_capacity(m.len());
    for (name, spec) in m {
        out.push((name.clone(), parse_condition(spec)?));
    }
    Ok(out)
}

fn scalar(v: &Json) -> bool {
    matches!(value::type_tag(v), Some("S" | "N" | "B"))
}

/// Evaluates one condition on an attribute value (`None`: absent).
pub fn check(c: &Check, attr: Option<&Json>) -> Result<bool> {
    let (op, list) = match c {
        Check::Absent => return Ok(attr.is_none()),
        Check::Cmp(op, list) => (*op, list),
    };
    let ordering = |want: fn(std::cmp::Ordering) -> bool| -> Result<bool> {
        let v = &list[0];
        if !scalar(v) {
            return Err(Error::validation(format!(
                "{} operator allow only the types String, Number, or Binary",
                op.name()
            )));
        }
        let Some(a) = attr else { return Ok(false) };
        Ok(compare_scalars(a, v)?.is_some_and(want))
    };
    match op {
        Op::Eq => Ok(attr.is_some_and(|a| json_eq(a, &list[0]))),
        Op::Ne => Ok(!attr.is_some_and(|a| json_eq(a, &list[0]))),
        Op::Lt => ordering(|o| o.is_lt()),
        Op::Le => ordering(|o| o.is_le()),
        Op::Gt => ordering(|o| o.is_gt()),
        Op::Ge => ordering(|o| o.is_ge()),
        Op::NotNull => Ok(attr.is_some()),
        Op::Null => Ok(attr.is_none()),
        Op::Contains | Op::NotContains => {
            let v = &list[0];
            if !scalar(v) {
                return Err(Error::validation(format!(
                    "CONTAINS operator requires a single AttributeValue of type String, Number, or Binary, got \"{}\" instead",
                    value::type_tag(v).unwrap_or("")
                )));
            }
            let Some(a) = attr else { return Ok(false) };
            let c = contains(a, v);
            Ok(if op == Op::Contains { c } else { !c })
        }
        Op::BeginsWith => {
            let v = &list[0];
            if !matches!(value::type_tag(v), Some("S" | "B")) {
                return Err(Error::validation(format!(
                    "begins_with() supports only string or binary type, got: {}",
                    v.to_json_string()
                )));
            }
            Ok(attr.is_some_and(|a| begins_with(a, v)))
        }
        Op::In => {
            let t = value::type_tag(&list[0]);
            if !list.iter().all(scalar) {
                return Err(Error::validation(
                    "IN operator requires AttributeValueList elements to be of type String, Number, or Binary ",
                ));
            }
            if list.iter().any(|v| value::type_tag(v) != t) {
                return Err(Error::validation(
                    "IN operator requires all AttributeValueList elements to have the same type ",
                ));
            }
            Ok(attr.is_some_and(|a| list.iter().any(|v| json_eq(a, v))))
        }
        Op::Between => {
            let (lo, hi) = (&list[0], &list[1]);
            if !scalar(lo) || !scalar(hi) {
                return Err(Error::validation(
                    "between allow only the types String, Number, or Binary",
                ));
            }
            let (tl, th) = (
                value::type_tag(lo).unwrap_or(""),
                value::type_tag(hi).unwrap_or(""),
            );
            if tl != th {
                return Err(Error::validation(format!(
                    "BETWEEN operator requires the same type for lower and upper bound; instead got \"{tl}\" and \"{th}\""
                )));
            }
            if compare_scalars(lo, hi)?.is_some_and(|o| o.is_gt()) {
                let p = |v: &Json| {
                    single_member(v)
                        .map(|(t, p)| crate::expr::eval::bound_text(t, p))
                        .unwrap_or_default()
                };
                return Err(Error::validation(format!(
                    "BETWEEN operator requires lower_bound <= upper_bound, but {} > {}",
                    p(lo),
                    p(hi)
                )));
            }
            let Some(a) = attr else { return Ok(false) };
            let (Some(x), Some(y)) = (compare_scalars(lo, a)?, compare_scalars(a, hi)?) else {
                return Ok(false);
            };
            Ok(x.is_le() && y.is_le())
        }
    }
}

impl Conditions {
    pub fn eval(&self, item: Option<&Item>) -> Result<bool> {
        if self.checks.is_empty() {
            return Ok(true);
        }
        for (name, c) in &self.checks {
            let r = check(c, item.and_then(|i| i.get(name)))?;
            if self.or && r {
                return Ok(true);
            }
            if !self.or && !r {
                return Ok(false);
            }
        }
        Ok(!self.or)
    }
}

/// One `AttributeUpdates` entry.
#[derive(Clone, Debug, PartialEq)]
pub enum AttrUpdate {
    Put(Json),
    Delete(Option<Json>),
    Add(Json),
}

/// Parses `AttributeUpdates`.
pub fn parse_attribute_updates(v: &Json) -> Result<Vec<(String, AttrUpdate)>> {
    let Some(m) = v.as_object() else {
        return Err(Error::validation("AttributeUpdates must be an object"));
    };
    let mut out = Vec::with_capacity(m.len());
    for (name, spec) in m {
        if let Err(e) = crate::expr::resolve::check_attr_name(name) {
            return Err(Error::validation(format!("in AttributeUpdates - {e}")));
        }
        if !spec.is_object() {
            return Err(json_assert("IsObject()"));
        }
        let action = match spec.get("Action") {
            None => return Err(json_assert("IsString()")),
            Some(Json::String(s)) => s.as_str(),
            Some(_) => {
                return Err(Error::internal(
                    "Internal server error: std::runtime_error (rjson::to_string_view expects a string value, got: kNumberType)",
                ));
            }
        };
        let value = spec.get("Value");
        if let Some(v) = value {
            value::validate(v, "AttributeUpdates")?;
        }
        let u = match action {
            "PUT" => AttrUpdate::Put(value.cloned().ok_or_else(|| json_assert("false"))?),
            "ADD" => AttrUpdate::Add(value.cloned().ok_or_else(|| json_assert("false"))?),
            "DELETE" => AttrUpdate::Delete(value.cloned()),
            a => {
                return Err(Error::validation(format!(
                    "Unknown Action value '{a}' in AttributeUpdates"
                )));
            }
        };
        out.push((name.clone(), u));
    }
    Ok(out)
}

fn same_element(a: &Json, b: &Json) -> bool {
    a.to_json_string() == b.to_json_string()
}

/// Applies AttributeUpdates to `item` (which starts as the old item or the key).
pub fn apply_attribute_updates(updates: &[(String, AttrUpdate)], item: &mut Item) -> Result<()> {
    for (name, u) in updates {
        match u {
            AttrUpdate::Put(v) => {
                item.insert(name.clone(), v.clone());
            }
            AttrUpdate::Delete(None) => {
                item.remove(name);
            }
            AttrUpdate::Delete(Some(v)) => {
                let vt = value::type_tag(v).unwrap_or("");
                if !matches!(vt, "SS" | "NS" | "BS") {
                    return Err(Error::validation(format!(
                        "AttributeUpdates DELETE operation with Value only valid for sets, got type {vt}"
                    )));
                }
                let Some(cur) = item.get(name) else { continue };
                let (ct, cp) = single_member(cur).unwrap_or(("", &Json::Null));
                if ct != vt {
                    return Err(Error::validation(format!(
                        "Set DELETE type mismatch: {ct} and {vt}"
                    )));
                }
                let remove = single_member(v)
                    .and_then(|(_, p)| p.as_array())
                    .unwrap_or_default();
                let rest: Vec<Json> = cp
                    .as_array()
                    .unwrap_or_default()
                    .iter()
                    .filter(|e| !remove.iter().any(|r| same_element(e, r)))
                    .cloned()
                    .collect();
                if rest.is_empty() {
                    item.remove(name);
                } else {
                    item.insert(name.clone(), value::tagged(ct, Json::Array(rest)));
                }
            }
            AttrUpdate::Add(v) => {
                let (vt, vp) = single_member(v).unwrap_or(("", &Json::Null));
                let addable = |t: &str| matches!(t, "N" | "SS" | "NS" | "BS" | "L");
                let incorrect = |x: &Json| {
                    Error::validation(format!(
                        "An operand in the AttributeUpdates ADD has an incorrect data type: {}",
                        x.to_json_string()
                    ))
                };
                let new = match item.get(name) {
                    None => {
                        if !addable(vt) {
                            return Err(incorrect(v));
                        }
                        v.clone()
                    }
                    Some(cur) => {
                        let (ct, cp) = single_member(cur).unwrap_or(("", &Json::Null));
                        if ct != vt {
                            return Err(Error::validation(format!(
                                "Operand type mismatch in AttributeUpdates ADD. Expected {ct}, got {vt}"
                            )));
                        }
                        if !addable(ct) {
                            return Err(incorrect(cur));
                        }
                        match ct {
                            "N" => {
                                let x = Number::parse(cp.as_str().unwrap_or_default())?;
                                let y = Number::parse(vp.as_str().unwrap_or_default())?;
                                value::n(&x.add(&y)?)
                            }
                            "L" => {
                                let mut l = cp.as_array().map(<[Json]>::to_vec).unwrap_or_default();
                                l.extend(vp.as_array().unwrap_or_default().iter().cloned());
                                value::tagged("L", Json::Array(l))
                            }
                            _ => {
                                let mut out =
                                    cp.as_array().map(<[Json]>::to_vec).unwrap_or_default();
                                for e in vp.as_array().unwrap_or_default() {
                                    if !out.iter().any(|x| same_element(x, e)) {
                                        out.push(e.clone());
                                    }
                                }
                                value::tagged(ct, Json::Array(out))
                            }
                        }
                    }
                };
                item.insert(name.clone(), new);
            }
        }
    }
    Ok(())
}

/// `AttributesToGet`: validated names, in order.
pub fn attributes_to_get(v: &Json) -> Result<Vec<String>> {
    let Some(a) = v.as_array() else {
        return Err(json_assert("IsArray()"));
    };
    if a.is_empty() {
        return Err(Error::validation(
            "Empty AttributesToGet is not allowed. Consider using Select=COUNT instead.",
        ));
    }
    let mut out: Vec<String> = Vec::with_capacity(a.len());
    for n in a {
        let Some(n) = n.as_str() else {
            return Err(Error::internal(format!(
                "Internal server error: std::runtime_error (rjson::to_string expects a string value, got: {})",
                crate::ops::rapid_type(n)
            )));
        };
        if let Err(e) = crate::expr::resolve::check_attr_name(n) {
            return Err(Error::validation(format!("in AttributesToGet - {e}")));
        }
        if out.iter().any(|x| x == n) {
            return Err(Error::validation(format!(
                "Invalid AttributesToGet: Duplicate attribute: {n}"
            )));
        }
        out.push(n.to_string());
    }
    Ok(out)
}
