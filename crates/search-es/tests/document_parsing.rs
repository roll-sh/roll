//! Document parsing against Elasticsearch 9.5.5: each case in `tests/fixtures/document/*.json`
//! was indexed into an index with the fixture's mapping. Rejected documents must fail with the
//! same exception (type, reason with its `[line:col]`, causes); accepted ones must record the same
//! `_ignored` fields.

use search_es::json::{self, Value};
use search_es::mapping::{Mapping, MappingCtx};

fn strip(v: &Value) -> Value {
    let mut out = json::Map::new();
    if let Value::Obj(m) = v {
        for k in ["type", "reason"] {
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

#[test]
fn elasticsearch_document_fixtures() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/document");
    let mut failures = Vec::new();
    let mut total = 0;
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    for path in files {
        let raw = std::fs::read(&path).unwrap();
        let fx = json::parse(&raw).unwrap().unwrap();
        let ctx = MappingCtx::from_settings(&Default::default());
        let mapping = Mapping::parse(fx.get("mapping").and_then(Value::as_object).unwrap(), &ctx).unwrap();
        for case in fx.get("cases").and_then(Value::as_array).unwrap() {
            total += 1;
            let src = case.get("doc").and_then(Value::as_str).unwrap();
            let id = case.get("id").and_then(Value::as_str);
            let doc = json::parse(src.as_bytes()).unwrap().unwrap();
            let got = search_es::document::parse_document(&mapping, &ctx, &doc, Some(src.as_bytes()), id);
            let ok = match (&got, case.get("error"), case.get("ignored")) {
                (Err(e), Some(want), _) => &strip(&e.to_value(false)) == want,
                (Ok(d), None, Some(Value::Arr(want))) => {
                    let want: Vec<String> = want.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
                    d.ignored == want
                }
                _ => false,
            };
            if !ok {
                let shown = match &got {
                    Ok(d) => format!("OK ignored {:?}", d.ignored),
                    Err(e) => format!("ERR {}", strip(&e.to_value(false)).to_json()),
                };
                let want = case.get("error").map(Value::to_json).unwrap_or_else(|| format!("ignored {}", case.get("ignored").map(Value::to_json).unwrap_or_default()));
                failures.push(format!("{src}\n   want {want}\n   got  {shown}"));
            }
        }
    }
    if !failures.is_empty() {
        panic!("{} of {total} document fixtures differ:\n{}", failures.len(), failures.join("\n"));
    }
}
