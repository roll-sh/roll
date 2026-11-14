//! A service's API, read from its OpenAPI document by the screenshot session (`github.openapi`,
//! `shot.js`), and what a change does to it against main's (docs/previews.md, "Surfaces"): the
//! operations it adds and removes (Optic's and Bump's API diff), and what breaks inside an
//! operation both keep (oasdiff's breaking-change rules).
//!
//! The session writes one line per fact, all of them the change's own document's, so every line
//! is checked for its shape here and anything else is dropped:
//!
//! - `GET /users/{id}`: an operation;
//! - `GET /users/{id} param query:limit type integer`, `… param query:limit required`,
//!   `… param query:sort enum "asc"`: a parameter's (`in:name`) type, required, allowed values;
//! - `POST /orders body total type number`, `… body total required`, `… body status enum "new"`:
//!   a request body field's (by path: `items[].sku`);
//! - `GET /users/{id} response 200 email type string`: a 2xx response's field.

use std::collections::{BTreeMap, BTreeSet};

/// The most lines kept from one document.
pub const MAX_LINES: usize = 5000;
/// The most breaking changes named.
pub const MAX_BREAKING: usize = 30;

const METHODS: [&str; 8] = ["GET", "PUT", "POST", "DELETE", "PATCH", "OPTIONS", "HEAD", "TRACE"];

/// What a screenshot run wrote (`{"api": [lines]}`) as the API's lines: each of a shape above,
/// printable, at most 300 characters, sorted, each once, at most [`MAX_LINES`]. `None`: no
/// document was read.
pub fn lines(json: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(json.trim()).ok()?;
    let mut out: Vec<String> = v
        .get("api")?
        .as_array()?
        .iter()
        .filter_map(|l| l.as_str())
        .filter(|l| l.chars().count() <= 300 && !l.chars().any(char::is_control) && fact(l).is_some())
        .take(MAX_LINES)
        .map(str::to_string)
        .collect();
    out.sort();
    out.dedup();
    Some(out)
}

/// The operations among the API's lines (`GET /users`), a line each: what's compared for the
/// operations added and removed.
pub fn operations(lines: &str) -> String {
    lines.lines().filter(|l| matches!(fact(l), Some(Fact { what: What::Operation, .. }))).collect::<Vec<_>>().join("\n")
}

#[derive(Debug, PartialEq)]
enum What<'a> {
    Operation,
    /// `param`/`body` with its name, or `response` with its status and field.
    Field { kind: &'a str, name: String },
}

#[derive(Debug, PartialEq)]
struct Fact<'a> {
    op: String,
    what: What<'a>,
    /// `type <t>`, `required`, `enum <v>`; empty for an operation.
    attr: &'a str,
}

/// One line read as a fact, if it has a shape above.
fn fact(l: &str) -> Option<Fact<'_>> {
    let mut parts = l.splitn(3, ' ');
    let (method, path) = (parts.next()?, parts.next()?);
    if !METHODS.contains(&method) || !path.starts_with('/') {
        return None;
    }
    let op = format!("{method} {path}");
    let Some(rest) = parts.next() else { return Some(Fact { op, what: What::Operation, attr: "" }) };
    let (kind, rest) = rest.split_once(' ')?;
    let (name, attr) = match kind {
        "param" | "body" => {
            let (name, attr) = rest.split_once(' ')?;
            (name.to_string(), attr)
        }
        "response" => {
            let (code, rest) = rest.split_once(' ')?;
            let (field, attr) = rest.split_once(' ')?;
            if !(code.len() == 3 && code.starts_with('2') && code.bytes().all(|b| b.is_ascii_digit()) || code == "2XX") {
                return None;
            }
            (format!("{code} {field}"), attr)
        }
        _ => return None,
    };
    let attr_ok = attr == "required" || attr.strip_prefix("type ").is_some_and(|t| !t.is_empty()) || attr.strip_prefix("enum ").is_some_and(|v| !v.is_empty());
    (attr_ok && !name.is_empty()).then_some(Fact { op, what: What::Field { kind, name }, attr })
}

/// One parameter's, body field's or response field's facts.
#[derive(Default)]
struct Shape {
    ty: Option<String>,
    required: bool,
    values: BTreeSet<String>,
}

type Shapes = BTreeMap<(String, String, String), Shape>;

/// The operations, and `(operation, kind, name)` → its shape.
fn shapes(lines: &str) -> (BTreeSet<String>, Shapes) {
    let (mut ops, mut out) = (BTreeSet::new(), Shapes::new());
    for f in lines.lines().filter_map(fact) {
        match f.what {
            What::Operation => {
                ops.insert(f.op);
            }
            What::Field { kind, name } => {
                let s = out.entry((f.op, kind.to_string(), name)).or_default();
                if f.attr == "required" {
                    s.required = true;
                } else if let Some(t) = f.attr.strip_prefix("type ") {
                    s.ty = Some(t.to_string());
                } else if let Some(v) = f.attr.strip_prefix("enum ") {
                    s.values.insert(v.to_string());
                }
            }
        }
    }
    (ops, out)
}

/// What the change breaks inside the operations main's API and its own both have (the removed
/// ones are the operations diff's): a parameter or body field that becomes required (or a new
/// required one), one whose type changes, a value one no longer takes, a response field that
/// goes or whose type changes. A sentence each, by operation, at most [`MAX_BREAKING`].
pub fn breaking(main: &str, change: &str) -> Vec<String> {
    let ((main_ops, main), (change_ops, change)) = (shapes(main), shapes(change));
    let kept: BTreeSet<&String> = main_ops.intersection(&change_ops).collect();
    let what = |kind: &str, name: &str| match kind {
        "param" => format!("parameter {name}"),
        "body" => format!("body field {name}"),
        _ => {
            let (code, field) = name.split_once(' ').unwrap_or((name, ""));
            format!("response field {field} ({code})")
        }
    };
    let mut out = Vec::new();
    for ((op, kind, name), now) in &change {
        if !kept.contains(op) || kind == "response" {
            continue;
        }
        match main.get(&(op.clone(), kind.clone(), name.clone())) {
            None if now.required => out.push(format!("{op}: requires a new {}", what(kind, name))),
            Some(before) if now.required && !before.required => out.push(format!("{op}: the {} becomes required", what(kind, name))),
            _ => {}
        }
    }
    for ((op, kind, name), before) in &main {
        if !kept.contains(op) {
            continue;
        }
        let Some(now) = change.get(&(op.clone(), kind.clone(), name.clone())) else {
            if kind == "response" {
                out.push(format!("{op}: the {} goes", what(kind, name)));
            }
            continue;
        };
        if let (Some(a), Some(b)) = (&before.ty, &now.ty)
            && a != b
        {
            out.push(format!("{op}: the {} changes from {a} to {b}", what(kind, name)));
        }
        // A request value it no longer takes (a response's fewer values break nobody).
        if kind != "response" && !now.values.is_empty() {
            let gone: Vec<&str> = before.values.difference(&now.values).take(5).map(String::as_str).collect();
            if !gone.is_empty() {
                out.push(format!("{op}: the {} no longer takes {}", what(kind, name), gone.join(", ")));
            }
        }
    }
    out.truncate(MAX_BREAKING);
    out
}

#[cfg(test)]
mod tests {
    /// A service's OpenAPI lines read back as facts of the shapes the session writes, sorted,
    /// each once; anything else its document could hold (another method, no path, a newline to
    /// fake a line, a 4xx response, a word that isn't a fact) doesn't.
    #[test]
    fn only_facts_of_their_shapes_read_back() {
        let got = super::lines(
            r#"{"api": ["POST /orders", "GET /users", "GET /users", "FETCH /x", "GET users", "GET /a\nDELETE /b", 7,
               "GET /users param query:limit type integer", "GET /users response 200 email type string",
               "GET /users response 404 error type string", "GET /users param query:limit maybe", "GET /users cookie x type y"]}"#,
        )
        .unwrap();
        assert_eq!(got, ["GET /users", "GET /users param query:limit type integer", "GET /users response 200 email type string", "POST /orders"]);
        assert_eq!(super::operations(&got.join("\n")), "GET /users\nPOST /orders");
        assert_eq!(super::lines(r#"{"seo": {}}"#), None, "no document read");
    }

    /// What breaks inside an operation both keep: a parameter that becomes required and a new
    /// required body field, a type that changes, a value no longer taken, a response field that
    /// goes. An optional new field, a new response field and anything in an operation the change
    /// removes aren't.
    #[test]
    fn breaking_changes_inside_an_operation_are_named() {
        let main = [
            "GET /users",
            "GET /users param query:limit type integer",
            "GET /users response 200 email type string",
            "GET /users response 200 id type integer",
            "POST /orders",
            "POST /orders body total type number",
            "POST /orders body status enum \"new\"",
            "POST /orders body status enum \"paid\"",
            "POST /orders body status type string",
            "DELETE /orders/{id}",
            "DELETE /orders/{id} param path:id required",
        ]
        .join("\n");
        let change = [
            "GET /users",
            "GET /users param query:limit required",
            "GET /users param query:limit type integer",
            "GET /users response 200 id type string",
            "GET /users response 200 nick type string",
            "POST /orders",
            "POST /orders body total type string",
            "POST /orders body status enum \"new\"",
            "POST /orders body status type string",
            "POST /orders body coupon type string",
            "POST /orders body customer required",
            "POST /orders body customer type string",
        ]
        .join("\n");
        assert_eq!(
            super::breaking(&main, &change),
            [
                "GET /users: the parameter query:limit becomes required",
                "POST /orders: requires a new body field customer",
                "GET /users: the response field email (200) goes",
                "GET /users: the response field id (200) changes from integer to string",
                "POST /orders: the body field status no longer takes \"paid\"",
                "POST /orders: the body field total changes from number to string",
            ]
        );
    }
}
