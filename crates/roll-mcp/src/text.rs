//! A JSON answer as the text block a model reads (docs/mcp.md, "Tool answers"): compact JSON, or,
//! for a list of objects, a tab-separated table that names each column once instead of on every
//! row. `structuredContent` always keeps the exact JSON; the text is whichever is shorter.

use serde_json::{Map, Value};

/// Rows a table needs before it's worth a header.
const MIN_ROWS: usize = 2;
/// Columns a table may have; past that a row reads better as JSON.
const MAX_COLUMNS: usize = 40;

/// The text for answer `v`.
pub fn render(v: &Value) -> String {
    let json = serde_json::to_string(v).unwrap_or_default();
    match table(v) {
        Some(t) if t.len() < json.len() => t,
        _ => json,
    }
}

/// `v` as a table: a list of objects, or an object whose one list of objects comes after its
/// other fields (`key: value` lines).
fn table(v: &Value) -> Option<String> {
    if let Some(rows) = v.as_array() {
        return rows_table(rows).map(|t| format!("{} rows, tab-separated:\n{t}", rows.len()));
    }
    let o = v.as_object()?;
    let mut lists = o.iter().filter(|(_, v)| v.as_array().is_some_and(|a| a.len() >= MIN_ROWS && a.iter().all(Value::is_object)));
    let (key, rows) = lists.next()?;
    if lists.next().is_some() {
        return None;
    }
    let rows = rows.as_array()?;
    let t = rows_table(rows)?;
    let mut out = String::new();
    for (k, v) in o.iter().filter(|(k, _)| *k != key) {
        out.push_str(k);
        out.push_str(": ");
        out.push_str(&serde_json::to_string(v).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("{key}: {} rows, tab-separated:\n{t}", rows.len()));
    Some(out)
}

fn rows_table(rows: &[Value]) -> Option<String> {
    if rows.len() < MIN_ROWS || !rows.iter().all(Value::is_object) {
        return None;
    }
    // Columns as first seen (a row's fields come in key order).
    let mut columns: Vec<&str> = Vec::new();
    for r in rows.iter().filter_map(Value::as_object) {
        for k in r.keys() {
            if !columns.contains(&k.as_str()) {
                columns.push(k);
                if columns.len() > MAX_COLUMNS {
                    return None;
                }
            }
        }
    }
    let mut out = columns.iter().map(|c| cell_str(c)).collect::<Vec<_>>().join("\t");
    for r in rows.iter().filter_map(Value::as_object) {
        out.push('\n');
        out.push_str(&columns.iter().map(|c| cell(r, c)).collect::<Vec<_>>().join("\t"));
    }
    Some(out)
}

/// A row's value for a column: a plain string as itself, a number or boolean as JSON, `null` and a
/// missing field as nothing, and anything else (an empty or quoted string, one with a tab or a line
/// break, an object, a list) as compact JSON, so a cell never spills into the next.
fn cell(row: &Map<String, Value>, column: &str) -> String {
    match row.get(column) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => cell_str(s),
        Some(v) => serde_json::to_string(v).unwrap_or_default(),
    }
}

fn cell_str(s: &str) -> String {
    if s.is_empty() || s.starts_with('"') || s.contains(['\t', '\n', '\r']) {
        serde_json::to_string(s).unwrap_or_default()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_list_of_objects_names_its_columns_once() {
        let v = json!({ "next": "c2", "items": [
            { "id": "svc_1", "name": "web", "replicas": 2, "paused": false, "tags": ["a"] },
            { "id": "svc_2", "name": "tab\there", "replicas": null, "paused": true, "note": "" },
            { "id": "svc_3", "name": "\"quoted\"", "replicas": 1, "paused": false },
        ] });
        let t = render(&v);
        assert_eq!(
            t,
            "next: \"c2\"\nitems: 3 rows, tab-separated:\nid\tname\tpaused\treplicas\ttags\tnote\nsvc_1\tweb\tfalse\t2\t[\"a\"]\t\nsvc_2\t\"tab\\there\"\ttrue\t\t\t\"\"\nsvc_3\t\"\\\"quoted\\\"\"\tfalse\t1\t\t"
        );
        assert!(t.len() < serde_json::to_string(&v).unwrap().len());
        // One row, two lists, or anything that isn't shorter stays JSON.
        for v in [json!([{ "id": "a" }]), json!({ "a": [{ "x": 1 }, { "x": 2 }], "b": [{ "y": 1 }, { "y": 2 }] }), json!({ "id": "x" }), json!([1, 2, 3])] {
            assert_eq!(render(&v), serde_json::to_string(&v).unwrap(), "{v}");
        }
    }
}
