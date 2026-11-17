//! The `compare` and `array_compare` conditions (`CompareCondition`, `ArrayCompareCondition`).

use super::exec::resolve;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;

/// A compare value: `{{ctx.path}}` references and `<{now-5m}>` date math resolve first.
fn compare_value(model: &Value, v: &Value, now: i64) -> Value {
    if let Value::Str(s) = v {
        if let Some(path) = s.strip_prefix("{{").and_then(|x| x.strip_suffix("}}")) {
            return resolve(model, path.trim()).cloned().unwrap_or(Value::Null);
        }
        if let Some(expr) = s.strip_prefix("<{").and_then(|x| x.strip_suffix("}>"))
            && let Ok(f) = crate::time::DateFormatter::new("strict_date_optional_time||epoch_millis")
            && let Ok(i) = crate::time::parse_date_math(expr, now, false, None, &f)
        {
            return Value::from(crate::time::java_instant_string(i.millis()));
        }
    }
    v.clone()
}

fn cmp_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Num(_), Value::Num(_)) => a.as_f64()?.partial_cmp(&b.as_f64()?),
        (Value::Str(x), Value::Str(y)) => {
            let fmt = crate::time::DateFormatter::new("strict_date_optional_time").ok()?;
            match (fmt.parse_millis(x), fmt.parse_millis(y)) {
                (Ok(p), Ok(q)) => Some(p.cmp(&q)),
                _ => Some(x.cmp(y)),
            }
        }
        (Value::Num(_), Value::Str(s)) => a.as_f64()?.partial_cmp(&s.parse::<f64>().ok()?),
        (Value::Str(s), Value::Num(_)) => s.parse::<f64>().ok()?.partial_cmp(&b.as_f64()?),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::Null, Value::Null) => Some(std::cmp::Ordering::Equal),
        _ => None,
    }
}

fn op_holds(op: &str, actual: &Value, expected: &Value) -> bool {
    use std::cmp::Ordering::*;
    let c = cmp_values(actual, expected);
    match op {
        "eq" => c == Some(Equal) || actual == expected,
        "not_eq" => !(c == Some(Equal) || actual == expected),
        "gt" => c == Some(Greater),
        "gte" => matches!(c, Some(Greater | Equal)),
        "lt" => c == Some(Less),
        "lte" => matches!(c, Some(Less | Equal)),
        _ => false,
    }
}

/// Evaluates a `compare` or `array_compare` condition against the execution model:
/// `(met, {resolved_values})`.
pub fn eval(ty: &str, body: &Value, model: &Value, now: i64) -> EsResult<(bool, Value)> {
    let bad = || EsError::new(400, "parse_exception", format!("could not parse [{ty}] condition"));
    let (path, spec) = body.as_object().and_then(|m| m.iter().next()).ok_or_else(bad)?;
    let mut resolved = Map::new();
    let met = if ty == "compare" {
        let (op, expected) = spec.as_object().and_then(|m| m.iter().next()).ok_or_else(bad)?;
        let actual = resolve(model, path).cloned().unwrap_or(Value::Null);
        let value = compare_value(model, expected, now);
        let met = op_holds(op, &actual, &value);
        resolved.insert(path.clone(), actual);
        if let Value::Str(s) = expected
            && (s.starts_with("{{") || s.starts_with("<{"))
        {
            resolved.insert(s.clone(), value);
        }
        met
    } else {
        let field = spec.get("path").and_then(Value::as_str).unwrap_or("");
        let (op, ospec) = spec.as_object().and_then(|m| m.iter().find(|(k, _)| k.as_str() != "path")).ok_or_else(bad)?;
        let expected = compare_value(model, ospec.get("value").unwrap_or(&Value::Null), now);
        let quantifier = ospec.get("quantifier").and_then(Value::as_str).unwrap_or("some");
        let arr = resolve(model, path).and_then(Value::as_array).cloned().unwrap_or_default();
        let checks: Vec<bool> = arr
            .iter()
            .map(|e| {
                let v = if field.is_empty() { Some(e) } else { resolve(e, field) };
                v.is_some_and(|v| op_holds(op, v, &expected))
            })
            .collect();
        resolved.insert(path.clone(), Value::Arr(arr));
        if quantifier == "all" { checks.iter().all(|c| *c) } else { checks.iter().any(|c| *c) }
    };
    Ok((met, obj! { "resolved_values" => Value::Obj(resolved) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_conditions() {
        let model = obj! { "ctx" => obj! { "payload" => obj! { "n" => 5, "m" => 3 } } };
        let c = obj! { "ctx.payload.n" => obj! { "gt" => 3 } };
        assert!(eval("compare", &c, &model, 0).unwrap().0);
        let c = obj! { "ctx.payload.n" => obj! { "lt" => "{{ctx.payload.m}}" } };
        assert!(!eval("compare", &c, &model, 0).unwrap().0);
        let model = obj! { "ctx" => obj! { "payload" => obj! { "a" => crate::arr![obj! { "v" => 1 }, obj! { "v" => 9 }] } } };
        let c = obj! { "ctx.payload.a" => obj! { "path" => "v", "gte" => obj! { "value" => 5 } } };
        assert!(eval("array_compare", &c, &model, 0).unwrap().0);
    }
}
