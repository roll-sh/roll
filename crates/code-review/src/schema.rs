//! The JSON schemas answers must follow. Structured outputs accept a subset of JSON Schema:
//! every object has `additionalProperties: false` and lists every key in `required`; no
//! numeric or length constraints, no recursion. [`check`] enforces that, so a schema change
//! that the API would reject fails a unit test instead of every review.

use serde_json::{Value, json};

pub const SEVERITIES: [&str; 5] = ["bug", "security", "perf", "style", "nit"];

fn nullable_string() -> Value {
    json!({ "anyOf": [{ "type": "string" }, { "type": "null" }] })
}

/// A review of one group of files.
pub fn review() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["walkthrough", "files", "diagram", "findings", "notes"],
        "properties": {
            "walkthrough": { "type": "string" },
            "files": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path", "summary"],
                    "properties": { "path": { "type": "string" }, "summary": { "type": "string" } }
                }
            },
            "diagram": nullable_string(),
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path", "side", "start_line", "line", "severity", "title", "body", "suggestion", "confidence"],
                    "properties": {
                        "path": { "type": "string" },
                        "side": { "type": "string", "enum": ["new", "old"] },
                        "start_line": { "type": "integer" },
                        "line": { "type": "integer" },
                        "severity": { "type": "string", "enum": SEVERITIES },
                        "title": { "type": "string" },
                        "body": { "type": "string" },
                        "suggestion": nullable_string(),
                        "confidence": { "type": "number" }
                    }
                }
            },
            "notes": { "type": "array", "items": { "type": "string" } }
        }
    })
}

/// The `auto` classifier's answer.
pub fn classify() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["mode", "reasons"],
        "properties": {
            "mode": { "type": "string", "enum": ["standard", "deep", "max"] },
            "reasons": { "type": "array", "items": { "type": "string" } }
        }
    })
}

/// The verification pass: which findings hold up.
pub fn verify() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["results"],
        "properties": {
            "results": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["index", "confirmed", "reason"],
                    "properties": {
                        "index": { "type": "integer" },
                        "confirmed": { "type": "boolean" },
                        "reason": { "type": "string" }
                    }
                }
            }
        }
    })
}

/// A reply in a thread (`@roll explain`, a question, `@roll fix`).
pub fn reply() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["reply", "suggestion"],
        "properties": { "reply": { "type": "string" }, "suggestion": nullable_string() }
    })
}

/// Problems with `schema` that structured outputs would reject (empty: it's fine).
pub fn check(schema: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    walk(schema, "$", &mut errs);
    errs
}

fn walk(v: &Value, at: &str, errs: &mut Vec<String>) {
    let Some(o) = v.as_object() else { return };
    for k in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum", "multipleOf", "minLength", "maxLength", "minItems", "maxItems", "$ref", "pattern"] {
        if o.contains_key(k) {
            errs.push(format!("{at}: `{k}` isn't supported"));
        }
    }
    if o.get("type").and_then(Value::as_str) == Some("object") {
        if o.get("additionalProperties") != Some(&Value::Bool(false)) {
            errs.push(format!("{at}: objects need additionalProperties: false"));
        }
        let props: Vec<&String> = o.get("properties").and_then(Value::as_object).map(|p| p.keys().collect()).unwrap_or_default();
        let req: Vec<&str> = o.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        for p in &props {
            if !req.contains(&p.as_str()) {
                errs.push(format!("{at}.{p}: every property must be required"));
            }
        }
        for r in &req {
            if !props.iter().any(|p| p == r) {
                errs.push(format!("{at}: required `{r}` isn't a property"));
            }
        }
        if let Some(p) = o.get("properties").and_then(Value::as_object) {
            for (k, s) in p {
                walk(s, &format!("{at}.{k}"), errs);
            }
        }
    }
    if let Some(items) = o.get("items") {
        walk(items, &format!("{at}[]"), errs);
    }
    for k in ["anyOf", "allOf"] {
        if let Some(a) = o.get(k).and_then(Value::as_array) {
            for (i, s) in a.iter().enumerate() {
                walk(s, &format!("{at}.{k}[{i}]"), errs);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_schema_is_accepted_by_structured_outputs() {
        for (name, s) in [("review", review()), ("classify", classify()), ("verify", verify()), ("reply", reply())] {
            assert!(check(&s).is_empty(), "{name}: {:?}", check(&s));
        }
        let bad = json!({"type": "object", "properties": {"a": {"type": "string", "minLength": 1}}, "required": []});
        let e = check(&bad);
        assert_eq!(e.len(), 3, "{e:?}");
    }
}
