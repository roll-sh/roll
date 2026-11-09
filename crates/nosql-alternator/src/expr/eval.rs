//! Evaluating conditions (ConditionExpression, FilterExpression) over items.
//!
//! The semantics are ScyllaDB's, which differ from DynamoDB in places:
//!
//! - `=`, `<>`, `IN` and `contains` compare the JSON of the values: numbers by their text
//!   (`{"N":"5"}` is not `{"N":"5.0"}`; stored top-level numbers read back canonical), sets
//!   ignoring element order, maps ignoring member order, lists in order.
//! - `<`, `<=`, `>`, `>=` and `BETWEEN` need both sides of one type among S, N and B (bytewise for
//!   strings and binaries, numeric for numbers) and are false otherwise. A constant of another
//!   type is an error, raised even when the item doesn't exist.
//! - A missing attribute makes every comparison false except `<>`, which is true.
//! - Errors that depend on an item's values are raised only when evaluation reaches them, so
//!   `AND`/`OR` short-circuit them, and a filter on an empty result never fails.

use std::borrow::Cow;

use super::ast::{Call, CmpOp, Cond, Operand};
use super::resolve::{Bindings, RPath, Step};
use crate::error::{Error, Result};
use crate::json::Json;
use crate::number::Number;
use crate::storage::single_member;
use crate::value::{self, Item, TYPES};

/// A resolved operand.
#[derive(Clone, Debug, PartialEq)]
pub enum ROp {
    Path(RPath),
    Value(Json),
    Call(RCall),
}

#[derive(Clone, Debug, PartialEq)]
pub struct RCall {
    pub name: String,
    pub args: Vec<ROp>,
}

/// A resolved condition.
#[derive(Clone, Debug, PartialEq)]
pub enum RCond {
    Or(Vec<RCond>),
    And(Vec<RCond>),
    Not(Box<RCond>),
    Cmp(ROp, CmpOp, ROp),
    Between(ROp, ROp, ROp),
    In(ROp, Vec<ROp>),
    Call(RCall),
    /// Always true (an absent condition).
    True,
    /// Legacy `Expected` / `QueryFilter` conditions.
    Legacy(crate::legacy::Conditions),
}

/// Functions that are conditions, not values.
const BOOLEAN_FUNCTIONS: [&str; 5] = [
    "attribute_exists",
    "attribute_not_exists",
    "attribute_type",
    "begins_with",
    "contains",
];
/// Functions that are values in conditions (`size`) or only in updates.
const VALUE_FUNCTIONS: [&str; 3] = ["size", "list_append", "if_not_exists"];

/// Resolves a parsed condition. `kind` prefixes the "not allowed here" errors (ScyllaDB says
/// `ConditionExpression` for filters too).
pub fn resolve_cond(c: &Cond, b: &mut Bindings, kind: &str) -> Result<RCond> {
    // Iterative (post-order with an explicit stack): nesting costs no native stack.
    enum Frame<'c> {
        List {
            or: bool,
            items: &'c [Cond],
            next: usize,
            out: Vec<RCond>,
        },
        Not,
    }
    let mut stack: Vec<Frame> = Vec::new();
    let mut cur = c;
    loop {
        let mut done = loop {
            match cur {
                Cond::Or(v) | Cond::And(v) if !v.is_empty() => {
                    let or = matches!(cur, Cond::Or(_));
                    stack.push(Frame::List {
                        or,
                        items: v,
                        next: 1,
                        out: Vec::with_capacity(v.len()),
                    });
                    cur = &v[0];
                }
                Cond::Or(_) => break RCond::Or(Vec::new()),
                Cond::And(_) => break RCond::And(Vec::new()),
                Cond::Not(x) => {
                    stack.push(Frame::Not);
                    cur = x;
                }
                _ => break resolve_primitive(cur, b, kind)?,
            }
        };
        loop {
            match stack.last_mut() {
                None => return Ok(done),
                Some(Frame::Not) => {
                    stack.pop();
                    done = RCond::Not(Box::new(done));
                }
                Some(Frame::List {
                    items, next, out, ..
                }) => {
                    out.push(done);
                    if *next < items.len() {
                        cur = &items[*next];
                        *next += 1;
                        break;
                    }
                    let Some(Frame::List { or, out, .. }) = stack.pop() else {
                        return Err(Error::internal("condition stack"));
                    };
                    done = if or { RCond::Or(out) } else { RCond::And(out) };
                }
            }
        }
    }
}

#[inline(never)]
fn resolve_primitive(c: &Cond, b: &mut Bindings, kind: &str) -> Result<RCond> {
    Ok(match c {
        Cond::Cmp(x, op, y) => {
            let x = resolve_operand(x, b, kind)?;
            RCond::Cmp(x, *op, resolve_operand(y, b, kind)?)
        }
        Cond::Between(x, lo, hi) => {
            let x = resolve_operand(x, b, kind)?;
            let lo = resolve_operand(lo, b, kind)?;
            RCond::Between(x, lo, resolve_operand(hi, b, kind)?)
        }
        Cond::In(x, list) => {
            let x = resolve_operand(x, b, kind)?;
            let mut l = Vec::with_capacity(list.len());
            for o in list {
                l.push(resolve_operand(o, b, kind)?);
            }
            RCond::In(x, l)
        }
        Cond::Call(call) => {
            if VALUE_FUNCTIONS.contains(&call.name.as_str()) {
                return Err(Error::validation(format!(
                    "{kind}: {}() not allowed here",
                    call.name
                )));
            }
            RCond::Call(resolve_call(call, b, kind)?)
        }
        Cond::Or(_) | Cond::And(_) | Cond::Not(_) => resolve_cond(c, b, kind)?,
    })
}

fn resolve_call(call: &Call, b: &mut Bindings, kind: &str) -> Result<RCall> {
    let mut args = Vec::with_capacity(call.args.len());
    for a in &call.args {
        let r = match (call.name.as_str(), a) {
            // Value arguments of these functions may not be function calls.
            ("begins_with" | "contains", Operand::Call(inner)) => {
                return Err(Error::validation(format!(
                    "{kind}: {}() not allowed here",
                    inner.name
                )));
            }
            // Others take whatever they are given; evaluation rejects what isn't a path.
            (n, Operand::Call(inner)) if n != "size" => ROp::Call(resolve_call(inner, b, kind)?),
            _ => resolve_operand(a, b, kind)?,
        };
        args.push(r);
    }
    Ok(RCall {
        name: call.name.clone(),
        args,
    })
}

pub fn resolve_operand(o: &Operand, b: &mut Bindings, kind: &str) -> Result<ROp> {
    Ok(match o {
        Operand::Path(p) => ROp::Path(b.path(p)?),
        Operand::Value(v) => ROp::Value(b.value(v)?),
        Operand::Call(call) => {
            let n = call.name.as_str();
            if BOOLEAN_FUNCTIONS.contains(&n) || n == "list_append" || n == "if_not_exists" {
                return Err(Error::validation(format!("{kind}: {n}() not allowed here")));
            }
            ROp::Call(resolve_call(call, b, kind)?)
        }
    })
}

/// Looks up a document path in an item.
pub fn lookup<'a>(item: Option<&'a Item>, p: &RPath) -> Option<&'a Json> {
    let mut v = item?.get(&p.root)?;
    for s in &p.steps {
        v = step(v, s)?;
    }
    Some(v)
}

/// One step into a value: `.field` of an `M`, `[i]` of an `L`.
pub fn step<'a>(v: &'a Json, s: &Step) -> Option<&'a Json> {
    let (tag, payload) = single_member(v)?;
    match (s, tag) {
        (Step::Field(f), "M") => payload.get(f),
        (Step::Index(i), "L") => payload.as_array()?.get(*i as usize),
        _ => None,
    }
}

/// Deep equality as ScyllaDB compares attribute values.
pub fn json_eq(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Object(x), Json::Object(y)) => {
            if let ([(ta, Json::Array(ea))], [(tb, Json::Array(eb))]) = (x.as_slice(), y.as_slice())
                && ta == tb
                && matches!(ta.as_str(), "SS" | "NS" | "BS")
            {
                return ea.len() == eb.len()
                    && ea.iter().all(|e| eb.iter().any(|f| json_eq(e, f)))
                    && eb.iter().all(|f| ea.iter().any(|e| json_eq(e, f)));
            }
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.iter().any(|(k2, v2)| k == k2 && json_eq(v, v2)))
        }
        (Json::Array(x), Json::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| json_eq(p, q))
        }
        (Json::Number(x), Json::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// An evaluated operand: borrowed from the item or the request, or computed (`size`).
type Val<'a> = Option<Cow<'a, Json>>;

pub struct Evaluator<'a> {
    pub item: Option<&'a Item>,
    /// Prefix of function errors (`ConditionExpression`).
    pub kind: &'a str,
}

fn ordering_name(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Lt => "LT",
        CmpOp::Le => "LE",
        CmpOp::Gt => "GT",
        CmpOp::Ge => "GE",
        CmpOp::Eq => "EQ",
        CmpOp::Ne => "NE",
    }
}

/// Whether `v` is an S, N or B value.
fn is_scalar_key_type(v: &Json) -> bool {
    matches!(value::type_tag(v), Some("S" | "N" | "B"))
}

fn invalid_binary() -> Error {
    Error::new(
        crate::error::ErrorKind::Serialization,
        "Invalid base64 data",
    )
}

/// A binary operand of `begins_with` / `contains` must decode.
fn check_binary(v: &Json) -> Result<()> {
    match single_member(v) {
        Some(("B", Json::String(s))) if value::decode_base64(s).is_none() => Err(invalid_binary()),
        _ => Ok(()),
    }
}

/// Compares two values of the same scalar type (S, N, B); `None` when they aren't comparable.
pub fn compare_scalars(a: &Json, b: &Json) -> Result<Option<std::cmp::Ordering>> {
    let (Some((ta, pa)), Some((tb, pb))) = (single_member(a), single_member(b)) else {
        return Ok(None);
    };
    if ta != tb {
        return Ok(None);
    }
    let (Some(sa), Some(sb)) = (pa.as_str(), pb.as_str()) else {
        return Ok(None);
    };
    Ok(match ta {
        "S" => Some(sa.as_bytes().cmp(sb.as_bytes())),
        "B" => match (value::decode_base64(sa), value::decode_base64(sb)) {
            (Some(x), Some(y)) => Some(x.cmp(&y)),
            // Comparing needs the bytes: a malformed value is an error.
            _ => return Err(invalid_binary()),
        },
        "N" => Some(Number::parse(sa)?.cmp(&Number::parse(sb)?)),
        _ => None,
    })
}

impl<'a> Evaluator<'a> {
    pub fn new(item: Option<&'a Item>) -> Evaluator<'a> {
        Evaluator {
            item,
            kind: "ConditionExpression",
        }
    }

    pub fn eval(&self, c: &'a RCond) -> Result<bool> {
        // Iterative with short-circuiting: nesting costs no native stack.
        enum Frame<'c> {
            List {
                or: bool,
                items: &'c [RCond],
                next: usize,
            },
            Not,
        }
        let mut stack: Vec<Frame> = Vec::new();
        let mut cur = c;
        loop {
            let mut r = loop {
                match cur {
                    RCond::Or(v) | RCond::And(v) if !v.is_empty() => {
                        stack.push(Frame::List {
                            or: matches!(cur, RCond::Or(_)),
                            items: v,
                            next: 1,
                        });
                        cur = &v[0];
                    }
                    RCond::Or(_) => break false,
                    RCond::And(_) => break true,
                    RCond::Not(x) => {
                        stack.push(Frame::Not);
                        cur = x;
                    }
                    _ => break self.primitive(cur)?,
                }
            };
            loop {
                match stack.last_mut() {
                    None => return Ok(r),
                    Some(Frame::Not) => {
                        stack.pop();
                        r = !r;
                    }
                    Some(Frame::List { or, items, next }) => {
                        let decided = if *or { r } else { !r };
                        if !decided && *next < items.len() {
                            cur = &items[*next];
                            *next += 1;
                            break;
                        }
                        stack.pop();
                    }
                }
            }
        }
    }

    #[inline(never)]
    fn primitive(&self, c: &'a RCond) -> Result<bool> {
        Ok(match c {
            RCond::True => true,
            RCond::Legacy(l) => l.eval(self.item)?,
            RCond::Cmp(x, op, y) => self.compare(x, *op, y)?,
            RCond::Between(x, lo, hi) => self.between(x, lo, hi)?,
            RCond::In(x, list) => {
                let Some(v) = self.value(x)? else {
                    return Ok(false);
                };
                for o in list {
                    if let Some(w) = self.value(o)?
                        && json_eq(&v, &w)
                    {
                        return Ok(true);
                    }
                }
                false
            }
            RCond::Call(call) => self.function(call)?,
            RCond::Or(_) | RCond::And(_) | RCond::Not(_) => self.eval(c)?,
        })
    }

    /// An operand that must be an attribute value (a JSON null from `size()` of a missing
    /// attribute is `invalid object: null`).
    fn value(&self, o: &'a ROp) -> Result<Val<'a>> {
        let v = self.operand(o)?;
        if let Some(j) = &v
            && !j.is_object()
        {
            return Err(Error::validation(format!(
                "invalid object: {}",
                j.to_json_string()
            )));
        }
        Ok(v)
    }

    fn operand(&self, o: &'a ROp) -> Result<Val<'a>> {
        Ok(match o {
            ROp::Path(p) => lookup(self.item, p).map(Cow::Borrowed),
            ROp::Value(v) => Some(Cow::Borrowed(v)),
            ROp::Call(call) => match call.name.as_str() {
                "size" => self.size(call)?.map(Cow::Owned),
                n => {
                    return Err(Error::validation(format!(
                        "{}: unknown function '{n}' called.",
                        self.kind
                    )));
                }
            },
        })
    }

    fn arity(&self, call: &RCall, n: usize) -> Result<()> {
        if call.args.len() != n {
            let s = if n == 1 { "parameter" } else { "parameters" };
            return Err(Error::validation(format!(
                "{}: {}() accepts {n} {s}, got {}",
                self.kind,
                call.name,
                call.args.len()
            )));
        }
        Ok(())
    }

    /// `size()`. Of a missing attribute it is a JSON null, which any comparison then rejects
    /// (`invalid object: null`), and which a further `size()` calls an unsupported type.
    fn size(&self, call: &'a RCall) -> Result<Option<Json>> {
        self.arity(call, 1)?;
        let arg = &call.args[0];
        let Some(v) = self.operand(arg)? else {
            return Ok(Some(Json::Null));
        };
        let Some((tag, payload)) = single_member(&v) else {
            return Err(Error::validation(format!(
                "Unsupported operand type \"{}\" for function size()",
                v.to_json_string()
            )));
        };
        let n = match (tag, payload) {
            ("S", Json::String(s)) => s.len(),
            ("B", Json::String(s)) => value::decode_base64(s).map_or(s.len(), |b| b.len()),
            ("SS" | "NS" | "BS" | "L", Json::Array(a)) => a.len(),
            ("M", Json::Object(m)) => m.len(),
            _ => {
                if !matches!(arg, ROp::Path(_)) {
                    return Err(Error::validation(format!(
                        "Unsupported operand type \"{tag}\" for function size()"
                    )));
                }
                return Ok(None);
            }
        };
        Ok(Some(value::n(&Number::from_i64(n as i64))))
    }

    fn check_constant(&self, o: &ROp, what: &str) -> Result<()> {
        if let ROp::Value(v) = o
            && !is_scalar_key_type(v)
        {
            return Err(Error::validation(format!(
                "{what} allow only the types String, Number, or Binary"
            )));
        }
        Ok(())
    }

    fn compare(&self, x: &'a ROp, op: CmpOp, y: &'a ROp) -> Result<bool> {
        if matches!(op, CmpOp::Eq | CmpOp::Ne) {
            let a = self.value(x)?;
            let b = self.value(y)?;
            let eq = matches!((&a, &b), (Some(a), Some(b)) if json_eq(a, b));
            return Ok(if op == CmpOp::Eq { eq } else { !eq });
        }
        let what = format!("{} operator", ordering_name(op));
        self.check_constant(x, &what)?;
        self.check_constant(y, &what)?;
        let (Some(a), Some(b)) = (self.value(x)?, self.value(y)?) else {
            return Ok(false);
        };
        let Some(o) = compare_scalars(&a, &b)? else {
            return Ok(false);
        };
        Ok(match op {
            CmpOp::Lt => o.is_lt(),
            CmpOp::Le => o.is_le(),
            CmpOp::Gt => o.is_gt(),
            _ => o.is_ge(),
        })
    }

    fn between(&self, x: &'a ROp, lo: &'a ROp, hi: &'a ROp) -> Result<bool> {
        for o in [x, lo, hi] {
            self.check_constant(o, "between")?;
        }
        let Some(v) = self.value(x)? else {
            return Ok(false);
        };
        let (Some(l), Some(h)) = (self.value(lo)?, self.value(hi)?) else {
            return Ok(false);
        };
        let (Some((tl, pl)), Some((th, ph))) = (single_member(&l), single_member(&h)) else {
            return Ok(false);
        };
        // Bounds are only checked against each other when both come from the request.
        if !(matches!(lo, ROp::Value(_)) && matches!(hi, ROp::Value(_))) {
            let (Some(a), Some(b)) = (compare_scalars(&l, &v)?, compare_scalars(&v, &h)?) else {
                return Ok(false);
            };
            return Ok(a.is_le() && b.is_le());
        }
        if tl != th {
            return Err(Error::validation(format!(
                "BETWEEN operator requires the same type for lower and upper bound; instead got \"{tl}\" and \"{th}\""
            )));
        }
        if compare_scalars(&l, &h)?.is_some_and(|o| o.is_gt()) {
            return Err(Error::validation(format!(
                "BETWEEN operator requires lower_bound <= upper_bound, but {} > {}",
                bound_text(tl, pl),
                bound_text(th, ph)
            )));
        }
        let (Some(a), Some(b)) = (compare_scalars(&l, &v)?, compare_scalars(&v, &h)?) else {
            return Ok(false);
        };
        Ok(a.is_le() && b.is_le())
    }

    fn function(&self, call: &'a RCall) -> Result<bool> {
        let kind = self.kind;
        match call.name.as_str() {
            n @ ("attribute_exists" | "attribute_not_exists") => {
                self.arity(call, 1)?;
                let ROp::Path(p) = &call.args[0] else {
                    return Err(Error::validation(format!(
                        "{kind}: {n}()'s parameter must be a path"
                    )));
                };
                let exists = lookup(self.item, p).is_some();
                Ok(if n == "attribute_exists" {
                    exists
                } else {
                    !exists
                })
            }
            "attribute_type" => {
                self.arity(call, 2)?;
                let ROp::Value(t) = &call.args[1] else {
                    return Err(Error::validation(format!(
                        "{kind}: attribute_types()'s first parameter must be an expression attribute"
                    )));
                };
                let Some(("S", Json::String(t))) = single_member(t) else {
                    return Err(Error::validation(format!(
                        "{kind}: attribute_type() second parameter must refer to a string, got {}",
                        t.to_json_string()
                    )));
                };
                if !TYPES.contains(&t.as_str()) {
                    return Err(Error::validation(format!(
                        "{kind}: attribute_types()'s second parameter, \"{t}\", is not a known type"
                    )));
                }
                let v = self.operand(&call.args[0])?;
                Ok(v.as_deref().and_then(value::type_tag) == Some(t.as_str()))
            }
            "begins_with" => {
                self.arity(call, 2)?;
                let both_constant = call.args.iter().all(|a| matches!(a, ROp::Value(_)));
                for a in &call.args {
                    if let ROp::Value(v) = a
                        && !matches!(value::type_tag(v), Some("S" | "B"))
                    {
                        let paren = if both_constant { "" } else { "()" };
                        return Err(Error::validation(format!(
                            "begins_with{paren} supports only string or binary type, got: {}",
                            v.to_json_string()
                        )));
                    }
                }
                let (a, b) = (self.value(&call.args[0])?, self.value(&call.args[1])?);
                for v in a.iter().chain(b.iter()) {
                    check_binary(v)?;
                }
                let (Some(a), Some(b)) = (a, b) else {
                    return Ok(false);
                };
                Ok(begins_with(&a, &b))
            }
            "contains" => {
                self.arity(call, 2)?;
                let (a, b) = (self.value(&call.args[0])?, self.value(&call.args[1])?);
                for v in a.iter().chain(b.iter()) {
                    check_binary(v)?;
                }
                let (Some(a), Some(b)) = (a, b) else {
                    return Ok(false);
                };
                Ok(contains(&a, &b))
            }
            n => Err(Error::validation(format!(
                "{kind}: unknown function '{n}' called."
            ))),
        }
    }
}

/// A BETWEEN bound as ScyllaDB prints it: the string or number, or a binary in hex.
pub fn bound_text(tag: &str, payload: &Json) -> String {
    let s = payload.as_str().unwrap_or_default();
    match tag {
        "B" => value::decode_base64(s).map_or_else(|| s.to_string(), hex::encode),
        _ => s.to_string(),
    }
}

/// `begins_with` of two S or two B values.
pub fn begins_with(a: &Json, b: &Json) -> bool {
    match (single_member(a), single_member(b)) {
        (Some(("S", Json::String(x))), Some(("S", Json::String(y)))) => x.starts_with(y.as_str()),
        (Some(("B", Json::String(x))), Some(("B", Json::String(y)))) => {
            match (value::decode_base64(x), value::decode_base64(y)) {
                (Some(x), Some(y)) => x.starts_with(&y),
                _ => false,
            }
        }
        _ => false,
    }
}

/// `contains`: substring, set membership or list membership.
pub fn contains(a: &Json, b: &Json) -> bool {
    let (Some((ta, pa)), Some((tb, pb))) = (single_member(a), single_member(b)) else {
        return false;
    };
    match (ta, pa) {
        ("S", Json::String(x)) => tb == "S" && pb.as_str().is_some_and(|y| x.contains(y)),
        ("B", Json::String(x)) => {
            tb == "B"
                && match (
                    value::decode_base64(x),
                    pb.as_str().and_then(value::decode_base64),
                ) {
                    (Some(x), Some(y)) => {
                        y.is_empty() || x.windows(y.len()).any(|w| w == y.as_slice())
                    }
                    _ => false,
                }
        }
        ("SS" | "NS" | "BS", Json::Array(elems)) => {
            ta.strip_suffix('S') == Some(tb) && elems.iter().any(|e| json_eq(e, pb))
        }
        ("L", Json::Array(elems)) => elems.iter().any(|e| json_eq(e, b)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::parse::parse_condition;
    use crate::json::parse;

    fn eval(expr: &str, values: &str, item: &str) -> Result<bool> {
        let body =
            parse(format!(r#"{{"ExpressionAttributeValues":{values}}}"#).as_bytes()).unwrap();
        let mut b = Bindings::new(&body);
        let c = resolve_cond(
            &parse_condition(expr).unwrap(),
            &mut b,
            "ConditionExpression",
        )?;
        let item: Option<Item> = (!item.is_empty()).then(|| {
            let j = parse(item.as_bytes()).unwrap();
            j.as_object().unwrap().iter().cloned().collect()
        });
        Evaluator::new(item.as_ref()).eval(&c)
    }

    #[test]
    fn alternating_nesting_is_shallow_enough() {
        // 400 alternating AND/OR parentheses: the deepest tree the parser accepts.
        let mut s = String::new();
        for i in 0..400 {
            s.push_str(if i % 2 == 0 {
                "a = :v AND ("
            } else {
                "a = :v OR ("
            });
        }
        s.push_str("a = :v");
        s.push_str(&")".repeat(400));
        let r = std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(move || eval(&s, r#"{":v":{"N":"1"}}"#, r#"{"a":{"N":"1"}}"#).unwrap())
            .unwrap()
            .join();
        assert!(r.unwrap());
    }

    #[test]
    fn comparisons() {
        let item =
            r#"{"a":{"N":"5"},"s":{"S":"abc"},"ss":{"SS":["x","y"]},"m":{"M":{"k":{"N":"1"}}}}"#;
        let v = r#"{":five":{"N":"5"},":fivep":{"N":"5.0"},":six":{"N":"6"},":ab":{"S":"ab"},":set":{"SS":["y","x"]},":l":{"L":[]}}"#;
        assert!(eval("a = :five", v, item).unwrap());
        assert!(!eval("a = :fivep", v, item).unwrap());
        assert!(eval("a <= :fivep", v, item).unwrap());
        assert!(eval("a < :six", v, item).unwrap());
        assert!(eval("ss = :set", v, item).unwrap());
        assert!(eval("missing <> :five", v, item).unwrap());
        assert!(!eval("missing = :five", v, item).unwrap());
        assert!(eval("begins_with(s, :ab)", v, item).unwrap());
        assert!(eval("contains(s, :ab)", v, item).unwrap());
        assert!(eval("m.k = m.k", v, item).unwrap());
        assert!(eval("a BETWEEN :five AND :six", v, item).unwrap());
        assert_eq!(
            eval("a < :l", v, "").unwrap_err().message,
            "LT operator allow only the types String, Number, or Binary"
        );
        assert_eq!(
            eval("a BETWEEN :six AND :five", v, item)
                .unwrap_err()
                .message,
            "BETWEEN operator requires lower_bound <= upper_bound, but 6 > 5"
        );
        assert!(!eval("missing BETWEEN :six AND :five", v, item).unwrap());
        assert_eq!(
            eval("size(zz) = :five", v, item).unwrap_err().message,
            "invalid object: null"
        );
        assert!(eval("size(s) < :five", v, item).unwrap());
        assert!(eval("attribute_exists(a) OR size(zz) = :five", v, item).unwrap());
        assert_eq!(
            eval("dog(a)", v, item).unwrap_err().message,
            "ConditionExpression: unknown function 'dog' called."
        );
    }
}
