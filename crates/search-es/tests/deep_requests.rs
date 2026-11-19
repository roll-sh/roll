//! Requests whose nesting comes from something other than the body's own JSON nesting: ingest
//! processors and pipelines, grok patterns, dotted paths, scripts reading a deep `_source`, sort
//! and watcher definitions, YAML bodies. Each scenario runs on a 2 MiB thread, the size of a
//! tokio worker's stack: a stack overflow aborts every tenant on the node.

use search_es::json::{Value, parse};
use search_es::{Cluster, EsRequest};

fn nest(open: &str, close: &str, inner: &str, n: usize) -> String {
    format!("{}{inner}{}", open.repeat(n), close.repeat(n))
}

fn q(s: &str) -> String {
    Value::Str(s.to_string()).to_json()
}

/// Runs `(method, uri, body, content type)` in order on a 2 MiB stack; (status, body) of each.
fn run(reqs: Vec<(&'static str, String, String, &'static str)>) -> Vec<(u16, Value)> {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let c = Cluster::in_memory("deep").await;
                let mut out = Vec::new();
                for (m, uri, body, ct) in reqs {
                    let mut r = if body.is_empty() { EsRequest::new(m, &uri) } else { EsRequest::json(m, &uri, body) };
                    if !r.body.is_empty() {
                        r.headers = vec![("content-type".into(), ct.into())];
                    }
                    let resp = search_es::handle(&c, r).await;
                    // A YAML request is answered in YAML: its text then.
                    let body = parse(&resp.body).ok().flatten().unwrap_or_else(|| Value::Str(String::from_utf8_lossy(&resp.body).into_owned()));
                    out.push((resp.status, body));
                }
                out
            })
        })
        .unwrap()
        .join()
        .expect("no stack overflow")
}

fn json(m: &'static str, uri: &str, body: String) -> (&'static str, String, String, &'static str) {
    (m, uri.to_string(), body, "application/json")
}

fn reason(v: &Value) -> String {
    v.to_json()
}

fn simulate(pipeline: String, doc: String) -> (&'static str, String, String, &'static str) {
    json("POST", "/_ingest/pipeline/_simulate", format!(r#"{{"pipeline":{pipeline},"docs":[{{"_source":{doc}}}]}}"#))
}

/// Nested processors (`foreach`), deep `set` values and nested `pipeline` processors: at most
/// 100 pipelines nest (`es.ingest.max_pipelines`), as Elasticsearch's message says.
#[test]
fn ingest_nesting_fits_the_stack() {
    let foreach = format!(
        r#"{{"processors":[{}]}}"#,
        nest(r#"{"foreach":{"field":"a","ignore_missing":true,"processor":"#, "}}", r#"{"set":{"field":"b","value":1}}"#, 490)
    );
    let deep_value = format!(r#"{{"processors":[{{"set":{{"field":"b","value":{}}}}}]}}"#, nest("{\"a\":", "}", "1", 980));
    let mut reqs = vec![simulate(foreach, r#"{"a":"x"}"#.into()), simulate(deep_value, "{}".into())];
    for i in 0..300 {
        reqs.push(json("PUT", &format!("/_ingest/pipeline/p{i}"), format!(r#"{{"processors":[{{"pipeline":{{"name":"p{}","ignore_missing_pipeline":true}}}}]}}"#, i + 1)));
    }
    reqs.push(json("PUT", "/i/_doc/1?pipeline=p0", r#"{"a":1}"#.into()));
    reqs.push(json("PUT", "/i/_doc/2?pipeline=p200", r#"{"a":1}"#.into()));
    let r = run(reqs);
    assert_eq!(r[0].0, 200, "{}", reason(&r[0].1));
    assert_eq!(r[1].0, 200, "{}", reason(&r[1].1));
    let (s, v) = &r[302];
    assert!(*s >= 400 && reason(v).contains("Too many nested pipelines. Cannot have more than 100 nested pipelines"), "{s} {}", reason(v));
    // p200 … p299: 100 pipelines.
    assert_eq!(r[303].0, 201, "{}", reason(&r[303].1));
}

/// Grok compiles its expanded pattern on the caller's thread: groups nest at most 1000 deep
/// (Oniguruma's parse depth error), and a deep chain of pattern definitions expands into them.
#[test]
fn grok_patterns_bound_their_nesting() {
    let grok = |patterns: String, defs: String| simulate(format!(r#"{{"processors":[{{"grok":{{"field":"a","patterns":[{patterns}],"pattern_definitions":{{{defs}}}}}}}]}}"#), r#"{"a":"x"}"#.into());
    let chain: Vec<String> = (0..5000).map(|i| format!(r#""P{i}":"(%{{P{}}})""#, i + 1)).collect();
    let r = run(vec![
        grok(q(&nest("(", ")", "x", 20_000)), String::new()),
        grok(q("%{P0}"), format!("{},\"P5000\":\"x\"", chain.join(","))),
        grok(q(&nest("(", ")", "x", 500)), String::new()),
    ]);
    for (s, v) in &r[..2] {
        assert!((400..500).contains(s) && reason(v).contains("parse depth limit over"), "{s} {}", reason(v));
    }
    assert_eq!(r[2].0, 200, "{}", reason(&r[2].1));
}

/// A dotted name or path spells out one object per part. Documents refuse names past 100
/// parts, ingest `set` and `dot_expander` paths past 1000 (a body's nesting), and mappings
/// build only the objects their depth validation needs to refuse the name, before building it.
#[test]
fn long_dotted_paths_are_refused_before_they_build() {
    let long = vec!["a"; 50_000].join(".");
    let r = run(vec![
        json("PUT", "/m", format!(r#"{{"mappings":{{"properties":{{{}:{{"type":"keyword"}}}}}}}}"#, q(&long))),
        json("PUT", "/d/_doc/1", format!(r#"{{{}:1}}"#, q(&long))),
        simulate(format!(r#"{{"processors":[{{"set":{{"field":{},"value":1}}}}]}}"#, q(&long)), "{}".into()),
        simulate(r#"{"processors":[{"dot_expander":{"field":"*"}}]}"#.into(), format!(r#"{{{}:1}}"#, q(&long))),
        // Within the limit the mapping's own depth validation answers.
        json("PUT", "/m2", format!(r#"{{"mappings":{{"properties":{{{}:{{"type":"keyword"}}}}}}}}"#, q(&vec!["a"; 900].join(".")))),
    ]);
    let depth = |v: &Value| reason(v).contains("Limit of mapping depth [20] has been exceeded due to object field [a.a.a.a.a.a.a.a.a.a.a.a.a.a.a.a.a.a.a.a]");
    assert!(r[0].0 == 400 && depth(&r[0].1), "{:?}", r[0]);
    assert!(r[1].0 == 400 && reason(&r[1].1).contains("has more than 100 parts"), "{:?}", r[1]);
    for x in &r[2..4] {
        assert!(reason(&x.1).contains("exceeds the maximum allowed (1000"), "{:?}", x);
    }
    assert!(r[4].0 == 400 && depth(&r[4].1), "{:?}", r[4]);
}

/// Scripts read a document's `_source` on the request's thread: as deep as a body may nest.
#[test]
fn scripts_read_deep_sources() {
    let deep = nest("{\"a\":", "}", "1", 985);
    let r = run(vec![
        simulate(r#"{"processors":[{"script":{"source":"ctx.n = 1"}}]}"#.into(), format!(r#"{{"o":{deep}}}"#)),
        json("PUT", "/s", r#"{"mappings":{"properties":{"o":{"type":"object","enabled":false}}}}"#.into()),
        json("PUT", "/s/_doc/1?refresh=true", format!(r#"{{"o":{deep}}}"#)),
        json("POST", "/s/_update/1", r#"{"script":{"source":"ctx._source.n = 3"}}"#.into()),
    ]);
    for (s, v) in &r {
        assert!(*s < 500, "{s} {}", reason(v));
    }
    assert_eq!(r[2].0, 201, "{}", reason(&r[2].1));
}

/// A sort's `nested` chain and a watch's `chain` inputs and transforms: chains nest at most
/// 16 levels (roll's own; running a watch recurses once per level).
#[test]
fn sort_and_watch_chains_fit_the_stack() {
    let watch = |transform: String| {
        json(
            "POST",
            "/_watcher/watch/_execute",
            format!(r#"{{"watch":{{"trigger":{{"schedule":{{"interval":"10h"}}}},"input":{{"simple":{{}}}},"transform":{transform},"actions":{{}}}}}}"#),
        )
    };
    let chain = |n| nest(r#"{"chain":["#, "]}", r#"{"script":"return 1"}"#, n);
    let r = run(vec![
        json("PUT", "/i/_doc/1?refresh=true", r#"{"n":1}"#.into()),
        json("POST", "/i/_search", format!(r#"{{"sort":[{{"n":{{"nested":{}}}}}]}}"#, nest(r#"{"path":"x","nested":"#, "}", r#"{"path":"x"}"#, 490))),
        watch(chain(490)),
        watch(chain(16)),
    ]);
    assert!(r[1].0 < 500, "{:?}", r[1]);
    assert!(r[2].0 == 400 && reason(&r[2].1).contains("chains nest deeper than 16 levels"), "{:?}", r[2]);
    assert_eq!(r[3].0, 200, "{}", reason(&r[3].1));
}

/// YAML bodies' flow collections nest at most 128 levels (the parser's recursion limit),
/// refused before parsing rather than after a scan that grows with the square of the depth.
#[test]
fn yaml_flow_nesting_is_bounded() {
    let t = std::time::Instant::now();
    let r = run(vec![
        ("POST", "/i/_search".into(), format!("query: {}", nest("{a: ", "}", "1", 100_000)), "application/yaml"),
        ("POST", "/i/_search".into(), format!("query: {{bool: {{filter: [{}]}}}}", nest("{bool: {filter: [", "]}}", "{match_all: {}}", 30)), "application/yaml"),
    ]);
    assert!(r[0].0 == 400 && reason(&r[0].1).contains("recursion limit exceeded"), "{:?}", r[0]);
    assert!(r[1].0 < 500 && !reason(&r[1].1).contains("recursion limit"), "{:?}", r[1]);
    assert!(t.elapsed() < std::time::Duration::from_secs(30), "{:?}", t.elapsed());
}
