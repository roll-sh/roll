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
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let c = Cluster::in_memory("deep").await;
                let mut out = Vec::new();
                for (m, uri, body) in reqs {
                    let r = if body.is_empty() { EsRequest::new(m, uri) } else { EsRequest::json(m, uri, body) };
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
