//! Filters (docs/code/automations.md, "Filters"): field conditions on a run's context. A field
//! is a dotted path, as in templates (`event.data.change.title`); a missing field is null.
//! Globs use roll code's path patterns (`review/pattern.rs`): `*` and `?` within a segment, `**`
//! only as a whole segment, matched segment by segment (never recursive backtracking).

use crate::review::pattern::Pattern;
use agent_proto::automation::{Filter, FilterOp, limits};
use serde_json::Value;

/// Whether `f` holds on `ctx`.
pub fn holds(f: &Filter, ctx: &Value) -> bool {
    let v = lookup(ctx, &f.field);
    test(f.op, v.unwrap_or(&Value::Null), &f.value) != f.not
}

/// Whether all of them hold.
pub fn all(fs: &[Filter], ctx: &Value) -> bool {
    fs.iter().all(|f| holds(f, ctx))
}

/// Each one's verdict.
pub fn verdicts(fs: &[Filter], ctx: &Value) -> Vec<bool> {
    fs.iter().map(|f| holds(f, ctx)).collect()
}

/// The save-time check: a field path, a value `op` can use, at most [`limits::FILTERS`].
pub fn check(fs: &[Filter], at: &str) -> Result<(), String> {
    if fs.len() > limits::FILTERS {
        return Err(format!("{at}: at most {} filters", limits::FILTERS));
    }
    for (i, f) in fs.iter().enumerate() {
        let at = format!("{at}.{i}");
        let path_ok = !f.field.is_empty()
            && f.field.len() <= 256
            && f.field.split('.').all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        if !path_ok {
            return Err(format!("{at}: field must be a dotted path like event.data.title"));
        }
        let ok = match f.op {
            FilterOp::Equals => !matches!(f.value, Value::Null),
            FilterOp::Contains | FilterOp::HasLabel => f.value.is_string(),
            FilterOp::AnyOf => f.value.as_array().is_some_and(|a| !a.is_empty() && a.len() <= 256),
            FilterOp::Glob => f.value.as_str().and_then(Pattern::new).is_some(),
            FilterOp::Exists => true,
            FilterOp::Gt | FilterOp::Lt => number(&f.value).is_some(),
        };
        if !ok {
            let want = match f.op {
                FilterOp::Equals => "a value",
                FilterOp::Contains | FilterOp::HasLabel => "a string",
                FilterOp::AnyOf => "a list of 1 to 256 values",
                FilterOp::Glob => "a glob",
                FilterOp::Gt | FilterOp::Lt => "a number",
                FilterOp::Exists => "",
            };
            return Err(format!("{at}: {:?} needs {want} as its value", f.op).to_lowercase());
        }
        if serde_json::to_string(&f.value).map_or(0, |s| s.len()) > 16 << 10 {
            return Err(format!("{at}: the value is too large"));
        }
    }
    Ok(())
}

pub(crate) fn lookup<'a>(ctx: &'a Value, path: &str) -> Option<&'a Value> {
    let mut v = ctx;
    for seg in path.split('.') {
        v = match v {
            Value::Object(o) => o.get(seg)?,
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(v)
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Equal as JSON, with a number and its string the same.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(_), Value::String(_)) | (Value::String(_), Value::Number(_)) => number(a).zip(number(b)).is_some_and(|(x, y)| x == y),
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

fn label(v: &Value) -> Option<&str> {
    v.as_str().or_else(|| v.get("name").and_then(Value::as_str))
}

fn test(op: FilterOp, v: &Value, want: &Value) -> bool {
    match op {
        FilterOp::Equals => same(v, want),
        FilterOp::Contains => match (v, want.as_str()) {
            (Value::String(s), Some(w)) => s.to_lowercase().contains(&w.to_lowercase()),
            (Value::Array(a), _) => a.iter().any(|x| same(x, want) || label(x).zip(want.as_str()).is_some_and(|(l, w)| l.eq_ignore_ascii_case(w))),
            _ => false,
        },
        FilterOp::AnyOf => want.as_array().is_some_and(|w| w.iter().any(|x| same(v, x))),
        FilterOp::HasLabel => match (v, want.as_str()) {
            (Value::Array(a), Some(w)) => a.iter().filter_map(label).any(|l| l.eq_ignore_ascii_case(w)),
            _ => false,
        },
        FilterOp::Glob => match (v.as_str(), want.as_str().and_then(Pattern::new)) {
            (Some(s), Some(p)) => p.matches(s),
            _ => false,
        },
        FilterOp::Exists => !v.is_null(),
        FilterOp::Gt => number(v).zip(number(want)).is_some_and(|(x, y)| x > y),
        FilterOp::Lt => number(v).zip(number(want)).is_some_and(|(x, y)| x < y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn f(field: &str, op: FilterOp, value: Value) -> Filter {
        Filter { field: field.into(), op, value, not: false }
    }

    fn ctx() -> Value {
        json!({ "event": { "data": {
            "repo": "acme/app", "number": 12, "title": "Fix the Flaky queue",
            "labels": [{ "name": "Bug" }, "p1"], "path": "src/queue/mod.rs", "draft": false,
        } } })
    }

    #[test]
    fn every_op() {
        let c = ctx();
        let yes = [
            f("event.data.repo", FilterOp::Equals, json!("acme/app")),
            f("event.data.number", FilterOp::Equals, json!("12")),
            f("event.data.draft", FilterOp::Equals, json!(false)),
            f("event.data.title", FilterOp::Contains, json!("flaky")),
            f("event.data.labels", FilterOp::Contains, json!("p1")),
            f("event.data.repo", FilterOp::AnyOf, json!(["acme/web", "acme/app"])),
            f("event.data.labels", FilterOp::HasLabel, json!("bug")),
            f("event.data.path", FilterOp::Glob, json!("src/**/*.rs")),
            f("event.data.path", FilterOp::Glob, json!("*.rs")),
            f("event.data.title", FilterOp::Exists, json!(null)),
            f("event.data.number", FilterOp::Gt, json!(10)),
            f("event.data.number", FilterOp::Lt, json!("13")),
        ];
        for x in &yes {
            assert!(holds(x, &c), "{x:?}");
            assert!(!holds(&Filter { not: true, ..x.clone() }, &c), "not {x:?}");
        }
        let no = [
            f("event.data.repo", FilterOp::Equals, json!("acme/web")),
            f("event.data.missing", FilterOp::Exists, json!(null)),
            f("event.data.labels", FilterOp::HasLabel, json!("p2")),
            f("event.data.path", FilterOp::Glob, json!("docs/**")),
            f("event.data.path", FilterOp::Glob, json!("src/*.rs")),
            f("event.data.title", FilterOp::Gt, json!(1)),
            f("event.data.number", FilterOp::Contains, json!("1")),
        ];
        for x in &no {
            assert!(!holds(x, &c), "{x:?}");
        }
        assert!(all(&yes, &c));
        assert_eq!(verdicts(&[yes[0].clone(), no[0].clone()], &c), vec![true, false]);
    }

    #[test]
    fn checks() {
        assert!(check(&[f("event.data.x", FilterOp::Equals, json!(1))], "filters").is_ok());
        for bad in [
            f("event..x", FilterOp::Exists, json!(null)),
            f("event.da ta", FilterOp::Exists, json!(null)),
            f("event.x", FilterOp::Equals, json!(null)),
            f("event.x", FilterOp::AnyOf, json!([])),
            f("event.x", FilterOp::Gt, json!("ten")),
            f("event.x", FilterOp::Glob, json!("")),
            f("event.x", FilterOp::HasLabel, json!(1)),
        ] {
            assert!(check(std::slice::from_ref(&bad), "filters").is_err(), "{bad:?}");
        }
    }
}
