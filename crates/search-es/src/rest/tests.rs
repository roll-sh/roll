//! REST framing tests: headers, rendering, the dialect, and what the handler future must be.

use super::*;
use crate::cluster::Cluster;

async fn run(c: &Cluster, method: &str, uri: &str, body: Option<&str>) -> EsResponse {
    let req = match body {
        Some(b) => EsRequest::json(method, uri, b.to_string()),
        None => EsRequest::new(method, uri),
    };
    handle(c, req).await
}

fn text(r: &EsResponse) -> String {
    String::from_utf8_lossy(&r.body).into_owned()
}

#[tokio::test]
async fn pretty_filter_path_and_opaque_id() {
    let c = Cluster::in_memory("t").await;
    let mut req = EsRequest::new("GET", "/_cluster/health?filter_path=status,number_of_nodes&pretty");
    req.headers.push(("X-Opaque-Id".into(), "abc".into()));
    let r = handle(&c, req).await;
    assert_eq!(r.status, 200);
    assert_eq!(text(&r), "{\n  \"status\" : \"green\",\n  \"number_of_nodes\" : 1\n}\n");
    assert_eq!(r.header("X-Opaque-Id"), Some("abc"));
}

#[test]
fn handle_future_is_send() {
    fn assert_send<T: Send>(_: T) {}
    let c: &'static Cluster = Box::leak(Box::new(futures_lite_block_on_cluster()));
    assert_send(handle(c, EsRequest::new("GET", "/")));
}

fn futures_lite_block_on_cluster() -> Cluster {
    tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(Cluster::in_memory("send"))
}

#[tokio::test]
async fn opensearch_dialect() {
    let mut cfg = crate::cluster::ClusterConfig::new("os");
    cfg.dialect = crate::cluster::Dialect::OpenSearch;
    let c = Cluster::open(cfg, std::sync::Arc::new(crate::cluster::MemoryStore::new())).await.unwrap();
    let r = run(&c, "GET", "/?pretty=false", None).await;
    assert_eq!(r.header("X-elastic-product"), None);
    let b = r.body_json();
    assert_eq!(b.pointer(&["version", "distribution"]).and_then(Value::as_str), Some("opensearch"));
    assert_eq!(b.get("tagline").and_then(Value::as_str), Some(crate::version::OPENSEARCH_TAGLINE));
    // Errors carry no product header either.
    let r = run(&c, "GET", "/missing", None).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.header("X-elastic-product"), None);
}

#[tokio::test]
async fn head_reports_the_get_length() {
    let c = Cluster::in_memory("t").await;
    run(&c, "PUT", "/idx", None).await;
    let get = run(&c, "GET", "/idx", None).await;
    let head = run(&c, "HEAD", "/idx", None).await;
    assert!(head.head);
    assert_eq!(head.body.len(), get.body.len());
    let root = run(&c, "HEAD", "/", None).await;
    assert_eq!(root.status, 200);
    assert!(root.body.ends_with(b"}\n"), "GET / is pretty by default");
}

#[tokio::test]
async fn deprecation_warnings_and_product_origin() {
    let c = Cluster::in_memory("t").await;
    let r = run(&c, "PUT", "/.dotted", None).await;
    let w = r.header("Warning").unwrap();
    assert!(w.starts_with("299 Elasticsearch-9.5.5-"), "{w}");
    assert!(w.ends_with("\"Index [.dotted] name begins with a dot (.), which is deprecated, and will not be allowed in a future Elasticsearch version.\""));
    let mut req = EsRequest::new("PUT", "/.dotted2");
    req.headers.push(("X-elastic-product-origin".into(), "kibana".into()));
    let r = handle(&c, req).await;
    assert_eq!(r.header("Warning"), None);
}

#[tokio::test]
async fn stack_traces_name_the_index() {
    let c = Cluster::in_memory("t").await;
    let r = run(&c, "GET", "/nope/_settings?error_trace=true", None).await;
    let v = r.body_json();
    assert_eq!(
        v.pointer(&["error", "stack_trace"]).and_then(|s| s.as_str()),
        Some("[nope] org.elasticsearch.index.IndexNotFoundException: no such index [nope]")
    );
}

#[tokio::test]
async fn shared_nodes_report_synthetic_metrics() {
    // The default: nothing about the host (its disks, memory, load) reaches a tenant.
    let c = Cluster::in_memory("t").await;
    let v = run(&c, "GET", "/_nodes/stats/fs,os,process", None).await.body_json();
    let node = v.get("nodes").and_then(|n| n.as_object()).and_then(|n| n.values().next()).cloned().unwrap();
    assert_eq!(node.pointer(&["fs", "total", "total_in_bytes"]).and_then(|x| x.as_i64()), Some(1 << 40));
    assert_eq!(node.pointer(&["os", "mem", "total_in_bytes"]).and_then(|x| x.as_i64()), Some(1 << 30));
    assert_eq!(node.pointer(&["os", "mem", "used_in_bytes"]).and_then(|x| x.as_i64()), Some(0));
    // A dedicated node reports its own.
    let mut cfg = crate::cluster::ClusterConfig::new("t");
    cfg.host_metrics = true;
    let c = Cluster::open(cfg, std::sync::Arc::new(crate::cluster::MemoryStore::new())).await.unwrap();
    let v = run(&c, "GET", "/_nodes/stats/os", None).await.body_json();
    let node = v.get("nodes").and_then(|n| n.as_object()).and_then(|n| n.values().next()).cloned().unwrap();
    if cfg!(target_os = "linux") {
        assert_ne!(node.pointer(&["os", "mem", "total_in_bytes"]).and_then(|x| x.as_i64()), Some(1 << 30));
    }
}
