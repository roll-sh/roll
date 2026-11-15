//! Dynamic mapping (`mapping::dynamic::dynamic_update`) checked against Elasticsearch 9.5.5:
//! each case of `tests/fixtures/dynamic_mapping.json` is an index created with `mappings` into
//! which `doc` was indexed; the fixture holds the indexing status (and error) and the mapping
//! Elasticsearch ended up with.
//!
//! Re-capture with `ROLL_ES_CAPTURE=http://127.0.0.1:19200 cargo test -p search-es --test
//! dynamic_mapping -- --ignored` (creates and deletes the index `esm-dyn`).

use search_es::json::{Map, Value, parse};
use search_es::mapping::{Mapping, MappingCtx, MergeReason, dynamic::dynamic_update};
use search_es::meta::settings::Settings;

fn path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dynamic_mapping.json")
}

fn cases() -> Vec<Value> {
    let raw = std::fs::read(path()).unwrap();
    parse(&raw).unwrap().unwrap().as_array().unwrap().clone()
}

/// Strips a `[line:col] ` location prefix (document parse locations aren't part of the pure
/// mapping computation).
fn strip_location(s: &str) -> &str {
    if s.starts_with('[')
        && let Some(i) = s.find("] ")
        && s[1..i].split(':').all(|p| p.bytes().all(|b| b.is_ascii_digit()))
    {
        return &s[i + 2..];
    }
    s
}

#[test]
fn dynamic_updates_match_elasticsearch() {
    let mut failures = Vec::new();
    for (i, case) in cases().iter().enumerate() {
        let mut settings = Settings::from_json(case.get("settings").unwrap_or(&Value::Null)).unwrap();
        settings.normalize_prefix("index.");
        let ctx = MappingCtx::from_settings(&settings);
        let mappings = case.get("mappings").and_then(Value::as_object).cloned().unwrap_or_default();
        let base = Mapping::parse(&mappings, &ctx).unwrap();
        let doc: Map = case.get("doc").and_then(Value::as_object).cloned().unwrap();
        let status = case.get("status").and_then(Value::as_i64).unwrap();
        let label = format!("case {i}: mappings {} doc {}", Value::Obj(mappings.clone()).to_json(), Value::Obj(doc.clone()).to_json());
        let result = dynamic_update(&base, &doc, &ctx);
        // Values a field can't hold fail later, when the document is parsed into fields; the
        // mapping update itself must still be computed.
        let value_error = case.pointer(&["error", "reason"]).and_then(Value::as_str).is_some_and(|r| r.contains("failed to parse field ["));
        if value_error {
            if let Err(e) = result {
                failures.push(format!("{label}: unexpected error {}", e.into_es(None).reason()));
            }
            continue;
        }
        let expected_mapping = case.get("mapping").cloned().unwrap_or_default();
        match (status < 300, result) {
            (true, Ok(update)) => {
                let merged = match update {
                    Some(u) => match base.merge(&u, &ctx, MergeReason::MappingUpdate) {
                        Ok(m) => m,
                        Err(e) => {
                            failures.push(format!("{label}: merge failed: {}", e.reason()));
                            continue;
                        }
                    },
                    None => base.clone(),
                };
                let got = merged.to_value(&ctx, false);
                if got != expected_mapping {
                    failures.push(format!("{label}\n  got:      {}\n  expected: {}", got.to_json(), expected_mapping.to_json()));
                }
            }
            (false, Err(e)) => {
                let e = e.into_es(None);
                let err = case.get("error").unwrap();
                let exp_type = err.get("type").and_then(Value::as_str).unwrap_or("");
                let exp_reason = strip_location(err.get("reason").and_then(Value::as_str).unwrap_or(""));
                if e.kind != exp_type || strip_location(e.reason()) != exp_reason {
                    failures.push(format!("{label}\n  got error:      {} {}\n  expected error: {exp_type} {exp_reason}", e.kind, e.reason()));
                }
            }
            (true, Err(e)) => failures.push(format!("{label}: unexpected error {} {}", e.clone().into_es(None).kind, e.into_es(None).reason())),
            (false, Ok(u)) => failures.push(format!(
                "{label}: expected error {}, got update {}",
                case.get("error").map(Value::to_json).unwrap_or_default(),
                u.map(|u| u.to_value(&ctx, false).to_json()).unwrap_or_default()
            )),
        }
    }
    assert!(failures.is_empty(), "{} dynamic mapping cases differ:\n\n{}", failures.len(), failures.join("\n\n"));
}

/// Re-captures the expected results from a real Elasticsearch.
#[tokio::test]
#[ignore]
async fn capture() {
    let Ok(base) = std::env::var("ROLL_ES_CAPTURE") else {
        eprintln!("ROLL_ES_CAPTURE not set; nothing to capture");
        return;
    };
    let client = reqwest::Client::new();
    let call = |method: reqwest::Method, path: String, body: Option<String>| {
        let client = client.clone();
        let base = base.clone();
        async move {
            let mut rb = client.request(method, format!("{base}{path}")).header("content-type", "application/json");
            if let Some(b) = body {
                rb = rb.body(b);
            }
            let r = rb.send().await.unwrap();
            let status = r.status().as_u16() as i64;
            let v = parse(&r.bytes().await.unwrap()).ok().flatten().unwrap_or_default();
            (status, v)
        }
    };
    let mut out = Vec::new();
    for case in cases() {
        call(reqwest::Method::DELETE, "/esm-dyn".into(), None).await;
        let mut body = Map::new();
        body.insert("mappings".into(), case.get("mappings").cloned().unwrap_or_default());
        if let Some(s) = case.get("settings") {
            body.insert("settings".into(), s.clone());
        }
        let (st, created) = call(reqwest::Method::PUT, "/esm-dyn".into(), Some(Value::Obj(body).to_json())).await;
        assert_eq!(st, 200, "{}", created.to_json());
        let (st, indexed) = call(reqwest::Method::PUT, "/esm-dyn/_doc/1".into(), Some(case.get("doc").unwrap().to_json())).await;
        let mut c = Map::new();
        for k in ["mappings", "settings", "doc"] {
            if let Some(v) = case.get(k) {
                c.insert(k.into(), v.clone());
            }
        }
        c.insert("status".into(), st.into());
        if st >= 300 {
            c.insert("error".into(), indexed.get("error").cloned().unwrap_or_default());
        }
        let (_, m) = call(reqwest::Method::GET, "/esm-dyn/_mapping".into(), None).await;
        c.insert("mapping".into(), m.pointer(&["esm-dyn", "mappings"]).cloned().unwrap_or_default());
        call(reqwest::Method::DELETE, "/esm-dyn".into(), None).await;
        out.push(Value::Obj(c));
    }
    std::fs::write(path(), Value::Arr(out).to_pretty_json() + "\n").unwrap();
}
