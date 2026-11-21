//! Reads a request makes of other documents on its reader's behalf go through the reader's
//! document and field level security, as its searches and gets do: a role whose query hides a
//! document, or whose `field_security` hides a field, hides it from these too.

use base64::Engine;
use search_es::json::{Value, parse};
use search_es::{Cluster, EsRequest};

async fn call(c: &Cluster, method: &str, uri: &str, body: &str, user: Option<&str>) -> (u16, Value) {
    let auth = user.map(|u| format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{u}-password-1"))));
    call_as(c, method, uri, body, auth).await
}

async fn call_as(c: &Cluster, method: &str, uri: &str, body: &str, auth: Option<String>) -> (u16, Value) {
    let mut req = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
    if let Some(a) = auth {
        req.headers.push(("Authorization".into(), a));
    }
    let r = search_es::handle(c, req).await;
    (r.status, parse(&r.body).ok().flatten().unwrap_or(Value::Null))
}

/// Index `p`: `1` is red (visible), `2` blue (hidden); `secret` is hidden on both. User `r` reads
/// `p` and `others` under that role.
async fn cluster(others: &str) -> Cluster {
    let c = Cluster::in_memory("rs").await;
    let setup = [
        ("PUT", "/p", r#"{"mappings":{"properties":{"team":{"type":"keyword"},"secret":{"type":"keyword"},"v":{"type":"dense_vector","dims":2,"similarity":"l2_norm"},"shape":{"type":"geo_shape"}}}}"#),
        ("PUT", "/p/_doc/1?refresh=true", r#"{"team":"red","secret":"s1","v":[1,1],"shape":{"type":"point","coordinates":[1,1]}}"#),
        ("PUT", "/p/_doc/2?refresh=true", r#"{"team":"blue","secret":"s2","v":[2,2],"shape":{"type":"envelope","coordinates":[[1.5,2.5],[2.5,1.5]]}}"#),
    ];
    for (m, u, b) in setup {
        assert!(call(&c, m, u, b, None).await.0 < 300, "{m} {u}");
    }
    let role = format!(
        r#"{{"indices":[{{"names":["p"],"privileges":["read"],"query":{{"term":{{"team":"red"}}}},"field_security":{{"grant":["team","shape"]}}}},{{"names":[{others}],"privileges":["read"]}}]}}"#
    );
    assert_eq!(call(&c, "PUT", "/_security/role/r", &role, None).await.0, 200);
    assert_eq!(call(&c, "PUT", "/_security/user/r", r#"{"password":"r-password-1","roles":["r"]}"#, None).await.0, 200);
    c
}

fn hits(v: &Value) -> usize {
    v.pointer(&["hits", "hits"]).and_then(Value::as_array).map_or(0, Vec::len)
}

/// `_explain` answers 404 for a hidden document, leaves hidden fields out of `_source`, and
/// sees them as unmapped in its query.
#[tokio::test]
async fn explain_reads_as_its_reader() {
    let c = cluster(r#""none""#).await;
    let (s, v) = call(&c, "POST", "/p/_explain/2?_source=true", r#"{"query":{"match_all":{}}}"#, Some("r")).await;
    assert_eq!(s, 404, "a hidden document: {}", v.to_json());
    assert!(!v.to_json().contains("s2"), "{}", v.to_json());
    let (s, v) = call(&c, "POST", "/p/_explain/1?_source=true", r#"{"query":{"match_all":{}}}"#, Some("r")).await;
    assert_eq!(s, 200, "{}", v.to_json());
    assert!(!v.to_json().contains("s1"), "a hidden field in _source: {}", v.to_json());
    let (_, v) = call(&c, "POST", "/p/_explain/1", r#"{"query":{"term":{"secret":"s1"}}}"#, Some("r")).await;
    assert_eq!(v.get("matched"), Some(&Value::Bool(false)), "a query on a hidden field: {}", v.to_json());
}

/// A `percolate` query's, an `indexed_shape`'s and a knn `lookup`'s document is read as a get
/// by the reader would read it.
#[tokio::test]
async fn query_lookups_read_as_their_reader() {
    let c = cluster(r#""q","g""#).await;
    for (m, u, b) in [
        ("PUT", "/q", r#"{"mappings":{"properties":{"query":{"type":"percolator"},"secret":{"type":"keyword"},"team":{"type":"keyword"}}}}"#),
        ("PUT", "/q/_doc/1?refresh=true", r#"{"query":{"term":{"secret":"s1"}}}"#),
        ("PUT", "/g", r#"{"mappings":{"properties":{"loc":{"type":"geo_shape"},"v":{"type":"dense_vector","dims":2,"similarity":"l2_norm"}}}}"#),
        ("PUT", "/g/_doc/1?refresh=true", r#"{"loc":{"type":"point","coordinates":[2,2]},"v":[2,2]}"#),
    ] {
        assert!(call(&c, m, u, b, None).await.0 < 300, "{m} {u}");
    }
    // The stored query matches the document's hidden field only.
    let (s, v) = call(&c, "POST", "/q/_search", r#"{"query":{"percolate":{"field":"query","index":"p","id":"1"}}}"#, None).await;
    assert_eq!((s, hits(&v)), (200, 1), "{}", v.to_json());
    let (s, v) = call(&c, "POST", "/q/_search", r#"{"query":{"percolate":{"field":"query","index":"p","id":"1"}}}"#, Some("r")).await;
    assert_eq!(hits(&v), 0, "percolated a hidden field ({s}): {}", v.to_json());
    // The shape of a hidden document.
    let shape = r#"{"query":{"geo_shape":{"loc":{"indexed_shape":{"index":"p","id":"2","path":"shape"}}}}}"#;
    let (s, v) = call(&c, "POST", "/g/_search", shape, None).await;
    assert_eq!((s, hits(&v)), (200, 1), "{}", v.to_json());
    let (s, v) = call(&c, "POST", "/g/_search", shape, Some("r")).await;
    assert!(s >= 400 && hits(&v) == 0, "used a hidden document's shape ({s}): {}", v.to_json());
    // A hidden vector as the query vector.
    let knn = r#"{"knn":{"field":"v","k":1,"num_candidates":5,"query_vector_builder":{"lookup":{"index":"p","id":"1","path":"v"}}}}"#;
    let (s, v) = call(&c, "POST", "/g/_search", knn, None).await;
    assert_eq!((s, hits(&v)), (200, 1), "{}", v.to_json());
    let (s, v) = call(&c, "POST", "/g/_search", knn, Some("r")).await;
    assert!(s >= 400 && hits(&v) == 0, "used a hidden field's vector ({s}): {}", v.to_json());
}

/// A `lookup` runtime field reads its target index as the search's reader.
#[tokio::test]
async fn lookup_runtime_fields_read_as_their_reader() {
    let c = cluster(r#""src""#).await;
    for (m, u, b) in [
        ("PUT", "/src", r#"{"mappings":{"properties":{"k":{"type":"keyword"}}}}"#),
        ("PUT", "/src/_doc/1?refresh=true", r#"{"k":"red"}"#),
        ("PUT", "/src/_doc/2?refresh=true", r#"{"k":"blue"}"#),
    ] {
        assert!(call(&c, m, u, b, None).await.0 < 300, "{m} {u}");
    }
    let body = r#"{"sort":["k"],"runtime_mappings":{"l":{"type":"lookup","target_index":"p","input_field":"k","target_field":"team","fetch_fields":["secret","team"]}},"fields":["l"]}"#;
    let (_, v) = call(&c, "POST", "/src/_search", body, None).await;
    assert!(v.to_json().contains("s1") && v.to_json().contains("s2"), "{}", v.to_json());
    let (s, v) = call(&c, "POST", "/src/_search", body, Some("r")).await;
    let text = v.to_json();
    assert_eq!(s, 200, "{text}");
    assert!(text.contains("\"red\""), "the visible document's visible field: {text}");
    assert!(!text.contains("s1") && !text.contains("s2") && !text.contains(r#"{"team":["blue"]}"#), "read around the reader's security: {text}");
}

/// Queries built outside the search's query phase read as its reader too: `_validate` with
/// `explain` and a `filter` aggregation, through an `indexed_shape` of a hidden document.
#[tokio::test]
async fn validate_and_aggregations_read_as_their_reader() {
    let c = cluster(r#""g""#).await;
    for (m, u, b) in [
        ("PUT", "/g", r#"{"mappings":{"properties":{"loc":{"type":"geo_shape"}}}}"#),
        ("PUT", "/g/_doc/1?refresh=true", r#"{"loc":{"type":"point","coordinates":[2,2]}}"#),
    ] {
        assert!(call(&c, m, u, b, None).await.0 < 300, "{m} {u}");
    }
    let shape = r#"{"geo_shape":{"loc":{"indexed_shape":{"index":"p","id":"2","path":"shape"}}}}"#;
    let validate = format!(r#"{{"query":{shape}}}"#);
    let (_, v) = call(&c, "POST", "/g/_validate/query?explain=true", &validate, None).await;
    assert!(v.to_json().contains("2.5"), "the superuser's explanation shows the shape: {}", v.to_json());
    let (_, v) = call(&c, "POST", "/g/_validate/query?explain=true", &validate, Some("r")).await;
    assert!(!v.to_json().contains("2.5"), "explained a hidden document's shape: {}", v.to_json());
    let agg = format!(r#"{{"size":0,"aggs":{{"f":{{"filter":{shape}}}}}}}"#);
    let (s, v) = call(&c, "POST", "/g/_search", &agg, None).await;
    assert_eq!((s, v.pointer(&["aggregations", "f", "doc_count"]).and_then(Value::as_i64)), (200, Some(1)), "{}", v.to_json());
    let (s, v) = call(&c, "POST", "/g/_search", &agg, Some("r")).await;
    assert_ne!((s, v.pointer(&["aggregations", "f", "doc_count"]).and_then(Value::as_i64)), (200, Some(1)), "counted through a hidden document: {}", v.to_json());
    // A restricted reader isn't served the superuser's cached answer to the same request.
    let terms = r#"{"size":0,"aggs":{"t":{"terms":{"field":"team"}}}}"#;
    let keys = |v: &Value| v.pointer(&["aggregations", "t", "buckets"]).and_then(Value::as_array).map(|b| b.iter().filter_map(|x| x.get("key").and_then(Value::as_str).map(str::to_string)).collect::<Vec<_>>());
    assert_eq!(keys(&call(&c, "POST", "/p/_search", terms, None).await.1), Some(vec!["blue".into(), "red".into()]));
    assert_eq!(keys(&call(&c, "POST", "/p/_search", terms, Some("r")).await.1), Some(vec!["red".to_string()]));
}

/// A runtime field named like the field `team` the role query reads: the request sees it, the
/// reader's document level security doesn't (Elasticsearch applies it to the shard's reader).
const SHADOW: &str = r#""runtime_mappings":{"team":{"type":"keyword","script":"emit('red')"}}"#;

fn global_count(v: &Value) -> Option<i64> {
    v.pointer(&["aggregations", "all", "doc_count"]).and_then(Value::as_i64)
}

/// A request's runtime fields don't reach its reader's document and field level security: one
/// shadowing the role query's field lets no hidden document through a search or its `global`
/// aggregation, and one without a script named like a hidden field reads nothing of it.
#[tokio::test]
async fn request_runtime_fields_dont_reach_read_security() {
    let c = cluster(r#""none""#).await;
    let search = format!(r#"{{{SHADOW},"query":{{"match_all":{{}}}},"aggs":{{"all":{{"global":{{}}}}}}}}"#);
    let (s, v) = call(&c, "POST", "/p/_search", &search, Some("r")).await;
    assert_eq!((s, hits(&v), global_count(&v)), (200, 1, Some(1)), "{}", v.to_json());
    let hidden = r#"{"runtime_mappings":{"secret":{"type":"keyword"}},"query":{"term":{"secret":"s1"}},"fields":["secret"]}"#;
    let (s, v) = call(&c, "POST", "/p/_search", hidden, None).await;
    assert_eq!((s, hits(&v)), (200, 1), "{}", v.to_json());
    let (s, v) = call(&c, "POST", "/p/_search", hidden, Some("r")).await;
    assert_eq!((s, hits(&v)), (200, 0), "a runtime field read a hidden field: {}", v.to_json());
}

/// An API key's role descriptors (what a browser or roll secrets is handed) are document level
/// security too: a request's runtime field doesn't shadow them, in a search, its `global`
/// aggregation or a top-level `knn` (the key's vectors aren't hidden).
#[tokio::test]
async fn request_runtime_fields_dont_reach_an_api_keys_role_descriptors() {
    let c = cluster(r#""none""#).await;
    assert_eq!(call(&c, "PUT", "/_security/user/o", r#"{"password":"o-password-1","roles":["superuser"]}"#, None).await.0, 200);
    let key = r#"{"name":"k","role_descriptors":{"red":{"indices":[{"names":["p"],"privileges":["read"],"query":{"term":{"team":"red"}}}]}}}"#;
    let (s, k) = call(&c, "POST", "/_security/api_key", key, Some("o")).await;
    assert_eq!(s, 200, "{}", k.to_json());
    let field = |f: &str| k.get(f).and_then(Value::as_str).unwrap_or_default().to_string();
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", field("id"), field("api_key")));
    let search = format!(r#"{{{SHADOW},"query":{{"match_all":{{}}}},"aggs":{{"all":{{"global":{{}}}}}}}}"#);
    let (s, v) = call_as(&c, "POST", "/p/_search", &search, Some(format!("ApiKey {encoded}"))).await;
    assert_eq!((s, hits(&v), global_count(&v)), (200, 1, Some(1)), "{}", v.to_json());
    let knn = format!(r#"{{{SHADOW},"knn":{{"field":"v","query_vector":[2,2],"k":2,"num_candidates":5}}}}"#);
    let (s, v) = call_as(&c, "POST", "/p/_search", &knn, Some(format!("ApiKey {encoded}"))).await;
    assert_eq!((s, hits(&v)), (200, 1), "{}", v.to_json());
}

/// A filtered alias's filter is not security: a request's runtime fields apply to it, as
/// Elasticsearch 9.5.5 applies them (a search through the alias with a runtime field shadowing
/// its filter's field reads every document).
#[tokio::test]
async fn request_runtime_fields_apply_to_alias_filters() {
    let c = cluster(r#""none""#).await;
    let alias = r#"{"actions":[{"add":{"index":"p","alias":"reds","filter":{"term":{"team":"red"}}}}]}"#;
    assert_eq!(call(&c, "POST", "/_aliases", alias, None).await.0, 200);
    let (s, v) = call(&c, "POST", "/reds/_search", r#"{"query":{"match_all":{}}}"#, None).await;
    assert_eq!((s, hits(&v)), (200, 1), "{}", v.to_json());
    let search = format!(r#"{{{SHADOW},"query":{{"match_all":{{}}}},"aggs":{{"all":{{"global":{{}}}}}}}}"#);
    let (s, v) = call(&c, "POST", "/reds/_search", &search, None).await;
    assert_eq!((s, hits(&v), global_count(&v)), (200, 2, Some(2)), "{}", v.to_json());
}
