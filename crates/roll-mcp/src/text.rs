//! A JSON answer as the text block a model reads (docs/mcp.md, "Tool answers"): compact JSON, or,
//! for a list of objects, a tab-separated table that names each column once instead of on every
//! row. `structuredContent` always keeps the exact JSON; the text is whichever is shorter.

use roll_auth::lockdown::{MAX_TEXT, cut, invisible, visible};
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

/// The text cut to fit a client's limit ([`cut`]'s): a table at the end of its last whole row, so
/// no row's last value looks whole when it isn't (`svc_123` cut to `svc_12`). `takes` is what the
/// tool can narrow its answer with ([`crate::tools::Tool::narrowing`]), so the note names those
/// arguments, or says there are none, instead of sending the model to look for a page cursor.
pub fn fit(text: String, takes: Option<&[&str]>) -> String {
    let shown = visible(&text);
    if shown.len() <= MAX_TEXT {
        return shown;
    }
    // Compact JSON has no line breaks of its own (they're escaped): only a table's rows end in one.
    match shown.as_bytes()[..MAX_TEXT].iter().rposition(|b| *b == b'\n').filter(|i| *i > MAX_TEXT / 2) {
        Some(end) => {
            let less = match takes {
                None => "ask for less, as most list tools take a limit, a filter or a page cursor".to_string(),
                Some([]) => "this tool takes no filter or page cursor".to_string(),
                Some(args) => format!("ask for less with this tool's {}", args.join(", ")),
            };
            format!("{}\n… (cut at {} KiB of {} bytes, after a whole row: {less}; the structured content has all of it)", &shown[..end], MAX_TEXT >> 10, shown.len())
        }
        None => cut(text, true),
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
        out.push_str(&cell_str(k));
        out.push_str(": ");
        out.push_str(&serde_json::to_string(v).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("{}: {} rows, tab-separated:\n{t}", cell_str(key), rows.len()));
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
/// missing field as nothing, and anything else (an empty or quoted string, one with any control
/// character, a line or paragraph separator, or a character a person can't see; an object, a list)
/// as JSON, so a cell never spills into the next, nor passes for another row.
fn cell(row: &Map<String, Value>, column: &str) -> String {
    match row.get(column) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => cell_str(s),
        Some(v) => serde_json::to_string(v).unwrap_or_default(),
    }
}

fn cell_str(s: &str) -> String {
    let odd = |c: char| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') || invisible(c);
    if s.is_empty() || s.starts_with('"') || s.chars().any(odd) {
        // JSON escapes C0 controls; the separators and NEL it leaves as they are, so they're done
        // here (what a person can't see, `visible` escapes later inside the quotes).
        serde_json::to_string(s).unwrap_or_default().replace('\u{2028}', "\\u2028").replace('\u{2029}', "\\u2029").replace('\u{85}', "\\u0085")
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
        // A value that could pass for a row or hide in one is quoted: line and paragraph separators,
        // other controls, what a person can't see; so are keys.
        let tricky = json!([{ "t": "a\u{2028}pv_9\tready", "u": "x\u{b}y", "w": "z\u{200b}" }, { "t": "b", "u": "c", "w": "d" }]);
        let t = render(&tricky);
        assert!(t.contains("\"a\\u2028pv_9\\tready\"") && t.contains("\"x\\u000by\"") && t.contains("\"z\u{200b}\""), "{t}");
        assert_eq!(t.lines().count(), 4, "a header line, the column names and two rows: {t}");
        let rows: Vec<Value> = (0..10).map(|i| json!({ "x": i })).collect();
        assert!(render(&json!({ "a\nb": 1, "items": rows })).starts_with("\"a\\nb\": 1\n"));
        // A long table is cut after a whole row.
        let rows: Vec<Value> = (0..20_000).map(|i| json!({ "id": format!("svc_{i:06}"), "n": i })).collect();
        let long = fit(render(&json!(rows)), Some(&["project", "limit"]));
        let last = long.lines().rev().nth(1).unwrap();
        assert!(long.len() < MAX_TEXT + 300 && last.starts_with("svc_") && last.split('\t').next().unwrap().len() == 10, "{last}");
        // The note names what this tool narrows with, or that it has nothing to narrow with.
        assert!(long.ends_with("ask for less with this tool's project, limit; the structured content has all of it)"), "{}", &long[long.len() - 200..]);
        assert!(fit(render(&json!(rows)), Some(&[])).contains("this tool takes no filter or page cursor"));
        // One row, two lists, or anything that isn't shorter stays JSON.
        for v in [json!([{ "id": "a" }]), json!({ "a": [{ "x": 1 }, { "x": 2 }], "b": [{ "y": 1 }, { "y": 2 }] }), json!({ "id": "x" }), json!([1, 2, 3])] {
            assert_eq!(render(&v), serde_json::to_string(&v).unwrap(), "{v}");
        }
    }
}
