//! Differential REST tests: request sequences replayed against roll and compared with the
//! responses Elasticsearch 9.5.5 gave for the same requests (`tests/fixtures/rest/*.json`).
//!
//! Each fixture is a scenario: a list of steps (`req` = `"METHOD /path?query"`, optional
//! `body` (JSON) or `raw` (text) and `headers`), each with the status, body and selected
//! headers Elasticsearch answered. Values that differ by nature (UUIDs, timestamps) are masked
//! on both sides before comparing; a step may add masks (`mask`) or be skipped with a reason
//! (`skip`).
//!
//! To (re)capture the expected responses from a real Elasticsearch:
//! `ROLL_ES_CAPTURE=http://127.0.0.1:19200 cargo test -p search-es --test rest_fixtures -- --ignored`.
//! Scenarios with `"dialect": "opensearch"` are captured from a real OpenSearch 3.9.0 instead
//! (`ROLL_ES_CAPTURE=<opensearch url> ROLL_ES_CAPTURE_ONLY=os_`) and replayed against the
//! opensearch dialect.
//! Capturing only creates and deletes objects named `esm-*` and never changes cluster settings.

use search_es::json::{Map, Value, parse};
use search_es::{Cluster, ClusterConfig, EsRequest, MemoryStore};
use std::path::PathBuf;
use std::sync::Arc;

const DEFAULT_MASKS: &[&str] = &[
    "uuid",
    "index_uuid",
    "cluster_uuid",
    "state_uuid",
    "creation_date",
    "index.creation_date",
    "index.uuid",
    "created_date_millis",
    "modified_date_millis",
    "creation.date",
    "creation.date.string",
    "epoch",
    "timestamp",
    "number_of_pending_tasks",
    "number_of_in_flight_fetch",
    "task_max_waiting_in_queue_millis",
];

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rest")
}

fn scenarios() -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir()).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
    entries.sort();
    for p in entries {
        let raw = std::fs::read(&p).unwrap();
        let v = parse(&raw).unwrap_or_else(|e| panic!("{}: {e}", p.display())).unwrap();
        out.push((p.file_name().unwrap().to_string_lossy().into_owned(), v));
    }
    out
}

fn mask(v: &mut Value, keys: &[String]) {
    mask_in(v, keys, "");
}

/// Masks values of the keys in `keys`; `parent.key` entries mask `key` only inside `parent`.
fn mask_in(v: &mut Value, keys: &[String], parent: &str) {
    // Object keys carrying dates (backing index names) or random names are normalized too,
    // keeping their order.
    if let Value::Obj(m) = v
        && m.keys().any(|k| mask_uuids(k) != *k)
    {
        let old = std::mem::take(m);
        for (k, val) in old {
            m.insert(mask_uuids(&k), val);
        }
    }
    match v {
        Value::Obj(m) => {
            for (k, val) in m.iter_mut() {
                if keys.iter().any(|x| x == k || x.strip_prefix(parent).and_then(|r| r.strip_prefix('.')) == Some(k.as_str())) {
                    *val = Value::Str("<masked>".into());
                } else {
                    mask_in(val, keys, k);
                }
            }
        }
        Value::Arr(a) => a.iter_mut().for_each(|x| mask_in(x, keys, parent)),
        Value::Str(s) => *s = mask_uuids(s),
        _ => {}
    }
}

/// Masks `key: value` (YAML) and `"key":value` (JSON) pairs of masked keys in a text body, and
/// UUIDs anywhere.
fn mask_text(s: &str, keys: &[String]) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let masked = keys.iter().any(|k| trimmed.starts_with(&format!("{k}: ")) || trimmed.starts_with(&format!("\"{k}\" : ")));
        if masked {
            let indent = &line[..line.len() - trimmed.len()];
            let key = trimmed.split(':').next().unwrap_or("");
            out.push_str(indent);
            out.push_str(key);
            out.push_str(": <masked>\n");
        } else {
            out.push_str(line);
        }
    }
    mask_uuids(&out)
}

/// Replaces `yyyy.MM.dd` dates (data stream backing index names carry the creation day).
fn mask_dates(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        let is_date = i + 10 <= b.len()
            && b[i..i + 4].iter().all(u8::is_ascii_digit)
            && b[i + 4] == b'.'
            && b[i + 5..i + 7].iter().all(u8::is_ascii_digit)
            && b[i + 7] == b'.'
            && b[i + 8..i + 10].iter().all(u8::is_ascii_digit);
        if is_date {
            out.push_str("<date>");
            i += 10;
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Replaces random base64 UUIDs (22 URL-safe characters with mixed case) inside text, e.g.
/// `index [x/Q4Oy1sisTRyNieQ43apZ4A] already exists`.
fn mask_uuids(s: &str) -> String {
    let s = &mask_dates(s);
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
            let looks_random = tok.len() == 22 && tok.bytes().any(|c| c.is_ascii_uppercase()) && tok.bytes().any(|c| c.is_ascii_lowercase());
            // Simulated templates get a random lower-cased name (also inside alias names made
            // from an `{index}` placeholder).
            let simulated = ["simulate_template_", "simulate_index_template_"].iter().find_map(|p| tok.find(*p).map(|at| at + p.len()));
            match simulated {
                Some(end) => {
                    out.push_str(&tok[..end]);
                    out.push_str("<uuid>");
                }
                None => out.push_str(if looks_random { "<uuid>" } else { tok }),
            }
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
        h.push(("Content-Type".into(), "application/json".into()));
    }
    h
}

#[tokio::test]
async fn responses_match_elasticsearch() {
    let mut failures = Vec::new();
    let mut checked = 0;
    for (file, scenario) in scenarios() {
        let mut cfg = ClusterConfig::new("docker-cluster");
        cfg.node_name = "es-node".into();
        // Scenarios captured from OpenSearch run against the opensearch dialect.
        if scenario.get("dialect").and_then(Value::as_str) == Some("opensearch") {
            cfg.dialect = search_es::Dialect::OpenSearch;
        }
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
            if let Some(reason) = step.get("skip").and_then(Value::as_str) {
                let _ = reason;
                continue;
            }
            let Some(status) = step.get("status").and_then(Value::as_i64) else { continue };
            checked += 1;
            let got_text = if resp.head { String::new() } else { String::from_utf8_lossy(&resp.body).into_owned() };
            if resp.status as i64 != status {
                failures.push(format!("{label}: status {} != {status}\n  got:      {got_text}\n  expected: {}", resp.status, step.get("response").map(Value::to_json).or_else(|| step.get("text").map(Value::to_json)).unwrap_or_default()));
                continue;
            }
            let keys = step_masks(step);
            if let Some(expected) = step.get("response") {
                let mut exp = expected.clone();
                let mut got = if resp.head { Value::Str(String::new()) } else { parse(&resp.body).ok().flatten().unwrap_or(Value::Str(got_text.clone())) };
                mask(&mut exp, &keys);
                mask(&mut got, &keys);
                if got != exp {
                    failures.push(format!("{label}:\n  got:      {}\n  expected: {}", got.to_json(), exp.to_json()));
                    continue;
                }
                // Key order is checked too (`ROLL_ES_FIXTURES_LOOSE=1` relaxes it while iterating).
                let strict = std::env::var("ROLL_ES_FIXTURES_LOOSE").is_err() && step.get("unordered").and_then(Value::as_bool) != Some(true);
                if strict && got.to_json() != exp.to_json() {
                    failures.push(format!("{label}: key order\n  got:      {}\n  expected: {}", got.to_json(), exp.to_json()));
                    continue;
                }
            } else if let Some(text) = step.get("text").and_then(Value::as_str)
                && mask_text(&got_text, &keys) != mask_text(text, &keys)
            {
                failures.push(format!("{label}:\n  got:      {got_text:?}\n  expected: {text:?}"));
                continue;
            }
            if let Some(Value::Obj(hs)) = step.get("response_headers") {
                for (k, v) in hs {
                    let got: Vec<String> = resp.headers.iter().filter(|(hk, _)| hk.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone()).collect();
                    let exp: Vec<String> = match v {
                        Value::Arr(a) => a.iter().map(Value::to_es_string).collect(),
                        other => vec![other.to_es_string()],
                    };
                    let normalize = |v: &Vec<String>| -> Vec<String> {
                        v.iter().map(|s| if k.eq_ignore_ascii_case("warning") { warning_text(s) } else { s.to_ascii_lowercase().replace(' ', "") }).collect()
                    };
                    if normalize(&got) != normalize(&exp) {
                        failures.push(format!("{label}: header {k}\n  got:      {got:?}\n  expected: {exp:?}"));
                    }
                }
            }
        }
    }
    if !failures.is_empty() {
        panic!("{} of {checked} steps differ from Elasticsearch:\n\n{}", failures.len(), failures.join("\n\n"));
    }
    assert!(checked > 0);
}

/// The quoted text of a `Warning` header (the agent part names the build).
fn warning_text(h: &str) -> String {
    h.split_once('"').map(|(_, rest)| rest.trim_end_matches('"').to_string()).unwrap_or_else(|| h.to_string())
}

/// Re-captures every fixture from a real Elasticsearch (see the module docs).
#[tokio::test]
#[ignore]
async fn capture() {
    let Ok(base) = std::env::var("ROLL_ES_CAPTURE") else {
        eprintln!("ROLL_ES_CAPTURE not set; nothing to capture");
        return;
    };
    let only = std::env::var("ROLL_ES_CAPTURE_ONLY").ok();
    let client = reqwest::Client::new();
    for (file, scenario) in scenarios() {
        if only.as_deref().is_some_and(|o| !file.contains(o)) {
            continue;
        }
        cleanup(&client, &base).await;
        let mut steps_out = Vec::new();
        for step in scenario.get("steps").and_then(Value::as_array).unwrap() {
            let req = step.get("req").and_then(Value::as_str).unwrap();
            let (method, uri) = split_req(req);
            let body = body_of(step);
            let mut rb = client.request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), format!("{base}{uri}"));
            for (k, v) in headers_of(step, body.is_some()) {
                rb = rb.header(k, v);
            }
            if let Some(b) = body {
                rb = rb.body(b);
            }
            let resp = rb.send().await.unwrap();
            let status = resp.status().as_u16();
            let mut hs = Map::new();
            let ct = resp.headers().get("content-type").map(|v| v.to_str().unwrap_or("").to_string());
            if let Some(c) = &ct {
                hs.insert("content-type".into(), Value::from(c.clone()));
            }
            let warnings: Vec<Value> = resp.headers().get_all("warning").iter().map(|v| Value::from(v.to_str().unwrap_or("").to_string())).collect();
            if !warnings.is_empty() {
                hs.insert("warning".into(), Value::Arr(warnings));
            }
            if let Some(a) = resp.headers().get("allow") {
                hs.insert("allow".into(), Value::from(a.to_str().unwrap_or("").to_string()));
            }
            let bytes = resp.bytes().await.unwrap();
            let mut out = Map::new();
            if let Value::Obj(m) = step {
                for (k, v) in m {
                    if !matches!(k.as_str(), "status" | "response" | "text" | "response_headers") {
                        out.insert(k.clone(), v.clone());
                    }
                }
            }
            out.insert("status".into(), Value::from(status as i64));
            let is_json = ct.as_deref().is_some_and(|c| c.contains("json"));
            match (is_json, parse(&bytes)) {
                (true, Ok(Some(v))) => {
                    out.insert("response".into(), v);
                }
                _ => {
                    out.insert("text".into(), Value::from(String::from_utf8_lossy(&bytes).into_owned()));
                }
            }
            out.insert("response_headers".into(), Value::Obj(hs));
            steps_out.push(Value::Obj(out));
        }
        cleanup(&client, &base).await;
        let mut sc = scenario.as_object().unwrap().clone();
        sc.insert("steps".into(), Value::Arr(steps_out));
        let text = Value::Obj(sc).to_pretty_json() + "\n";
        std::fs::write(dir().join(&file), text).unwrap();
        eprintln!("captured {file}");
    }
}

/// Deletes every `esm-*` object on the reference cluster.
async fn cleanup(client: &reqwest::Client, base: &str) {
    let get = |path: String| {
        let client = client.clone();
        async move {
            let r = client.get(path).send().await.ok()?;
            let b = r.bytes().await.ok()?;
            parse(&b).ok().flatten()
        }
    };
    let del = |path: String| {
        let client = client.clone();
        async move {
            let _ = client.delete(path).send().await;
        }
    };
    del(format!("{base}/_data_stream/esm*?expand_wildcards=all")).await;
    if let Some(Value::Arr(rows)) = get(format!("{base}/_cat/indices/esm*?format=json&h=index&expand_wildcards=all")).await {
        for r in rows {
            if let Some(i) = r.get("index").and_then(Value::as_str) {
                del(format!("{base}/{i}")).await;
            }
        }
    }
    if let Some(v) = get(format!("{base}/_index_template/esm*")).await {
        for t in v.get("index_templates").and_then(Value::as_array).cloned().unwrap_or_default() {
            if let Some(n) = t.get("name").and_then(Value::as_str) {
                del(format!("{base}/_index_template/{n}")).await;
            }
        }
    }
    if let Some(v) = get(format!("{base}/_component_template/esm*")).await {
        for t in v.get("component_templates").and_then(Value::as_array).cloned().unwrap_or_default() {
            if let Some(n) = t.get("name").and_then(Value::as_str) {
                del(format!("{base}/_component_template/{n}")).await;
            }
        }
    }
    if let Some(Value::Obj(m)) = get(format!("{base}/_template/esm*")).await {
        for n in m.keys() {
            del(format!("{base}/_template/{n}")).await;
        }
    }
    if let Some(Value::Obj(m)) = get(format!("{base}/_ingest/pipeline/esm*")).await {
        for n in m.keys() {
            del(format!("{base}/_ingest/pipeline/{n}")).await;
        }
    }
    if let Some(v) = get(format!("{base}/_synonyms?size=10000")).await {
        for r in v.get("results").and_then(Value::as_array).cloned().unwrap_or_default() {
            if let Some(n) = r.get("synonyms_set").and_then(Value::as_str).filter(|n| n.starts_with("esm")) {
                del(format!("{base}/_synonyms/{n}")).await;
            }
        }
    }
    if let Some(v) = get(format!("{base}/_cluster/state/metadata?filter_path=metadata.stored_scripts")).await
        && let Some(Value::Obj(m)) = v.pointer(&["metadata", "stored_scripts"])
    {
        for n in m.keys().filter(|n| n.starts_with("esm")) {
            del(format!("{base}/_scripts/{n}")).await;
        }
    }
}
