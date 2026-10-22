//! Runs the captured update and ingest fixtures (`tests/fixtures/update.json`, `ingest.json`,
//! produced by `tests/fixtures/capture_write.py` against Elasticsearch 9.5.5) through the engine
//! the way the document APIs drive write scripts: build `ctx`, run, read `op`, serialize the
//! new `_source` with XContent rules, and compare outcomes and error bodies with what
//! Elasticsearch answered.

use std::path::Path;

use search_painless::es::write::{ctx_of, ctx_value, CtxKind};
use search_painless::xcontent::{parse_source, parse_source_unordered, to_xcontent};
use search_painless::{Context, Engine, NoHost, ScriptError, Settings, Value};
use serde_json::{json, Value as Json};

const INDEX: &str = "painless-write";
const NOW: i64 = 1_700_000_000_000;

fn load(name: &str) -> Vec<Json> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Our `script_exception` body without `root_cause` (Elasticsearch nests it as a cause).
fn script_error_body(e: &ScriptError) -> Json {
    let mut body = e.to_response()["error"].clone();
    if let Json::Object(o) = &mut body {
        o.remove("root_cause");
    }
    body
}

/// Masks the per-request timestamp (`_now=…`) and compares.
fn same(want: &Json, have: &Json) -> bool {
    fn norm(v: &Json) -> Json {
        match v {
            Json::String(s) => {
                let mut out = String::new();
                let mut rest = s.as_str();
                while let Some(i) = rest.find("_now=") {
                    out.push_str(&rest[..i + 5]);
                    rest = rest[i + 5..].trim_start_matches(|c: char| c.is_ascii_digit());
                    out.push('N');
                }
                out.push_str(rest);
                Json::String(out)
            }
            Json::Array(a) => Json::Array(a.iter().map(norm).collect()),
            Json::Object(o) => Json::Object(o.iter().map(|(k, v)| (k.clone(), norm(v))).collect()),
            other => other.clone(),
        }
    }
    norm(want) == norm(have)
}

/// What the update API answers: `(status, result or error, stored source)`.
fn run_update(engine: &Engine, case: &Json) -> Result<(u64, Json, Option<String>), String> {
    let req = &case["request"];
    let src = req["script"]["source"].as_str().unwrap();
    let params = req["script"].get("params").cloned().unwrap_or(json!({}));
    let id = req["id"].as_str().unwrap();
    let upsert = req.get("upsert_raw").and_then(|u| u.as_str());
    let script = match engine.compile(src, Context::Update) {
        Ok(s) => s,
        Err(e) => return Ok((400, json!({ "error": { "type": "illegal_argument_exception", "reason": "failed to execute script", "caused_by": script_error_body(&e) } }), None)),
    };
    let (kind, raw, meta): (CtxKind, &str, Vec<(&str, Value)>) = match upsert {
        Some(u) => (CtxKind::Upsert, u, vec![("_index", Value::str(INDEX)), ("_id", Value::str(id)), ("op", Value::str("create")), ("_now", Value::Long(NOW))]),
        None => (
            CtxKind::Update,
            req["doc_raw"].as_str().unwrap(),
            vec![
                ("_index", Value::str(INDEX)),
                ("_id", Value::str(id)),
                ("_version", Value::Long(1)),
                ("_routing", Value::Null),
                ("_type", Value::str("_doc")),
                ("op", Value::str("index")),
                ("_now", Value::Long(NOW)),
            ],
        ),
    };
    let source = if kind == CtxKind::Upsert { parse_source_unordered(raw) } else { parse_source(raw) }.map_err(|e| format!("bad source: {e:?}"))?;
    let ctx = ctx_value(kind, source, &meta, None)?;
    let mut inst = script.instance();
    inst.set_params(&params);
    inst.set_var("ctx", ctx.clone());
    if let Err(e) = inst.execute(&mut NoHost) {
        return Ok((e.status as u64, json!({ "error": { "type": "illegal_argument_exception", "reason": "failed to execute script", "caused_by": script_error_body(&e) } }), None));
    }
    let c = ctx_of(&ctx).unwrap();
    let op = c.op().unwrap_or_default();
    let created = kind == CtxKind::Upsert;
    match op.as_str() {
        "index" | "create" if (op == "create") == created => match to_xcontent(&c.source()) {
            Ok(s) => Ok((if created { 201 } else { 200 }, json!(if created { "created" } else { "updated" }), Some(s))),
            Err(e) => Ok((400, json!({ "error": { "type": e.kind, "reason": e.reason } }), None)),
        },
        "delete" if !created => Ok((200, json!("deleted"), None)),
        _ => Ok((200, json!("noop"), None)),
    }
}

#[test]
fn update() {
    let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
    let cases = load("update.json");
    let mut failures = Vec::new();
    for c in &cases {
        let src = c["request"]["script"]["source"].as_str().unwrap();
        let status = c["status"].as_u64().unwrap();
        let resp = &c["response"];
        let (gs, got, stored) = match run_update(&engine, c) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{src}\n  harness: {e}"));
                continue;
            }
        };
        let ok = if status < 300 {
            let want_raw = c.get("after_raw").and_then(|a| a.as_str());
            let result = &resp["result"];
            let stored_ok = match result.as_str() {
                Some("updated" | "created") => stored.as_deref() == want_raw || same(&json!(want_raw), &json!(stored)),
                _ => true,
            };
            gs == status && &got == result && stored_ok
        } else {
            let mut want = resp["error"].clone();
            if let Json::Object(o) = &mut want {
                o.remove("root_cause");
            }
            gs == status && same(&want, &got["error"])
        };
        if !ok {
            failures.push(format!(
                "{src}\n  want: {status} {}\n        {}\n  got:  {gs} {}\n        {}",
                if status < 300 { resp["result"].to_string() } else { resp["error"].to_string() },
                c.get("after_raw").and_then(|a| a.as_str()).unwrap_or(""),
                got,
                stored.unwrap_or_default()
            ));
        }
    }
    for f in &failures {
        eprintln!("---- {f}");
    }
    assert!(failures.is_empty(), "update.json: {} of {} cases differ from Elasticsearch", failures.len(), cases.len());
}

/// What `_ingest/pipeline/_simulate` answers for a one-script pipeline: the document (metadata
/// as strings, `_source`) or the error.
/// An ingest pipeline of one processor: a `script` processor, or (`condition`) a `set` processor
/// whose `if` is the script and which sets `_cond: true` when it holds.
fn run_ingest(engine: &Engine, case: &Json, condition: bool) -> Result<Json, String> {
    let req = &case["request"];
    let src = req["script"]["source"].as_str().unwrap();
    let params = req["script"].get("params").cloned().unwrap_or(json!({}));
    let script = match engine.compile(src, if condition { Context::ProcessorConditional } else { Context::Ingest }) {
        Ok(s) => s,
        Err(e) => {
            let mut body = script_error_body(&e);
            body["processor_type"] = json!(if condition { "set" } else { "script" });
            return Ok(json!({ "top": body }));
        }
    };
    let doc = &req["doc"];
    let source = parse_source_unordered(req["source_raw"].as_str().unwrap()).map_err(|e| format!("bad source: {e:?}"))?;
    let mut meta: Vec<(&str, Value)> = vec![
        ("_index", doc.get("_index").and_then(|x| x.as_str()).map(Value::str).unwrap_or(Value::str("_index"))),
        ("_id", doc.get("_id").and_then(|x| x.as_str()).map(Value::str).unwrap_or(Value::str("_id"))),
        ("_version", Value::Long(doc.get("_version").and_then(|x| x.as_i64()).unwrap_or(-3))),
    ];
    if let Some(r) = doc.get("_routing").and_then(|x| x.as_str()) {
        meta.push(("_routing", Value::str(r)));
    }
    if let Some(r) = doc.get("_version_type").and_then(|x| x.as_str()) {
        meta.push(("_version_type", Value::str(r)));
    }
    let ctx = ctx_value(CtxKind::Ingest, source, &meta, Some(search_painless::value::zoned_date_time_millis(NOW)))?;
    let mut inst = script.instance();
    inst.set_params(&params);
    if condition {
        inst.set_var("ctx", search_painless::es::write::conditional_ctx(&ctx));
        match inst.execute_bool(&mut NoHost) {
            Err(e) => return Ok(json!({ "error": script_error_body(&e) })),
            Ok(true) => {
                if let Some(Value::Map(m)) = Some(ctx_of(&ctx).unwrap().source()) {
                    m.insert_raw(Value::str("_cond"), Value::Bool(true));
                }
            }
            Ok(false) => {}
        }
    } else {
        inst.set_var("ctx", ctx.clone());
        if let Err(e) = inst.execute(&mut NoHost) {
            return Ok(json!({ "error": script_error_body(&e) }));
        }
    }
    let c = ctx_of(&ctx).unwrap();
    let source = c.source();
    if let Err(e) = search_painless::xcontent::check_ingest_value(&source) {
        return Ok(json!({ "error": { "type": e.kind, "reason": e.reason } }));
    }
    let mut out = serde_json::Map::new();
    for (k, v) in c.metadata() {
        if !v.is_null() {
            out.insert(k, json!(v.to_java_string()));
        }
    }
    let src_json = to_xcontent(&source).map_err(|e| format!("{e:?}"))?;
    out.insert("_source".into(), serde_json::from_str(&src_json).unwrap());
    Ok(json!({ "doc": out }))
}

#[test]
fn ingest() {
    check_ingest("ingest.json", false);
}

#[test]
fn ingest_conditions() {
    check_ingest("conditional.json", true);
}

/// `Processors.*` of ingest scripts, and Jackson's JSON errors (`Processors.json`, `Json.load`).
#[test]
fn ingest_processors() {
    check_ingest("processors.json", false);
}

#[test]
fn ingest_json_errors() {
    check_ingest("jackson.json", false);
}

fn check_ingest(name: &str, condition: bool) {
    let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
    let cases = load(name);
    let known = known_diffs(name);
    let mut failures = Vec::new();
    for c in &cases {
        let src = c["request"]["script"]["source"].as_str().unwrap();
        if known.iter().any(|k| k == src) {
            continue;
        }
        let resp = &c["response"];
        let got = match run_ingest(&engine, c, condition) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{src}\n  harness: {e}"));
                continue;
            }
        };
        let strip = |v: &Json| {
            let mut v = v.clone();
            if let Json::Object(o) = &mut v {
                o.remove("root_cause");
                o.remove("_ingest");
            }
            v
        };
        let (want, have) = if let Some(d) = resp.get("docs") {
            let d = &d[0];
            match (d.get("error"), d.get("doc")) {
                (Some(e), _) => (strip(e), got.get("error").map(strip).unwrap_or(got.clone())),
                (_, Some(doc)) => (strip(doc), got.get("doc").cloned().unwrap_or(got.clone())),
                _ => (d.clone(), got.clone()),
            }
        } else {
            (strip(&resp["error"]), got.get("top").cloned().unwrap_or(got.clone()))
        };
        if !same(&want, &have) {
            failures.push(format!("{src}\n  want: {want}\n  got:  {have}"));
        }
    }
    for f in &failures {
        eprintln!("---- {f}");
    }
    assert!(failures.is_empty(), "{name}: {} of {} cases differ from Elasticsearch", failures.len(), cases.len());
}

/// Sources of cases known to differ (`tests/fixtures/known_diffs.json`, as in `execute.rs`).
fn known_diffs(name: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/known_diffs.json");
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let all: Json = serde_json::from_str(&text).unwrap();
    all.get(name).and_then(|l| l.as_array()).map(|l| l.iter().filter_map(|c| c["source"].as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// Update-by-query and reindex: `(op, _id, _routing, stored source)` or the script error.
/// `(op, _id, _routing, stored source)` after a successful script.
type Written = (String, Value, Value, String);

fn run_bulk_write(engine: &Engine, case: &Json, ctx_kind: Context) -> Result<Result<Written, Json>, String> {
    let req = &case["request"];
    let src = req["script"]["source"].as_str().unwrap();
    let params = req["script"].get("params").cloned().unwrap_or(json!({}));
    let id = req["id"].as_str().unwrap();
    let script = match engine.compile(src, ctx_kind) {
        Ok(s) => s,
        Err(e) => return Ok(Err(script_error_body(&e))),
    };
    let kind = if ctx_kind == Context::Reindex { CtxKind::Reindex } else { CtxKind::UpdateByQuery };
    let meta = vec![
        ("_index", Value::str(INDEX)),
        ("_id", Value::str(id)),
        ("_version", Value::Long(-1)),
        ("_routing", Value::Null),
        ("op", Value::str("index")),
        ("_now", Value::Long(NOW)),
    ];
    let source = parse_source_unordered(req["doc_raw"].as_str().unwrap()).map_err(|e| format!("{e:?}"))?;
    let ctx = ctx_value(kind, source, &meta, None)?;
    let mut inst = script.instance();
    inst.set_params(&params);
    inst.set_var("ctx", ctx.clone());
    if let Err(e) = inst.execute(&mut NoHost) {
        return Ok(Err(script_error_body(&e)));
    }
    let c = ctx_of(&ctx).unwrap();
    let stored = match to_xcontent(&c.source()) {
        Ok(s) => s,
        Err(e) => return Ok(Err(json!({ "host": e.reason }))),
    };
    Ok(Ok((c.op().unwrap_or_default(), c.meta_get("_id"), c.meta_get("_routing"), stored)))
}

fn check_bulk_write(file: &str, ctx_kind: Context) {
    let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
    let cases = load(file);
    let mut failures = Vec::new();
    let mut checked = 0;
    for c in &cases {
        let src = c["request"]["script"]["source"].as_str().unwrap();
        let resp = &c["response"];
        let got = match run_bulk_write(&engine, c, ctx_kind) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{src}\n  harness: {e}"));
                continue;
            }
        };
        let ok = match (resp.get("error"), &got) {
            // errors outside the script (version conflicts, serialization) belong to the host
            (Some(e), _) if e["type"] != "script_exception" => continue,
            (Some(_), Err(have)) if have.get("host").is_some() => continue,
            (Some(e), Err(have)) => {
                let mut want = e.clone();
                want.as_object_mut().unwrap().remove("root_cause");
                same(&want, have)
            }
            (Some(_), Ok(_)) => false,
            (None, Err(_)) => false,
            (None, Ok((op, id, routing, stored))) => {
                let after = c["after"].as_array().unwrap();
                match op.as_str() {
                    "index" => {
                        let stored: Json = serde_json::from_str(stored).unwrap();
                        after.len() == 1
                            && after[0]["_source"] == stored
                            && (id.is_null() || after[0]["_id"] == json!(id.to_java_string()))
                            && after[0]["_routing"] == if routing.is_null() { Json::Null } else { json!(routing.to_java_string()) }
                    }
                    "noop" => resp["noops"] == 1,
                    "delete" => resp["deleted"] == 1,
                    _ => false,
                }
            }
        };
        checked += 1;
        if !ok {
            failures.push(format!("{src}\n  want: {} {}\n  got:  {got:?}", resp, c["after"]));
        }
    }
    for f in &failures {
        eprintln!("---- {f}");
    }
    assert!(failures.is_empty(), "{file}: {} of {checked} cases differ from Elasticsearch", failures.len());
}

#[test]
fn update_by_query() {
    check_bulk_write("ubq.json", Context::UpdateByQuery);
}

#[test]
fn reindex() {
    check_bulk_write("reindex.json", Context::Reindex);
}
