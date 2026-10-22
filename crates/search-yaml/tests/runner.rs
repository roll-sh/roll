//! End-to-end: the runner against an in-process mock of a few Elasticsearch endpoints.
//!
//! The mock is deliberately tiny (a raw HTTP/1.1 loop over a document map) so the test checks the
//! runner's behaviour — request building, stash, catch, warnings, HEAD, ignore, prerequisites,
//! capabilities, product header, reports — rather than a server.

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use search_yaml::report::Status;
use search_yaml::runner::{Config, SuiteRoot, run};
use search_yaml::value::{Map, Value};
use search_yaml::wipe::WipeMode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type Docs = Arc<Mutex<HashMap<String, Value>>>;

struct Req {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn json(v: &str) -> Value {
    Value::from_json(v.as_bytes()).unwrap()
}

/// Returns (status, content type, body, extra headers).
fn handle(req: &Req, docs: &Docs) -> (u16, &'static str, String, Vec<(String, String)>) {
    let ok = |b: String| (200, "application/json", b, vec![]);
    let q = |k: &str| req.query.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let segs: Vec<String> = req.path.trim_start_matches('/').split('/').map(decode).collect();
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    match (req.method.as_str(), segs.as_slice()) {
        ("GET", [""]) => ok(r#"{"name":"mock","version":{"number":"9.5.5"},"tagline":"You Know, for Search"}"#.into()),
        ("GET", ["_nodes", "os,plugins"]) => ok(
            r#"{"nodes":{"n1":{"version":"9.5.5","os":{"pretty_name":"Mock OS"},"modules":[{"name":"x-pack-core"}]}}}"#.into(),
        ),
        ("GET", ["_cluster", "state"]) => ok(r#"{"nodes_features":[{"node_id":"n1","features":["mock_feature"]}]}"#.into()),
        ("GET", ["_capabilities"]) => {
            let supported = q("path").as_deref() == Some("/_mock") && q("capabilities").as_deref() != Some("nope");
            ok(format!(r#"{{"supported":{supported}}}"#))
        }
        ("GET", ["_cluster", "settings"]) => ok(r#"{"persistent":{},"transient":{}}"#.into()),
        ("GET", ["_snapshot", "_all"]) | ("GET", ["_ingest", "pipeline"]) | ("GET", ["_template"]) => ok("{}".into()),
        ("GET", ["_index_template"]) => ok(r#"{"index_templates":[]}"#.into()),
        ("GET", ["_component_template"]) => ok(r#"{"component_templates":[]}"#.into()),
        ("GET", ["_cluster", "state", "metadata"]) => ok("{}".into()),
        ("GET", ["_cluster", "pending_tasks"]) => ok(r#"{"tasks":[]}"#.into()),
        ("GET", ["_cluster", "health"]) | ("GET", ["_query", "view"]) => ok(r#"{"status":"green"}"#.into()),
        ("POST", ["_features", "_reset"]) => ok("{}".into()),
        ("DELETE", ["_data_stream", _]) => ok(r#"{"acknowledged":true}"#.into()),
        ("DELETE", [indices]) if indices.starts_with('*') => {
            docs.lock().unwrap().retain(|k, _| k == "__writes");
            ok(r#"{"acknowledged":true}"#.into())
        }
        ("PUT" | "POST", [index, "_doc", id]) => {
            let doc = Value::from_json(&req.body).unwrap();
            let key = format!("{index}/{id}");
            let created = docs.lock().unwrap().insert(key, doc).is_none();
            let result = if created { "created" } else { "updated" };
            (if created { 201 } else { 200 }, "application/json", format!(r#"{{"_index":"{index}","_id":"{id}","result":"{result}","_seq_no":3}}"#), vec![])
        }
        ("GET", [index, "_doc", id]) => match docs.lock().unwrap().get(&format!("{index}/{id}")) {
            Some(doc) => ok(format!(r#"{{"_index":"{index}","_id":"{id}","found":true,"_source":{}}}"#, doc.to_json())),
            None => (
                404,
                "application/json",
                format!(
                    r#"{{"error":{{"root_cause":[{{"type":"document_missing_exception","reason":"[{id}]: document missing"}}],"type":"document_missing_exception","reason":"[{id}]: document missing"}},"status":404}}"#
                ),
                vec![],
            ),
        },
        ("HEAD", [index]) => {
            let exists = docs.lock().unwrap().keys().any(|k| k.starts_with(&format!("{index}/")));
            (if exists { 200 } else { 404 }, "application/json", String::new(), vec![])
        }
        ("POST", ["_bulk"]) => {
            let text = String::from_utf8_lossy(&req.body);
            let lines = text.lines().filter(|l| !l.is_empty()).count();
            ok(format!(r#"{{"errors":false,"lines":{lines},"ends_with_newline":{}}}"#, text.ends_with('\n')))
        }
        ("GET", ["_cat", "health"]) => (200, "text/plain; charset=UTF-8", "1700000000 10:11:12 mock green\n".into(), vec![]),
        ("POST" | "GET", ["_echo"]) => {
            let mut m = Map::new();
            m.insert("method".into(), Value::Str(req.method.clone()));
            let qm: Map = req.query.iter().map(|(k, v)| (k.clone(), Value::Str(v.clone()))).collect();
            m.insert("query".into(), Value::Map(qm));
            let hm: Map = req.headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), Value::Str(v.clone()))).collect();
            m.insert("headers".into(), Value::Map(hm));
            m.insert("body".into(), Value::Str(String::from_utf8_lossy(&req.body).into_owned()));
            ok(Value::Map(m).to_json())
        }
        ("GET", ["_warn"]) => (
            200,
            "application/json",
            r#"{"ok":true}"#.into(),
            vec![("Warning".into(), "299 Elasticsearch-9.5.5-abcdef1 \"[x] is deprecated\"".into())],
        ),
        // A header ending in a lone quote (once a panic in the runner) and a 12-digit hash.
        ("GET", ["_badwarn"]) => (
            200,
            "application/json",
            r#"{"ok":true}"#.into(),
            vec![
                ("Warning".into(), "299 Elasticsearch-9.5.5-abcdef0 \"".into()),
                ("Warning".into(), "299 Elasticsearch-9.5.5-abcdef012345 \"x\"".into()),
            ],
        ),
        ("GET", ["_dup"]) => (200, "application/json", r#"{"status":"green","status":"red"}"#.into(), vec![]),
        ("GET", ["_noproduct"]) => (200, "application/json", "{}".into(), vec![("X-No-Product".into(), "1".into())]),
        ("GET", ["_fail"]) => (500, "application/json", r#"{"error":{"type":"x_exception","reason":"boom"},"status":500}"#.into(), vec![]),
        _ => (400, "application/json", format!(r#"{{"error":"no handler for {} {}","status":400}}"#, req.method, req.path), vec![]),
    }
}

async fn serve(listener: tokio::net::TcpListener, docs: Docs) {
    loop {
        let Ok((mut sock, _)) = listener.accept().await else { return };
        let docs = docs.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let head_end = loop {
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i;
                    }
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let mut lines = head.split("\r\n");
                let mut first = lines.next().unwrap().split(' ');
                let method = first.next().unwrap().to_owned();
                let target = first.next().unwrap().to_owned();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())))
                    .collect();
                let len: usize = headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .map(|(_, v)| v.parse().unwrap())
                    .unwrap_or(0);
                while buf.len() < head_end + 4 + len {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = buf[head_end + 4..head_end + 4 + len].to_vec();
                buf.drain(..head_end + 4 + len);
                let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                let query = query
                    .split('&')
                    .filter(|p| !p.is_empty())
                    .map(|p| {
                        let (k, v) = p.split_once('=').unwrap_or((p, ""));
                        (decode(k), decode(v))
                    })
                    .collect();
                let req = Req { method: method.clone(), path: path.to_owned(), query, headers, body };
                // Spawned mocks only: die like a server that panics on a request.
                if std::env::var("SEARCH_YAML_MOCK_CRASH").is_ok_and(|p| !p.is_empty() && req.path.contains(&p)) {
                    eprintln!("mock: crashing on {}", req.path);
                    std::process::exit(101);
                }
                if req.method == "PUT" || req.method == "POST" {
                    docs.lock().unwrap().entry("__writes".into()).and_modify(|v| *v = Value::Int(v.as_i64().unwrap_or(0) as i128 + 1)).or_insert(Value::Int(1));
                }
                let (status, ct, body, extra) = handle(&req, &docs);
                let mut resp = format!("HTTP/1.1 {status} X\r\ncontent-type: {ct}\r\ncontent-length: {}\r\n", body.len());
                if !extra.iter().any(|(k, _)| k == "X-No-Product") {
                    resp.push_str("X-Elastic-Product: Elasticsearch\r\n");
                }
                for (k, v) in extra {
                    resp.push_str(&format!("{k}: {v}\r\n"));
                }
                resp.push_str("\r\n");
                if method != "HEAD" {
                    resp.push_str(&body);
                }
                if sock.write_all(resp.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
    }
}

const SPEC: &[(&str, &str)] = &[
    ("_common.json", r#"{"params":{"error_trace":{},"filter_path":{},"pretty":{}}}"#),
    ("info.json", r#"{"info":{"url":{"paths":[{"path":"/","methods":["GET"]}]},"params":{}}}"#),
    (
        "index.json",
        r#"{"index":{"url":{"paths":[{"path":"/{index}/_doc/{id}","methods":["PUT","POST"],"parts":{"index":{},"id":{}}}]},"params":{"refresh":{}},"body":{"required":true}}}"#,
    ),
    ("get.json", r#"{"get":{"url":{"paths":[{"path":"/{index}/_doc/{id}","methods":["GET"],"parts":{"index":{},"id":{}}}]},"params":{}}}"#),
    ("indices.exists.json", r#"{"indices.exists":{"url":{"paths":[{"path":"/{index}","methods":["HEAD"],"parts":{"index":{}}}]},"params":{}}}"#),
    ("bulk.json", r#"{"bulk":{"url":{"paths":[{"path":"/_bulk","methods":["POST","PUT"]}]},"params":{"refresh":{}},"body":{"required":true}}}"#),
    ("cat.health.json", r#"{"cat.health":{"url":{"paths":[{"path":"/_cat/health","methods":["GET"]}]},"params":{"v":{}}}}"#),
    (
        "echo.json",
        r#"{"echo":{"url":{"paths":[{"path":"/_echo","methods":["GET","POST"]}]},"params":{"q":{},"list":{}},"body":{},"headers":{"accept":["application/json"],"content_type":["application/x-ndjson"]}}}"#,
    ),
    ("warn.json", r#"{"warn":{"url":{"paths":[{"path":"/_warn","methods":["GET"]}]},"params":{}}}"#),
    ("noproduct.json", r#"{"noproduct":{"url":{"paths":[{"path":"/_noproduct","methods":["GET"]}]},"params":{}}}"#),
    ("badwarn.json", r#"{"badwarn":{"url":{"paths":[{"path":"/_badwarn","methods":["GET"]}]},"params":{}}}"#),
    ("dup.json", r#"{"dup":{"url":{"paths":[{"path":"/_dup","methods":["GET"]}]},"params":{}}}"#),
    ("fail.json", r#"{"fail":{"url":{"paths":[{"path":"/_fail","methods":["GET"]}]},"params":{}}}"#),
];

const BASIC: &str = r#"---
setup:
  - do:
      index: { index: test, id: "1", body: { title: "hello", n: 5, f: 1.5 } }
---
teardown:
  - do:
      echo: { q: teardown }
---
"Index, get, set and stash":
  - do:
      get: { index: test, id: "1" }
  - match: { _id: "1" }
  - match: { _source.title: hello }
  - match: { _source: { title: hello, n: 5, f: 1.5 } }
  - set: { _source.n: n }
  - do:
      echo:
        q: "v${n}"
        body: { val: $n, key: "${n}", "$n": x }
  - match: { query.q: v5 }
  - match: { query.error_trace: "true" }
  - match: { body: '{"val":5,"key":"5","5":"x"}' }
  - match: { $body.method: POST }
  - is_true: headers.host
---
"Catch and error regex":
  - do:
      catch: missing
      get: { index: test, id: "2" }
  - match: { error.type: document_missing_exception }
  - do:
      catch: /document.missing/
      get: { index: test, id: "2" }
  - do:
      catch: request
      fail: {}
  - do:
      catch: param
      get: { index: test }
---
"HEAD and ignore":
  - do:
      indices.exists: { index: test }
  - is_true: ''
  - do:
      indices.exists: { index: nope }
  - is_false: ''
  - do:
      get: { index: test, id: "missing", ignore: 404 }
  - match: { status: 404 }
---
"Bulk NDJSON and text bodies":
  - do:
      bulk:
        refresh: true
        body:
          - index: { _index: test }
          - f: 1
          - '{"index": {}}'
          - '{"f": 2}'
  - match: { lines: 4 }
  - match: { ends_with_newline: true }
  - do:
      cat.health: { v: true }
  - match: { $body: "/^ \\d+ \\s+ \\d{2}:\\d{2}:\\d{2} \\s+ mock \\s+ green \\n $/" }
---
"Headers and lists":
  - requires:
      test_runner_features: [headers]
  - do:
      headers: { X-Test: "abc", Content-Type: "application/json" }
      echo: { list: [a, b, c], body: { x: 1 } }
  - match: { query.list: "a,b,c" }
  - match: { headers.x-test: abc }
  - match: { $request_headers.X-Test: abc }
  - match: { headers.content-type: application/json }
  - do:
      echo: { body: { x: 1 } }
  # As ClientYamlTestExecutionContext.createEntity: the media type plus `charset=UTF-8`.
  - match: { headers.content-type: "application/json; charset=UTF-8" }
---
"Warnings":
  - requires:
      test_runner_features: [warnings, allowed_warnings, warnings_regex]
  - do:
      warnings: ["[x] is deprecated"]
      warn: {}
  - do:
      allowed_warnings: ["[x] is deprecated"]
      warn: {}
  - do:
      warnings_regex: ["\\[x\\] is .*"]
      warn: {}
---
"Malformed warning headers fail":
  - requires:
      test_runner_features: [allowed_warnings]
  - do:
      allowed_warnings: ["x"]
      badwarn: {}
---
"Duplicate response fields fail":
  - do:
      dup: {}
  - match: { status: red }
---
"Unexpected warning fails":
  - do:
      warn: {}
---
"Missing product header fails":
  - do:
      noproduct: {}
---
"Failing match":
  - do:
      get: { index: test, id: "1" }
  - match: { _source.title: goodbye }
---
"Skipped by version":
  - skip:
      version: " - 9.9.99"
      reason: old
  - do:
      fail: {}
---
"Required feature present":
  - requires:
      cluster_features: [mock_feature, gte_v9.0.0]
      reason: x
  - do:
      info: {}
  - match: { version.number: 9.5.5 }
---
"Required feature missing":
  - requires:
      cluster_features: [other_feature]
      reason: x
  - do:
      fail: {}
---
"Capabilities":
  - requires:
      test_runner_features: [capabilities]
      capabilities:
        - method: GET
          path: /_mock
          capabilities: [yes_please]
      reason: x
  - skip:
      capabilities:
        - path: /_mock
          capabilities: [nope]
      reason: y
  - do:
      info: {}
---
"Capability missing":
  - requires:
      test_runner_features: [capabilities]
      capabilities:
        - path: /_other
      reason: x
  - do:
      fail: {}
"#;

#[tokio::test]
async fn runs_a_suite_against_a_mock() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve(listener, Arc::new(Mutex::new(HashMap::new()))));

    let dir = tempfile::tempdir().unwrap();
    let api = dir.path().join("api");
    std::fs::create_dir_all(&api).unwrap();
    for (name, body) in SPEC {
        std::fs::write(api.join(name), body).unwrap();
    }
    let tests = dir.path().join("test").join("mock");
    std::fs::create_dir_all(&tests).unwrap();
    std::fs::write(tests.join("10_basic.yml"), BASIC).unwrap();
    std::fs::write(tests.join("20_broken.yml"), "\"t\":\n  - nope: {}\n").unwrap();
    // elasticsearch-clients-tests files (a top-level `requires`) get the clients' semantics.
    std::fs::write(
        tests.join("40_clients.yml"),
        r#"---
requires:
  serverless: true
  stack: true
---
"clients":
  - do:
      warn: {}
  - do:
      catch: document_missing_exception
      get: { index: test, id: "nope" }
  - do:
      echo: { zzz: "1", body: { a: 1 } }
  - match: { query.zzz: "1" }
  - set: { query.zzz: z }
  - match: { $body.query.zzz: "$z" }
  - match: { headers.accept: application/json }
  - match: { headers.content-type: application/x-ndjson }
  - do:
      index: { index: lists, id: "1", body: { tags: [x, y] } }
  - do:
      get: { index: lists, id: "1" }
  - set: { _source.tags.0: t0 }
  - match: { _source.tags: ["$t0", y] }
---
"clients catch checks the error type":
  - do:
      catch: some_other_exception
      get: { index: test, id: "nope" }
---
"clients still check expected warnings":
  - do:
      warnings: ["something else"]
      warn: {}
"#,
    )
    .unwrap();
    // The same calls in Elasticsearch format follow the Java runner.
    std::fs::write(tests.join("50_strict.yml"), "\"unknown param\":\n  - do:\n      echo: { zzz: \"1\" }\n\"unknown catch\":\n  - do:\n      catch: document_missing_exception\n      get: { index: test, id: \"nope\" }\n\"no accept\":\n  - do:\n      echo: { body: { a: 1 } }\n  - is_false: headers.accept\n  - match: { headers.content-type: \"application/json; charset=UTF-8\" }\n").unwrap();
    // A failing teardown step doesn't fail the test (the Java runner never checks it).
    std::fs::write(
        tests.join("30_teardown.yml"),
        "---\nteardown:\n  - do:\n      catch: missing\n      info: {}\n---\n\"t\":\n  - do: { info: {} }\n",
    )
    .unwrap();

    let cfg = Config {
        url: Some(format!("http://127.0.0.1:{port}")),
        suites: vec![SuiteRoot { label: "m".into(), path: dir.path().join("test") }],
        api_spec: api,
        wipe: Some(WipeMode::Full),
        ..Config::default()
    };
    let report = run(cfg).await.unwrap();
    let status: HashMap<&str, (Status, String)> = report
        .tests
        .iter()
        .map(|t| {
            let detail = t
                .failure
                .as_ref()
                .map(|f| f.message.clone())
                .or_else(|| t.skip_reason.clone())
                .unwrap_or_default();
            (t.id.as_str(), (t.status, detail))
        })
        .collect();
    let expect = |name: &str, want: Status, needle: &str| {
        let id = format!("m:mock/10_basic/{name}");
        let (got, detail) = status.get(id.as_str()).unwrap_or_else(|| panic!("no result for {id}: {status:?}"));
        assert_eq!(*got, want, "{id}: {detail}");
        assert!(detail.contains(needle), "{id}: [{detail}] should contain [{needle}]");
    };
    expect("Index, get, set and stash", Status::Pass, "");
    expect("Catch and error regex", Status::Pass, "");
    expect("HEAD and ignore", Status::Pass, "");
    expect("Bulk NDJSON and text bodies", Status::Pass, "");
    expect("Headers and lists", Status::Pass, "");
    expect("Warnings", Status::Pass, "");
    expect("Unexpected warning fails", Status::Fail, "got unexpected warning header");
    expect("Malformed warning headers fail", Status::Fail, "got unmatched warning headers [\n\t299 Elasticsearch-9.5.5-abcdef0 \"\n\t299 Elasticsearch-9.5.5-abcdef012345 \"x\"\n]");
    expect("Duplicate response fields fail", Status::Fail, "Duplicate field 'status'");
    expect("Missing product header fails", Status::Fail, "X-Elastic-Product");
    expect("Failing match", Status::Fail, "field [_source.title] does not match the expected value");
    expect("Skipped by version", Status::Skip, "old");
    expect("Required feature present", Status::Pass, "");
    expect("Required feature missing", Status::Skip, "other_feature");
    expect("Capabilities", Status::Pass, "");
    expect("Capability missing", Status::Skip, "/_other");
    let broken = status.get("m:mock/20_broken/(file)").expect("parse failure reported");
    assert_eq!(broken.0, Status::Fail);
    assert!(broken.1.contains("unknown section [nope]"), "{}", broken.1);

    let st = |id: &str| report.tests.iter().find(|t| t.id == id).unwrap_or_else(|| panic!("no {id}"));
    let c = st("m:mock/40_clients/clients");
    assert_eq!(c.status, Status::Pass, "{:?}", c.failure);
    let c = st("m:mock/40_clients/clients catch checks the error type");
    assert!(c.failure.as_ref().unwrap().message.contains("[some_other_exception]"), "{:?}", c.failure);
    let c = st("m:mock/40_clients/clients still check expected warnings");
    assert!(c.failure.as_ref().unwrap().message.contains("did not get expected warning header"), "{:?}", c.failure);
    let c = st("m:mock/50_strict/unknown param");
    assert!(c.failure.as_ref().unwrap().message.contains("path/param [zzz] not supported by [echo] api"), "{:?}", c.failure);
    let c = st("m:mock/50_strict/unknown catch");
    assert!(c.failure.as_ref().unwrap().message.contains("catch value [document_missing_exception] not supported"), "{:?}", c.failure);
    let c = st("m:mock/50_strict/no accept");
    assert_eq!(c.status, Status::Pass, "{:?}", c.failure);

    let td = report.tests.iter().find(|t| t.id == "m:mock/30_teardown/t").unwrap();
    assert_eq!(td.status, Status::Pass, "{:?}", td.failure);
    let tf = td.teardown_failure.as_ref().expect("teardown failure recorded");
    assert!(tf.message.starts_with("expected [404] status code but api [info] returned [200"), "{}", tf.message);

    let failing = report.tests.iter().find(|t| t.name == "Failing match").unwrap();
    let f = failing.failure.as_ref().unwrap();
    assert_eq!(f.line, BASIC.lines().position(|l| l.contains("_source.title: goodbye")).unwrap() + 1);
    assert_eq!(f.actual, Some(Value::Str("hello".into())));
    assert_eq!(f.expected, Some(json(r#""goodbye""#)));
    assert!(f.response.as_deref().unwrap().starts_with("200 "));
    // Causes group failures: the API whose response an assertion rejected, or the API and the
    // error it returned (digits folded).
    assert_eq!(f.cause.as_deref(), Some("get: match mismatch"));
    let c = st("m:mock/40_clients/clients catch checks the error type");
    assert_eq!(
        c.failure.as_ref().unwrap().cause.as_deref(),
        Some("get → 404 document_missing_exception: [nope]: document missing")
    );
    let c = st("m:mock/50_strict/unknown param");
    assert_eq!(c.failure.as_ref().unwrap().cause.as_deref(), Some("echo: no response"));
    assert!(report.summary(50).contains("top failure causes:"));

    let summary = report.summary(10);
    assert!(summary.contains("m:mock"), "{summary}");
    let out = dir.path().join("report.json");
    report.write_json(&out).unwrap();
    let parsed = Value::from_json(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(parsed.get("totals").unwrap().get("total"), Some(&Value::Int(report.tests.len() as i128)));
    assert!(Path::new(&out).exists());
}

/// A cluster hook that starts a fresh mock per `POST` and counts deletions.
async fn hook_server(listener: tokio::net::TcpListener, created: Arc<Mutex<Vec<String>>>, deleted: Arc<Mutex<Vec<String>>>) {
    loop {
        let Ok((mut sock, _)) = listener.accept().await else { return };
        let created = created.clone();
        let deleted = deleted.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let head_end = loop {
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i;
                    }
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                    .unwrap_or(0);
                while buf.len() < head_end + 4 + len {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = String::from_utf8_lossy(&buf[head_end + 4..head_end + 4 + len]).into_owned();
                buf.drain(..head_end + 4 + len);
                let mut first = head.split(' ');
                let method = first.next().unwrap().to_owned();
                let path = first.next().unwrap().to_owned();
                // The hook's headers authenticate creation and deletion alike.
                assert!(head.to_ascii_lowercase().contains("\r\nauthorization: bearer hook-token"), "{method} {path} without the hook header");
                let resp = if method == "POST" && path == "/v1/clusters" {
                    assert!(body.contains("\"name\""), "{body}");
                    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let port = l.local_addr().unwrap().port();
                    tokio::spawn(serve(l, Arc::new(Mutex::new(HashMap::new()))));
                    let id = format!("c{port}");
                    let n = {
                        let mut c = created.lock().unwrap();
                        c.push(id.clone());
                        c.len()
                    };
                    // The accepted shapes: flat, nested under `cluster`, with a delete_url.
                    if n % 3 == 0 {
                        format!(r#"{{"delete_url":"/v1/clusters/{id}","url":"http://127.0.0.1:{port}","username":"u","password":"p"}}"#)
                    } else if n % 3 == 1 {
                        format!(r#"{{"id":"{id}","url":"http://127.0.0.1:{port}","username":"u","password":"p"}}"#)
                    } else {
                        format!(r#"{{"cluster":{{"id":"{id}","url":"http://127.0.0.1:{port}"}},"username":"u","password":"p"}}"#)
                    }
                } else if method == "DELETE" {
                    deleted.lock().unwrap().push(path.trim_start_matches("/v1/clusters/").to_owned());
                    "{}".to_owned()
                } else {
                    panic!("unexpected hook request {method} {path}");
                };
                let out = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{resp}", resp.len());
                if sock.write_all(out.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_files_on_fresh_clusters() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let created = Arc::new(Mutex::new(Vec::new()));
    let deleted = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(hook_server(listener, created.clone(), deleted.clone()));

    let dir = tempfile::tempdir().unwrap();
    let api = dir.path().join("api");
    std::fs::create_dir_all(&api).unwrap();
    for (name, body) in SPEC {
        std::fs::write(api.join(name), body).unwrap();
    }
    let tests = dir.path().join("test");
    std::fs::create_dir_all(&tests).unwrap();
    // Every file writes the same document id: they only all pass if each file has its own cluster.
    let file = "\"isolated\":\n  - do:\n      catch: missing\n      get: { index: t, id: \"1\" }\n  - do:\n      index: { index: t, id: \"1\", body: { a: 1 } }\n  - match: { result: created }\n  - do:\n      echo: {}\n  - match: { headers.authorization: \"Basic dTpw\" }\n";
    for i in 0..8 {
        std::fs::write(tests.join(format!("{i:02}_file.yml")), file).unwrap();
    }
    let cfg = Config {
        suites: vec![SuiteRoot { label: String::new(), path: tests }],
        api_spec: api,
        jobs: 4,
        cluster_hook: Some(search_yaml::runner::ClusterHook {
            create_url: format!("http://127.0.0.1:{port}/v1/clusters"),
            headers: vec![("Authorization".into(), "Bearer hook-token".into())],
        }),
        // No wipe at all: isolation comes from the fresh clusters.
        wipe: Some(WipeMode::None),
        ..Config::default()
    };
    let report = run(cfg).await.unwrap();
    assert_eq!(report.totals.total, 8);
    for t in &report.tests {
        assert_eq!(t.status, Status::Pass, "{}: {:?}", t.id, t.failure);
    }
    // One probe cluster plus one per file, all deleted.
    let mut c = created.lock().unwrap().clone();
    let mut d = deleted.lock().unwrap().clone();
    c.sort();
    d.sort();
    assert_eq!(c.len(), 9);
    assert_eq!(c, d);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn several_shared_clusters_one_worker_each() {
    // Two independent clusters given as --url a --url b: files are spread over both, each
    // cluster wiped between tests like a single shared one.
    let mut urls = Vec::new();
    let mut stores = Vec::new();
    for _ in 0..2 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        urls.push(format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port()));
        let docs: Docs = Arc::new(Mutex::new(HashMap::new()));
        stores.push(docs.clone());
        tokio::spawn(serve(listener, docs));
    }
    let dir = tempfile::tempdir().unwrap();
    let api = dir.path().join("api");
    std::fs::create_dir_all(&api).unwrap();
    for (name, body) in SPEC {
        std::fs::write(api.join(name), body).unwrap();
    }
    let tests = dir.path().join("test");
    std::fs::create_dir_all(&tests).unwrap();
    let file = "\"t\":\n  - do:\n      catch: missing\n      get: { index: t, id: \"1\" }\n  - do:\n      index: { index: t, id: \"1\", body: { a: 1 } }\n  - match: { result: created }\n";
    for i in 0..8 {
        std::fs::write(tests.join(format!("{i:02}_file.yml")), file).unwrap();
    }
    let cfg = Config {
        url: Some(urls[0].clone()),
        more_urls: vec![urls[1].clone()],
        suites: vec![SuiteRoot { label: String::new(), path: tests }],
        api_spec: api,
        jobs: 2,
        ..Config::default()
    };
    let report = run(cfg).await.unwrap();
    assert_eq!(report.totals.passed, 8, "{:?}", report.tests.iter().map(|t| &t.failure).collect::<Vec<_>>());
    // Both clusters did work.
    for docs in &stores {
        let writes = docs.lock().unwrap().get("__writes").and_then(Value::as_i64).unwrap_or(0);
        assert!(writes > 0, "a cluster got no requests");
    }
}

/// Not a test: the server process `spawned_clusters_per_file` starts (the test binary re-run with
/// this filter). Serves the mock on `$SEARCH_YAML_MOCK_PORT` until killed and records its pid
/// and cluster name in `$SEARCH_YAML_MOCK_DIR`.
#[test]
#[ignore]
fn mock_server_process() {
    let Ok(port) = std::env::var("SEARCH_YAML_MOCK_PORT") else { return };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await.unwrap();
        if let Ok(dir) = std::env::var("SEARCH_YAML_MOCK_DIR") {
            let name = std::env::var("SEARCH_YAML_MOCK_NAME").unwrap_or_default();
            std::fs::write(Path::new(&dir).join(std::process::id().to_string()), name).unwrap();
        }
        serve(listener, Arc::new(Mutex::new(HashMap::new()))).await;
    });
}

/// Whether a process is gone (exited, or a zombie nobody has reaped yet).
#[cfg(target_os = "linux")]
fn process_gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with(['Z', 'X'])),
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawned_clusters_per_file() {
    let dir = tempfile::tempdir().unwrap();
    let pids = dir.path().join("pids");
    std::fs::create_dir_all(&pids).unwrap();
    let api = dir.path().join("api");
    std::fs::create_dir_all(&api).unwrap();
    for (name, body) in SPEC {
        std::fs::write(api.join(name), body).unwrap();
    }
    let tests = dir.path().join("test");
    std::fs::create_dir_all(&tests).unwrap();
    // As with the hook: the files only all pass if each one has its own cluster.
    let file = "\"isolated\":\n  - do:\n      catch: missing\n      get: { index: t, id: \"1\" }\n  - do:\n      index: { index: t, id: \"1\", body: { a: 1 } }\n  - match: { result: created }\n";
    for i in 0..6 {
        std::fs::write(tests.join(format!("{i:02}_file.yml")), file).unwrap();
    }
    // The cluster of 05_file dies on its first index request: only that file fails, and its
    // failure says the process exited and where its output went.
    std::fs::write(tests.join("05_file.yml"), file.replace("index: t,", "index: crash,")).unwrap();
    let exe = std::env::current_exe().unwrap();
    let command = format!(
        "SEARCH_YAML_MOCK_CRASH=/crash/ SEARCH_YAML_MOCK_PORT={{port}} SEARCH_YAML_MOCK_NAME={{name}} SEARCH_YAML_MOCK_DIR='{}' '{}' mock_server_process --exact --ignored --test-threads 1 --nocapture",
        pids.display(),
        exe.display()
    );
    let cfg = Config {
        suites: vec![SuiteRoot { label: String::new(), path: tests }],
        api_spec: api,
        jobs: 3,
        spawn: Some(search_yaml::SpawnCommand::new(command)),
        wipe: Some(WipeMode::None),
        ..Config::default()
    };
    let report = run(cfg).await.unwrap();
    assert_eq!(report.totals.total, 6);
    for t in &report.tests {
        if t.id.starts_with("05_file/") {
            assert_eq!(t.status, Status::Fail);
            let msg = &t.failure.as_ref().unwrap().message;
            assert!(msg.contains("the cluster process exited during the run"), "{msg}");
            let log = msg.rsplit_once("its output is in ").unwrap().1.trim_end_matches(']');
            assert!(std::fs::read_to_string(log).unwrap().contains("mock: crashing on /crash/"));
            std::fs::remove_file(log).unwrap();
        } else {
            assert_eq!(t.status, Status::Pass, "{}: {:?}", t.id, t.failure);
        }
    }
    assert!(report.target.url.starts_with("spawn: "), "{}", report.target.url);
    // A probe process plus one per file, each with its own name, all stopped.
    let started: Vec<(u32, String)> = std::fs::read_dir(&pids)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (e.file_name().to_string_lossy().parse().unwrap(), std::fs::read_to_string(e.path()).unwrap())
        })
        .collect();
    assert_eq!(started.len(), 7, "{started:?}");
    let names: std::collections::HashSet<&str> = started.iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(names.len(), 7, "{started:?}");
    #[cfg(target_os = "linux")]
    for (pid, _) in &started {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !process_gone(*pid) {
            assert!(std::time::Instant::now() < deadline, "process {pid} still runs");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
