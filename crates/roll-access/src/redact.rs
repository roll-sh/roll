//! What the audit log and approvals keep of a request: secrets replaced, long strings cut
//! (docs/permissions.md, "Audit").

use crate::catalog::Row;
use roll_auth::scope::Action;
use serde_json::{Map, Value};

pub const REDACTED: &str = "[redacted]";
/// Strings kept at most, in bytes.
const MAX_STRING: usize = 2048;
/// Nesting kept at most.
const MAX_DEPTH: usize = 8;
/// Array items and object fields kept at most.
const MAX_ITEMS: usize = 100;

/// A field name that holds a secret: a password, secret, token, key, credential or value.
pub fn secret_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase().replace(['-', '_'], "");
    ["password", "passwd", "secret", "token", "apikey", "accesskey", "privatekey", "credential", "authorization", "cookie", "value", "values", "contentb64", "seal", "dsn", "connectionstring", "uri"]
        .iter()
        .any(|s| n.contains(s))
        || n == "key"
        || n.ends_with("key") && !matches!(n.as_str(), "partitionkey" | "sortkey" | "routingkey" | "objectkey" | "primarykey")
}

/// `v` with secrets replaced and long strings cut.
pub fn value(v: &Value) -> Value {
    walk(v, 0)
}

fn walk(v: &Value, depth: usize) -> Value {
    if depth > MAX_DEPTH {
        return Value::String("…".into());
    }
    match v {
        Value::String(s) if s.len() > MAX_STRING => {
            let mut end = MAX_STRING;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            Value::String(format!("{}… ({} bytes)", &s[..end], s.len()))
        }
        Value::Array(a) => Value::Array(a.iter().take(MAX_ITEMS).map(|x| walk(x, depth + 1)).collect()),
        Value::Object(o) => {
            let mut out = Map::new();
            for (k, x) in o.iter().take(MAX_ITEMS) {
                let kept = if secret_name(k) && !x.is_null() { Value::String(REDACTED.into()) } else { walk(x, depth + 1) };
                out.insert(k.clone(), kept);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// A request body as kept: JSON redacted, anything else only its size.
pub fn body(_row: &Row, body: &[u8]) -> Value {
    if body.is_empty() {
        return Value::Null;
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(v) => value(&v),
        Err(_) => serde_json::json!({ "bytes": body.len() }),
    }
}

/// One line for the person deciding: "delete postgres prj_x (DELETE /v1/projects/{id}, tool postgres_delete_project)".
pub fn summary(row: &Row, action: Action, method: &str, resource: Option<&str>, tool: Option<&str>) -> String {
    let what = resource.map_or_else(|| row.product.to_string(), |r| format!("{} {r}", row.product));
    let via = tool.map(|t| format!(", tool {t}")).unwrap_or_default();
    format!("{action} {what} ({method} {}{via})", row.route)
}

/// A credential hidden: a URL keeps everything but its password (`postgres://roll:[redacted]@host`).
fn hide(x: &Value) -> Value {
    match x {
        Value::String(s) => match url::Url::parse(s) {
            Ok(mut u) if u.password().is_some() => match u.set_password(Some("redacted")) {
                Ok(()) => Value::String(u.to_string()),
                Err(()) => Value::String(REDACTED.into()),
            },
            _ => Value::String(REDACTED.into()),
        },
        Value::Array(a) => Value::Array(a.iter().map(hide).collect()),
        Value::Null => Value::Null,
        _ => Value::String(REDACTED.into()),
    }
}

/// Replace the named fields wherever they are in an answer (credentials a grant may not see).
pub fn fields(v: &mut Value, names: &[&str]) {
    match v {
        Value::Object(o) => {
            for (k, x) in o.iter_mut() {
                if names.contains(&k.as_str()) && !x.is_null() {
                    *x = hide(x);
                } else {
                    fields(x, names);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| fields(x, names)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Secrets go, ids stay, long strings are cut.
    #[test]
    fn keeps_what_matters() {
        let v = value(&serde_json::json!({
            "name": "db", "password": "hunter2", "api_key": "k", "routing_key": "rk", "nested": [{ "token": "t", "sql": "x".repeat(5000) }]
        }));
        assert_eq!(v["name"], "db");
        assert_eq!(v["password"], REDACTED);
        assert_eq!(v["api_key"], REDACTED);
        assert_eq!(v["routing_key"], "rk");
        assert_eq!(v["nested"][0]["token"], REDACTED);
        assert!(v["nested"][0]["sql"].as_str().unwrap().len() < 2200);
        let mut a = serde_json::json!({ "id": "c1", "password": "p", "branches": [{ "password": "q", "connection_uri": "postgres://roll:pw@h:5432/main" }] });
        fields(&mut a, &["password", "connection_uri"]);
        assert_eq!(
            a,
            serde_json::json!({ "id": "c1", "password": REDACTED, "branches": [{ "password": REDACTED, "connection_uri": "postgres://roll:redacted@h:5432/main" }] })
        );
    }
}
