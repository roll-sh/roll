//! Deeply nested request bodies must never overflow the stack: a stack overflow aborts the
//! process, taking every tenant on the node with it. Each scenario runs on a 2 MiB thread, the
//! size of a tokio worker's stack, in whatever profile the tests are built with.

use search_es::json::{Value, parse};
use search_es::{Cluster, EsRequest};

fn nest(open: &str, close: &str, inner: &str, n: usize) -> String {
    let mut s = String::with_capacity((open.len() + close.len()) * n + inner.len());
    for _ in 0..n {
        s.push_str(open);
    }
    s.push_str(inner);
    for _ in 0..n {
        s.push_str(close);
    }
    s
}

/// Runs the requests in order on a 2 MiB stack; returns (status, body) of each.
fn run(reqs: Vec<(&'static str, &'static str, String)>) -> Vec<(u16, Value)> {
    run_in(search_es::Dialect::Elasticsearch, reqs.into_iter().map(|(m, u, b)| (m, u.to_string(), b)).collect())
}

/// [`run`] on a cluster of `dialect`.
fn run_in(dialect: search_es::Dialect, reqs: Vec<(&'static str, String, String)>) -> Vec<(u16, Value)> {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let mut cfg = search_es::ClusterConfig::new("deep");
                cfg.unsealed_secrets = true;
                cfg.dialect = dialect;
                let c = Cluster::open(cfg, std::sync::Arc::new(search_es::MemoryStore::new())).await.unwrap();
                let mut out = Vec::new();
                for (m, uri, body) in reqs {
                    let r = if body.is_empty() { EsRequest::new(m, &uri) } else { EsRequest::json(m, &uri, body) };
                    let resp = search_es::handle(&c, r).await;
                    let v = parse(&resp.body).ok().flatten().unwrap_or(Value::Null);
                    out.push((resp.status, v));
                }
                out
            })
        })
        .unwrap()
        .join()
        .expect("no stack overflow")
}

fn reason(v: &Value, path: &[&str]) -> String {
    v.pointer(path).and_then(Value::as_str).unwrap_or_default().to_string()
}

#[test]
fn values_nested_to_the_parser_limit() {
    let s = nest("{\"a\":", "}", "1", 990);
    let v = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            let v = parse(s.as_bytes()).unwrap().unwrap();
            let c = v.clone();
            assert!(c == v);
            assert_eq!(v.to_json().len(), s.len());
            let _ = v.to_pretty_json();
            let _ = v.to_es_string();
            let mut cbor = Vec::new();
            search_es::xcontent::cbor::write_cbor(&mut cbor, &v);
            assert!(search_es::xcontent::cbor::read_cbor(&cbor).unwrap() == v);
            true
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(v);
}

#[test]
fn deep_settings_and_meta() {
    let deep = nest("{\"a\":", "}", "1", 990);
    let r = run(vec![
        ("PUT", "/_cluster/settings", format!("{{\"persistent\":{deep}}}")),
        ("PUT", "/_ingest/pipeline/deep-p", format!("{{\"processors\":[],\"_meta\":{deep}}}")),
        ("GET", "/_ingest/pipeline/deep-p", String::new()),
        ("PUT", "/deep-i", format!("{{\"mappings\":{{\"_meta\":{deep}}}}}")),
        ("GET", "/deep-i/_mapping?pretty", String::new()),
        ("GET", "/_cluster/state?format=yaml", String::new()),
    ]);
    assert_eq!(r[0].0, 400);
    assert!(reason(&r[0].1, &["error", "reason"]).starts_with("persistent setting [a.a.a."), "{:?}", r[0]);
    assert_eq!(r.iter().skip(1).map(|x| x.0).collect::<Vec<_>>(), vec![200, 200, 200, 200, 200]);
}

#[test]
fn mapping_depth_is_checked_while_parsing() {
    // Elasticsearch: one level past the limit is caught by validation, two by the parser
    // (`MappingParserContext.incrementMappingObjectDepth`, objects only).
    let props = |n| format!("{{\"mappings\":{}}}", nest("{\"properties\":{\"a\":", "}}", "{\"type\":\"keyword\"}", n));
    let r = run(vec![("PUT", "/deep-a", props(20)), ("PUT", "/deep-b", props(21)), ("PUT", "/deep-c", props(22)), ("PUT", "/deep-d", props(400))]);
    assert_eq!(r[0].0, 200);
    assert_eq!(r[1].0, 400);
    assert_eq!(
        reason(&r[1].1, &["error", "reason"]),
        "Limit of mapping depth [20] has been exceeded due to object field [a.a.a.a.a.a.a.a.a.a.a.a.a.a.a.a.a.a.a.a]"
    );
    for x in &r[2..] {
        assert_eq!(x.0, 400);
        assert_eq!(reason(&x.1, &["error", "type"]), "mapper_parsing_exception");
        assert_eq!(reason(&x.1, &["error", "reason"]), "Failed to parse mapping: Limit of mapping depth [20] has been exceeded");
        assert_eq!(reason(&x.1, &["error", "caused_by", "type"]), "mapper_parsing_exception");
        assert_eq!(reason(&x.1, &["error", "caused_by", "reason"]), "Limit of mapping depth [20] has been exceeded");
    }
    // Nested objects don't count while parsing; the validation catches them.
    let nested = format!("{{\"mappings\":{{{}}}}}", nest("\"properties\":{\"a\":{\"type\":\"nested\",", "}}", "\"properties\":{}", 400));
    let r = run(vec![("PUT", "/deep-n", nested)]);
    assert_eq!(r[0].0, 400);
    assert!(reason(&r[0].1, &["error", "reason"]).starts_with("Limit of mapping depth [20] has been exceeded due to object field"), "{:?}", r[0]);
}

#[test]
fn deep_alias_filters() {
    let intervals = |n| {
        format!(
            "{{\"actions\":[{{\"add\":{{\"index\":\"deep-i\",\"alias\":\"deep-a{n}\",\"filter\":{{\"intervals\":{{\"f\":{}}}}}}}}}]}}",
            nest("{\"all_of\":{\"intervals\":[", "]}}", "{\"match\":{\"query\":\"x\"}}", n)
        )
    };
    let geo = format!(
        "{{\"actions\":[{{\"add\":{{\"index\":\"deep-i\",\"alias\":\"deep-g\",\"filter\":{{\"geo_shape\":{{\"f\":{{\"shape\":{{\"type\":\"polygon\",\"coordinates\":{}}}}}}}}}}}}}]}}",
        nest("[", "]", "1", 800)
    );
    let wkt = format!(
        "{{\"actions\":[{{\"add\":{{\"index\":\"deep-i\",\"alias\":\"deep-w\",\"filter\":{{\"geo_shape\":{{\"f\":{{\"shape\":\"{}\"}}}}}}}}}}]}}",
        nest("GEOMETRYCOLLECTION (", ")", "POINT (1 2)", 5000)
    );
    let r = run(vec![
        ("PUT", "/deep-i", String::new()),
        ("POST", "/_aliases", intervals(150)),
        ("POST", "/_aliases", intervals(330)),
        ("GET", "/_alias/deep-a150", String::new()),
        ("POST", "/_aliases", geo),
        ("POST", "/_aliases", wkt),
    ]);
    assert_eq!(r.iter().take(4).map(|x| x.0).collect::<Vec<_>>(), vec![200, 200, 200, 200]);
    assert_eq!(r[4].0, 400);
    assert_eq!(r[5].0, 400);
    assert_eq!(reason(&r[5].1, &["error", "caused_by", "reason"]), "java.text.ParseException: maximum nested depth of 1000 exceeded", "{:?}", r[5]);
}

fn chain(item: &str, sep: &str, n: usize) -> String {
    vec![item; n].join(sep)
}

/// The query languages bound how deep they nest (parentheses, prefix operators, the chains an
/// operator folds, subqueries and views): far past the limit is a 400, not a stack overflow,
/// and just inside it every step that walks the tree fits a worker's stack.
#[test]
fn query_languages_bound_their_nesting() {
    let q = |s: String| Value::Str(s).to_json();
    let esql = |s: String| ("POST", "/_query".to_string(), format!("{{\"query\":{}}}", q(s)));
    let sql = |s: String| ("POST", "/_sql".to_string(), format!("{{\"query\":{}}}", q(s)));
    let eql = |s: String| ("POST", "/ql/_eql/search".to_string(), format!("{{\"query\":{}}}", q(s)));
    let search = |kind: &str, s: String| ("POST", "/ql/_search".to_string(), format!("{{\"query\":{{\"{kind}\":{{\"query\":{}}}}}}}", q(s)));
    let parens = |n, inner: &str| nest("(", ")", inner, n);
    let far: Vec<(&'static str, String, String)> = vec![
        esql(format!("ROW a = {}", parens(20_000, "1"))),
        esql(format!("ROW a = {}true", "NOT ".repeat(20_000))),
        esql(format!("ROW a = {}1", "- ".repeat(20_000))),
        esql(format!("ROW a = {}", chain("1", " + ", 100_000))),
        // An `IN` list's items don't restart the count of the chain around them.
        esql(format!("FROM ql | WHERE {}", chain("n IN (1)", " AND ", 100_000))),
        esql(format!("FROM {}", nest("(FROM ", ")", "ql", 5_000))),
        esql(format!("ROW a = 1 | {}", nest("FORK (", ")", "LIMIT 1", 20_000))),
        esql(format!("ROW a = {}", chain("1", " + ", 301))),
        esql(format!("PROMQL index=ql step=1m {}", nest("sum(", ")", "x", 20_000))),
        sql(format!("SELECT {}", parens(20_000, "1"))),
        sql(format!("SELECT {}", chain("1", " + ", 100_000))),
        eql(format!("any where {}", parens(20_000, "true"))),
        eql(format!("any where {}", chain("n == 1", " and ", 100_000))),
        search("kql", format!("{}k:a", "not ".repeat(20_000))),
        search("query_string", parens(20_000, "a")),
        search("simple_query_string", parens(20_000, "a")),
    ];
    let near: Vec<(&'static str, String, String)> = vec![
        esql(format!("ROW a = {}", parens(290, "1"))),
        esql(format!("ROW a = {}true", "NOT ".repeat(290))),
        esql(format!("ROW a = {}", chain("1", " + ", 290))),
        esql(format!("ROW a = {}", parens(140, &chain("1", " + ", 140)))),
        sql(format!("SELECT {}", parens(240, "1"))),
        sql(format!("SELECT {}", chain("1", " + ", 240))),
        eql(format!("any where {}", parens(240, "true"))),
        search("kql", format!("{}k:a", "not ".repeat(290))),
        // Elasticsearch's own limits are inclusive, and its languages count nesting, not chains.
        esql(format!("ROW a = {}", parens(300, "1"))),
        esql(format!("ROW a = {}", chain("1", " + ", 300))),
        esql(format!("FROM ql | WHERE ({}) AND ({})", chain("n == 1", " OR ", 160), chain("k == \"a\"", " OR ", 160))),
        esql(format!("FROM ql | WHERE {} | LIMIT 1", chain("n == 1", " OR ", 299))),
        esql(format!("FROM ql | WHERE {}", parens(299, "MATCH(k, \"a\")"))),
        sql(format!("SELECT * FROM ql WHERE {}", chain("n = 1", " OR ", 251))),
        sql(format!("SELECT * FROM ql WHERE {}", chain("n = 1", " OR ", 900))),
        ("POST", "/_sql/translate".to_string(), format!("{{\"query\":{}}}", q(format!("SELECT * FROM ql WHERE {}", chain("n = 1", " OR ", 900))))),
        sql(format!("SELECT {}", parens(250, "1"))),
        eql(format!("any where {}", chain("n == 1", " or ", 251))),
        eql(format!("any where {}", parens(250, "true"))),
        search("query_string", parens(290, "a")),
        search("simple_query_string", parens(290, "a")),
    ];
    let mut reqs = vec![
        ("PUT", "/ql".to_string(), r#"{"mappings":{"properties":{"@timestamp":{"type":"date"},"k":{"type":"keyword"},"n":{"type":"long"},"event.category":{"type":"keyword"}}}}"#.to_string()),
        ("PUT", "/ql/_doc/1?refresh=true".to_string(), r#"{"@timestamp":"2026-01-01","k":"a","n":1,"event.category":"x"}"#.to_string()),
    ];
    // Views that run one another: 40 deep is refused, 10 deep runs.
    for i in (0..40).rev() {
        let from = if i == 39 { "ql".to_string() } else { format!("v{}", i + 1) };
        reqs.push(("PUT", format!("/_query/view/v{i}"), format!("{{\"query\":\"FROM {from}\"}}")));
    }
    let (n_far, n_near) = (far.len(), near.len());
    reqs.extend(far);
    reqs.extend(near);
    reqs.push(esql("FROM v0".into()));
    reqs.push(esql("FROM v30 | LIMIT 1".into()));
    let r = run_in(search_es::Dialect::Elasticsearch, reqs);
    let r = &r[42..];
    let too_deep = |v: &Value| {
        let t = v.to_json();
        t.contains("exceeded the maximum expression depth") || t.contains("deeper than") || t.contains("levels deep") || t.contains("chains more than")
    };
    for (i, (s, v)) in r[..n_far].iter().enumerate() {
        assert!((400..500).contains(s) && too_deep(v), "far #{i}: {s} {}", v.to_json());
    }
    for (i, (s, v)) in r[n_far..n_far + n_near].iter().enumerate() {
        assert!(!too_deep(v) && *s < 500, "near #{i}: {s} {}", v.to_json());
    }
    let views = &r[n_far + n_near..];
    assert!(views[0].0 == 400 && too_deep(&views[0].1), "{:?}", views[0]);
    assert_eq!(views[1].0, 200, "{}", views[1].1.to_json());

    // OpenSearch's PPL, in both its expression grammar and the `search` command's.
    let ppl = |s: String| ("POST", "/_plugins/_ppl".to_string(), format!("{{\"query\":{}}}", q(s)));
    let r = run_in(
        search_es::Dialect::OpenSearch,
        vec![
            ("PUT", "/ql".to_string(), r#"{"mappings":{"properties":{"n":{"type":"long"}}}}"#.to_string()),
            ppl(format!("source=ql | where {}", parens(20_000, "n = 1"))),
            ppl(format!("source=ql | eval x = {}", chain("n", " + ", 100_000))),
            ppl(format!("search source=ql {}n=1", "NOT ".repeat(20_000))),
            ppl(format!("source=ql | where {}", parens(290, "n = 1"))),
        ],
    );
    for (s, v) in &r[1..4] {
        assert!((400..500).contains(s) && too_deep(v), "{s} {}", v.to_json());
    }
    assert!(!too_deep(&r[4].1) && r[4].0 < 500, "{:?}", r[4]);
}
