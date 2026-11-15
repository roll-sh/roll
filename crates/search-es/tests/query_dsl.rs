//! Query DSL parsing against Elasticsearch 9.5.5: every case in `tests/fixtures/query/*.json`
//! was sent to a real Elasticsearch as `{"query": …}`. Valid queries must print back exactly as
//! Elasticsearch printed them (an alias filter shown by `_search_shards`), invalid ones must fail
//! with the same exception (type, reason, location, causes), and queries Elasticsearch only
//! rejected against the index's mappings must parse.

use search_es::json::locate::Seg;
use search_es::json::{self, Value};
use search_es::query;

fn strip(v: &Value) -> Value {
    let mut out = json::Map::new();
    if let Value::Obj(m) = v {
        for k in ["type", "reason", "line", "col"] {
            if let Some(x) = m.get(k) {
                out.insert(k.to_string(), x.clone());
            }
        }
        if let Some(c) = m.get("caused_by") {
            out.insert("caused_by".into(), strip(c));
        }
    }
    Value::Obj(out)
}

/// `SearchSourceBuilder`'s check of the `query` value's token.
fn source_query_error(q: &Value, body: &[u8]) -> Value {
    let tok = match q {
        Value::Arr(_) => "START_ARRAY",
        Value::Str(_) => "VALUE_STRING",
        Value::Num(_) => "VALUE_NUMBER",
        Value::Bool(_) => "VALUE_BOOLEAN",
        _ => "VALUE_NULL",
    };
    let (l, c) = json::locate::locate(body, &[Seg::Key("query")], json::locate::Target::Value).unwrap_or((1, 1));
    search_es::obj! { "type" => "parsing_exception", "reason" => format!("Unknown key for a {tok} in [query]."), "line" => l, "col" => c }
}

#[test]
fn elasticsearch_query_fixtures() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/query");
    let mut failures = Vec::new();
    let mut total = 0;
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    for path in files {
        let raw = std::fs::read(&path).unwrap();
        let doc = json::parse(&raw).unwrap().unwrap();
        for case in doc.get("cases").and_then(Value::as_array).unwrap() {
            total += 1;
            let src = case.get("query").and_then(Value::as_str).unwrap();
            let body = format!("{{\"query\":{src}}}");
            let v = json::parse(body.as_bytes()).unwrap().unwrap();
            let q = v.get("query").unwrap();
            let got: Result<(query::Query, Vec<String>), Value> = if matches!(q, Value::Obj(_)) {
                query::parse_query(q, Some(body.as_bytes()), &[Seg::Key("query")]).map_err(|e| strip(&e.to_value(false)))
            } else {
                Err(source_query_error(q, body.as_bytes()))
            };
            let ok = match (&got, case.get("expect"), case.get("error"), case.get("parses")) {
                (Ok((q, _)), Some(expect), _, _) => &q.to_value() == expect,
                (Err(e), _, Some(expect), _) => e == expect,
                (Ok(_), _, _, Some(_)) => true,
                _ => false,
            };
            let warnings_ok = match (case.get("warnings"), &got) {
                (Some(Value::Arr(w)), Ok((_, got_w))) => {
                    let want: Vec<String> = w.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
                    &want == got_w
                }
                _ => true,
            };
            if !ok || !warnings_ok {
                let shown = match &got {
                    Ok((q, w)) => format!("OK {} warnings {w:?}", q.to_value().to_json()),
                    Err(e) => format!("ERR {}", e.to_json()),
                };
                let want = case.get("expect").or(case.get("error")).map(Value::to_json).unwrap_or_else(|| "parses".into());
                failures.push(format!("{src}\n   want {want}\n   got  {shown}"));
            }
        }
    }
    if !failures.is_empty() {
        panic!("{} of {total} query fixtures differ:\n{}", failures.len(), failures.join("\n"));
    }
}

/// Search request bodies (`tests/fixtures/search_source.json`): parse errors must match
/// Elasticsearch's; bodies it accepted (or rejected only when searching) must parse. Sections
/// parsed elsewhere (aggregations, highlight, suggest, runtime mappings) are `deferred`.
#[test]
fn elasticsearch_search_source_fixtures() {
    let raw = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/search_source.json")).unwrap();
    let doc = json::parse(&raw).unwrap().unwrap();
    let mut failures = Vec::new();
    for case in doc.get("cases").and_then(Value::as_array).unwrap() {
        if case.get("deferred").is_some() {
            continue;
        }
        let src = case.get("body").and_then(Value::as_str).unwrap();
        let Ok(Some(v)) = json::parse(src.as_bytes()) else {
            // Duplicate keys fail in the JSON parser itself (covered by tests/json_errors.rs).
            continue;
        };
        let got = search_es::search_source::SearchSource::parse(&v, Some(src.as_bytes())).map_err(|e| strip(&e.to_value(false)));
        let ok = match (&got, case.get("error"), case.get("parses")) {
            (Err(e), Some(want), _) => e == want,
            (Ok(_), _, Some(_)) => true,
            _ => false,
        };
        if !ok {
            let shown = match &got {
                Ok(_) => "OK".to_string(),
                Err(e) => format!("ERR {}", e.to_json()),
            };
            let want = case.get("error").map(Value::to_json).unwrap_or_else(|| "parses".into());
            failures.push(format!("{src}\n   want {want}\n   got  {shown}"));
        }
    }
    if !failures.is_empty() {
        panic!("{} search source fixtures differ:\n{}", failures.len(), failures.join("\n"));
    }
}
