//! Mapping parse/merge/print checked against responses captured from Elasticsearch 9.5.5
//! (`tests/fixtures/mapping_*.json`).

use super::*;
use crate::json::parse;

fn fixture(name: &str) -> Vec<Value> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read(&path).unwrap();
    parse(&raw).unwrap().unwrap().as_array().unwrap().clone()
}

fn ctx_for(settings: Option<&Value>) -> MappingCtx {
    let mut s = match settings {
        Some(v) => Settings::from_json(v).unwrap(),
        None => Settings::new(),
    };
    s.normalize_prefix("index.");
    MappingCtx::from_settings(&s)
}

/// Types roll search rejects on purpose.
fn skipped(case: &Value) -> bool {
    let s = case.to_json();
    s.contains("semantic_text") || s.contains("pattern_text")
}

fn check_error(label: &str, got: EsError, expected: &Value) {
    let err = expected.get("error").unwrap();
    let body = got.to_body(false);
    let g = body.get("error").unwrap();
    assert_eq!(g.get("type"), err.get("type"), "{label}: type; got {}", body.to_json());
    assert_eq!(g.get("reason"), err.get("reason"), "{label}: reason");
    let cb = |v: &Value| v.get("caused_by").map(|c| (c.get("type").cloned(), c.get("reason").cloned()));
    assert_eq!(cb(g), cb(err), "{label}: caused_by");
    let rc = |v: &Value| v.get("root_cause").and_then(|r| r.as_array()).and_then(|a| a.first()).and_then(|r| r.get("type")).cloned();
    assert_eq!(rc(g), rc(err), "{label}: root_cause");
    assert_eq!(body.get("status"), expected.get("status"), "{label}: status");
}

#[test]
fn create_index_mappings_match_elasticsearch() {
    let mut checked = 0;
    for name in ["mapping_create_1.json", "mapping_create_2.json"] {
        for case in fixture(name) {
            if skipped(&case) {
                continue;
            }
            let src = case.get("mappings").unwrap().as_object().unwrap().clone();
            let label = Value::Obj(src.clone()).to_json();
            let ctx = ctx_for(None);
            let status = case.get("status").and_then(Value::as_i64).unwrap();
            let res = for_create(&src, &ctx);
            if status == 200 {
                let m = res.unwrap_or_else(|e| panic!("{label}: unexpected error {}", e.to_body(false).to_json()));
                let expected = case.pointer(&["get", "mt", "mappings"]).unwrap();
                assert_eq!(m.to_value(&ctx, false), *expected, "{label}");
            } else {
                let e = match res {
                    Ok(m) => panic!("{label}: expected an error, got {}", m.to_value(&ctx, false).to_json()),
                    Err(e) => e,
                };
                check_error(&label, e, case.get("response").unwrap());
            }
            checked += 1;
        }
    }
    assert!(checked > 100, "{checked}");
}

#[test]
fn mapping_updates_match_elasticsearch() {
    let mut checked = 0;
    for case in fixture("mapping_merge_1.json") {
        let base = case.get("base").unwrap().as_object().unwrap().clone();
        let update = case.get("update").unwrap().as_object().unwrap().clone();
        let label = format!("{} + {}", Value::Obj(base.clone()).to_json(), Value::Obj(update.clone()).to_json());
        let ctx = ctx_for(case.get("settings"));
        let existing = for_create(&base, &ctx).unwrap_or_else(|e| panic!("{label}: base failed {}", e.to_body(false).to_json()));
        let status = case.get("status").and_then(Value::as_i64).unwrap();
        let res = for_update(&existing, &update, &ctx);
        if status == 200 {
            let m = res.unwrap_or_else(|e| panic!("{label}: unexpected error {}", e.to_body(false).to_json()));
            let expected = case.pointer(&["get", "mm", "mappings"]).unwrap();
            assert_eq!(m.to_value(&ctx, false), *expected, "{label}");
        } else {
            let e = match res {
                Ok(m) => panic!("{label}: expected an error, got {}", m.to_value(&ctx, false).to_json()),
                Err(e) => e,
            };
            check_error(&label, e, case.get("response").unwrap());
        }
        checked += 1;
    }
    assert!(checked > 40, "{checked}");
}

#[test]
fn every_type_prints_like_elasticsearch() {
    let ctx = ctx_for(None);
    let expected = parse(&std::fs::read(format!("{}/tests/fixtures/mapping_types.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap().unwrap();
    let props = expected.pointer(&["types", "mappings", "properties"]).unwrap().as_object().unwrap();
    let defaults_doc =
        parse(&std::fs::read(format!("{}/tests/fixtures/mapping_types_include_defaults.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap().unwrap();
    let defaults = defaults_doc.pointer(&["types", "mappings"]).unwrap().as_object().unwrap();
    for (name, def) in props {
        if matches!(name.as_str(), "pattern_text" | "alias") {
            continue;
        }
        // Rebuild the input definition: the type plus what Elasticsearch printed.
        let src = obj! { "properties" => obj! { name.clone() => def.clone() } };
        let m = for_create(src.as_object().unwrap(), &ctx).unwrap_or_else(|e| panic!("{name}: {}", e.to_body(false).to_json()));
        assert_eq!(m.to_value(&ctx, false).pointer(&["properties", name]).unwrap(), def, "{name}");
        // include_defaults prints every parameter in Elasticsearch's order.
        if let Some(full) = defaults.get(name).and_then(|d| d.pointer(&["mapping", name])) {
            let MapperRef::Field(f) = m.lookup(name).unwrap() else { continue };
            let got = f.to_value(&ctx, true);
            let got_keys: Vec<&String> = got.as_object().unwrap().keys().collect();
            let want_keys: Vec<&String> = full.as_object().unwrap().keys().collect();
            assert_eq!(got_keys, want_keys, "{name}: include_defaults keys");
            for (k, v) in full.as_object().unwrap() {
                if matches!(k.as_str(), "index_options" | "similarity") && name == "dense_vector" {
                    continue;
                }
                assert_eq!(got.get(k), Some(v), "{name}.{k}");
            }
        }
    }
}

#[test]
fn java_regex_classes_are_ascii() {
    for (java, rust) in [
        (r"\w+", r"[[:word:]]+"),
        (r"[\w.-]+", r"[[:word:].-]+"),
        (r"[^\d]", r"[^[:digit:]]"),
        (r"\bfoo\B", r"(?-u:\b)foo(?-u:\B)"),
        (r"\\d", r"\\d"),
        (r"[]\s]", r"[][:space:]]"),
        ("plain", "plain"),
    ] {
        assert_eq!(super::java_classes(java), rust, "{java}");
        let rx = regex::Regex::new(&format!("^(?:{})$", super::java_classes(java)));
        assert!(rx.is_ok(), "{java}: {rx:?}");
    }
}
