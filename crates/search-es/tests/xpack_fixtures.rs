//! Differential tests for the x-pack APIs (`crate::xpack`): request sequences replayed against
//! roll and compared with the responses Elasticsearch 9.5.5 (trial license) gave for the same
//! requests (`tests/fixtures/xpack/*.json`).
//!
//! The format is `doc_fixtures.rs`'s: a scenario is a list of steps (`req` = `"METHOD
//! /path?query"`, optional `body` (JSON) or `raw` (text, e.g. NDJSON) and `headers`), each with
//! the status and body Elasticsearch answered. Values that differ by nature (`took`, UUIDs,
//! timestamps) are masked on both sides; a step may add masks (`mask`), compare without key
//! order (`unordered`), or be replayed without comparing (`skip` with a reason). Scenarios
//! clean up the x-pack objects they create in their own last steps.
//!
//! To (re)capture from a real Elasticsearch (security may be on, over plain HTTP):
//! `ROLL_ES_CAPTURE=http://127.0.0.1:19530 ROLL_ES_CAPTURE_AUTH=elastic:<password> cargo test
//! -p search-es --test xpack_fixtures -- --ignored` (`ROLL_ES_CAPTURE_ONLY=<file part>` picks
//! scenarios). Capturing only touches objects named `compat-xpack-*` and never changes cluster
//! settings.

use base64::Engine;
use search_es::json::{Map, Value, parse};
use search_es::{Cluster, ClusterConfig, EsRequest, MemoryStore};
use std::path::PathBuf;
use std::sync::Arc;

const DEFAULT_MASKS: &[&str] = &[
    "took",
    "timestamp",
    "uuid",
    "index_uuid",
    "creation_date",
    "index.creation_date",
    "index.uuid",
    "task",
    "start_time_in_millis",
    "running_time_in_nanos",
];

const PREFIX: &str = "compat-xpack-";

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/xpack")
}

fn scenarios() -> Vec<(String, Value)> {
    let mut entries: Vec<_> = std::fs::read_dir(dir()).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
    entries.sort();
    entries
        .into_iter()
        .map(|p| {
            let raw = std::fs::read(&p).unwrap();
            let v = parse(&raw).unwrap_or_else(|e| panic!("{}: {e}", p.display())).unwrap();
            (p.file_name().unwrap().to_string_lossy().into_owned(), v)
        })
        .collect()
}

/// Masks the values of `keys` anywhere in `v`, and random base64 UUIDs inside strings.
fn mask(v: &mut Value, keys: &[String]) {
    match v {
        Value::Obj(m) => {
            for (k, val) in m.iter_mut() {
                if keys.iter().any(|x| x == k) {
                    *val = Value::Str("<masked>".into());
                } else {
                    mask(val, keys);
                }
            }
        }
        Value::Arr(a) => a.iter_mut().for_each(|x| mask(x, keys)),
        Value::Str(s) => *s = mask_uuids(s),
        _ => {}
    }
}

/// Replaces 20–22 character URL-safe base64 tokens with mixed case (auto-generated ids, UUIDs).
fn mask_uuids(s: &str) -> String {
    let b = s.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-';
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if word(b[i]) {
            let start = i;
            while i < b.len() && word(b[i]) {
                i += 1;
            }
            let tok = &s[start..i];
            let random = (20..=22).contains(&tok.len()) && tok.bytes().any(|c| c.is_ascii_uppercase()) && tok.bytes().any(|c| c.is_ascii_lowercase()) && !tok.contains(PREFIX);
            out.push_str(if random { "<uuid>" } else { tok });
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn step_masks(step: &Value) -> Vec<String> {
    let mut keys: Vec<String> = DEFAULT_MASKS.iter().map(|s| s.to_string()).collect();
    if let Some(Value::Arr(extra)) = step.get("mask") {
        keys.extend(extra.iter().filter_map(Value::as_str).map(str::to_string));
    }
    keys
}

fn split_req(req: &str) -> (&str, &str) {
    req.split_once(' ').unwrap_or(("GET", req))
}

fn body_of(step: &Value) -> Option<String> {
    if let Some(raw) = step.get("raw").and_then(Value::as_str) {
        return Some(raw.to_string());
    }
    step.get("body").map(Value::to_json)
}

fn headers_of(step: &Value, has_body: bool) -> Vec<(String, String)> {
    let mut h = Vec::new();
    if let Some(Value::Obj(m)) = step.get("headers") {
        for (k, v) in m {
            h.push((k.clone(), v.to_es_string()));
        }
    }
    if has_body && !h.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
        let ct = if step.get("raw").is_some() { "application/x-ndjson" } else { "application/json" };
        h.push(("Content-Type".into(), ct.into()));
    }
    h
}

/// Sorts arrays of objects and object keys so `unordered` steps compare as sets.
fn canonical(v: &Value) -> String {
    match v {
        Value::Obj(m) => {
            let mut parts: Vec<String> = m.iter().map(|(k, v)| format!("{k:?}:{}", canonical(v))).collect();
            parts.sort();
            format!("{{{}}}", parts.join(","))
        }
        Value::Arr(a) => {
            let mut parts: Vec<String> = a.iter().map(canonical).collect();
            parts.sort();
            format!("[{}]", parts.join(","))
        }
        other => other.to_json(),
    }
}

#[tokio::test]
async fn responses_match_elasticsearch() {
    let mut failures = Vec::new();
    let mut checked = 0;
    for (file, scenario) in scenarios() {
        let mut cfg = ClusterConfig::new("docker-cluster");
        cfg.node_name = "es-node".into();
        // The reference node registers `/tmp` as `path.repo`; each scenario gets its own.
        let repo = std::env::temp_dir().join(format!("roll-xpack-fixtures-{}-{file}", std::process::id()));
        cfg.node_settings.push(("path.repo".into(), repo.to_string_lossy().into_owned()));
        let cluster = Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap();
        for (i, step) in scenario.get("steps").and_then(Value::as_array).unwrap().iter().enumerate() {
            let req = step.get("req").and_then(Value::as_str).unwrap();
            let label = format!("{file} #{i} {req}");
            let (method, uri) = split_req(req);
            let body = body_of(step);
            let mut r = EsRequest::new(method, uri);
            r.headers = headers_of(step, body.is_some());
            if let Some(b) = &body {
                r.body = b.clone().into();
            }
            let resp = search_es::handle(&cluster, r).await;
            if step.get("skip").is_some() {
                continue;
            }
            let Some(status) = step.get("status").and_then(Value::as_i64) else { continue };
            checked += 1;
            let got_text = if resp.head { String::new() } else { String::from_utf8_lossy(&resp.body).into_owned() };
            if resp.status as i64 != status {
                failures.push(format!("{label}: status {} != {status}\n  got:      {got_text}\n  expected: {}", resp.status, step.get("response").map(Value::to_json).unwrap_or_default()));
                continue;
            }
            let keys = step_masks(step);
            if let Some(expected) = step.get("response") {
                let mut exp = expected.clone();
                let mut got = if resp.head { Value::Str(String::new()) } else { parse(&resp.body).ok().flatten().unwrap_or(Value::Str(got_text.clone())) };
                mask(&mut exp, &keys);
                mask(&mut got, &keys);
                let unordered = step.get("unordered").and_then(Value::as_bool) == Some(true);
                let same = if unordered { canonical(&got) == canonical(&exp) } else { got.to_json() == exp.to_json() };
                if !same {
                    failures.push(format!("{label}:\n  got:      {}\n  expected: {}", got.to_json(), exp.to_json()));
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("roll-xpack-fixtures-{}", std::process::id())));
    if !failures.is_empty() {
        panic!("{} of {checked} steps differ from Elasticsearch:\n\n{}", failures.len(), failures.join("\n\n"));
    }
    assert!(checked > 0);
}

/// Re-captures the fixtures from a real Elasticsearch (see the module docs).
#[tokio::test]
#[ignore]
async fn capture() {
    let Ok(base) = std::env::var("ROLL_ES_CAPTURE") else {
        eprintln!("ROLL_ES_CAPTURE not set; nothing to capture");
        return;
    };
    let auth = std::env::var("ROLL_ES_CAPTURE_AUTH").ok().map(|a| format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(a)));
    let only = std::env::var("ROLL_ES_CAPTURE_ONLY").ok();
    let client = reqwest::Client::new();
    for (file, scenario) in scenarios() {
        if only.as_deref().is_some_and(|o| !file.contains(o)) {
            continue;
        }
        cleanup(&client, &base, auth.as_deref()).await;
        let mut steps_out = Vec::new();
        for step in scenario.get("steps").and_then(Value::as_array).unwrap() {
            let req = step.get("req").and_then(Value::as_str).unwrap();
            let (method, uri) = split_req(req);
            let (_, path) = uri.split_once('/').unwrap_or(("", uri));
            assert!(path.is_empty() || path.starts_with('_') || path.starts_with(PREFIX), "{file}: capture only touches {PREFIX}* objects: {req}");
            let body = body_of(step);
            let mut rb = client.request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), format!("{base}{uri}"));
            for (k, v) in headers_of(step, body.is_some()) {
                rb = rb.header(k, v);
            }
            if let Some(a) = &auth {
                rb = rb.header("Authorization", a);
            }
            if let Some(b) = body {
                rb = rb.body(b);
            }
            let resp = rb.send().await.unwrap();
            let status = resp.status().as_u16();
            let bytes = resp.bytes().await.unwrap();
            let mut out = Map::new();
            if let Value::Obj(m) = step {
                for (k, v) in m {
                    if !matches!(k.as_str(), "status" | "response") {
                        out.insert(k.clone(), v.clone());
                    }
                }
            }
            out.insert("status".into(), Value::from(status as i64));
            if let Ok(Some(v)) = parse(&bytes) {
                out.insert("response".into(), v);
            }
            steps_out.push(Value::Obj(out));
        }
        cleanup(&client, &base, auth.as_deref()).await;
        let mut sc = scenario.as_object().unwrap().clone();
        sc.insert("steps".into(), Value::Arr(steps_out));
        std::fs::write(dir().join(&file), Value::Obj(sc).to_pretty_json() + "\n").unwrap();
        eprintln!("captured {file}");
    }
}

/// Deletes every `compat-xpack-*` index and ingest pipeline on the reference cluster.
async fn cleanup(client: &reqwest::Client, base: &str, auth: Option<&str>) {
    let req = |m: reqwest::Method, path: String| {
        let mut rb = client.request(m, path);
        if let Some(a) = auth {
            rb = rb.header("Authorization", a);
        }
        rb
    };
    let get = |path: String| {
        let rb = req(reqwest::Method::GET, path);
        async move {
            let r = rb.send().await.ok()?;
            let b = r.bytes().await.ok()?;
            parse(&b).ok().flatten()
        }
    };
    if let Some(Value::Arr(rows)) = get(format!("{base}/_cat/indices/{PREFIX}*?format=json&h=index&expand_wildcards=all")).await {
        for r in rows {
            if let Some(i) = r.get("index").and_then(Value::as_str) {
                let _ = req(reqwest::Method::DELETE, format!("{base}/{i}")).send().await;
            }
        }
    }
    if let Some(Value::Obj(m)) = get(format!("{base}/_ingest/pipeline/{PREFIX}*")).await {
        for n in m.keys() {
            let _ = req(reqwest::Method::DELETE, format!("{base}/_ingest/pipeline/{n}")).send().await;
        }
    }
}
