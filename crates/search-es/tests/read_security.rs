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

/// A browser key's `filter` is its owner's document level security: the key's own bans
/// (expensive queries such as this `prefix`, scripts) don't refuse it, and a request runtime
/// field shadowing its field doesn't widen it.
#[tokio::test]
async fn a_browser_keys_filter_ignores_request_runtime_fields() {
    let c = cluster(r#""none""#).await;
    assert_eq!(call(&c, "PUT", "/_security/user/o", r#"{"password":"o-password-1","roles":["superuser"]}"#, None).await.0, 200);
    let key = r#"{"name":"page","metadata":{"roll_browser":{"limits":{"allow_scripts":true}}},"role_descriptors":{"roll_browser":{"indices":[{"names":["p"],"privileges":["read"],"query":{"prefix":{"team":"re"}}}]}}}"#;
    let (s, k) = call(&c, "POST", "/_security/api_key", key, Some("o")).await;
    assert_eq!(s, 200, "{}", k.to_json());
    let field = |f: &str| k.get(f).and_then(Value::as_str).unwrap_or_default().to_string();
    let auth = format!("ApiKey {}", base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", field("id"), field("api_key"))));
    // The key is a browser key: the same query in its request is refused.
    let (s, v) = call_as(&c, "POST", "/p/_search", r#"{"query":{"prefix":{"team":"re"}}}"#, Some(auth.clone())).await;
    assert_eq!(s, 400, "{}", v.to_json());
    let search = format!(r#"{{{SHADOW},"query":{{"match_all":{{}}}},"aggs":{{"all":{{"global":{{}}}}}}}}"#);
    let (s, v) = call_as(&c, "POST", "/p/_search", &search, Some(auth)).await;
    assert_eq!((s, hits(&v), global_count(&v)), (200, 1, Some(1)), "{}", v.to_json());
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

/// A search on a point in time reads under its reader's role as it is now, as Elasticsearch's
/// do: a role query or field restriction added after the point in time opened applies to its
/// next search, and a role that lost the index can't search it.
#[tokio::test]
async fn a_point_in_time_reads_under_its_readers_current_role() {
    let c = Cluster::in_memory("pit-rs").await;
    for (m, u, b) in [
        ("PUT", "/p", r#"{"mappings":{"properties":{"team":{"type":"keyword"},"secret":{"type":"keyword"}}}}"#),
        ("PUT", "/p/_doc/1?refresh=true", r#"{"team":"red","secret":"s1"}"#),
        ("PUT", "/p/_doc/2?refresh=true", r#"{"team":"blue","secret":"s2"}"#),
        ("PUT", "/_security/role/r", r#"{"indices":[{"names":["p"],"privileges":["read"]}]}"#),
        ("PUT", "/_security/user/r", r#"{"password":"r-password-1","roles":["r"]}"#),
    ] {
        assert!(call(&c, m, u, b, None).await.0 < 300, "{m} {u}");
    }
    let (s, v) = call(&c, "POST", "/p/_pit?keep_alive=1h", "", Some("r")).await;
    assert_eq!(s, 200, "{}", v.to_json());
    let search = format!(r#"{{"pit":{{"id":"{}","keep_alive":"1h"}}}}"#, v.get("id").and_then(Value::as_str).unwrap());
    let (s, v) = call(&c, "POST", "/_search", &search, Some("r")).await;
    assert_eq!((s, hits(&v)), (200, 2), "{}", v.to_json());
    let role = r#"{"indices":[{"names":["p"],"privileges":["read"],"query":{"term":{"team":"red"}},"field_security":{"grant":["team"]}}]}"#;
    assert_eq!(call(&c, "PUT", "/_security/role/r", role, None).await.0, 200);
    let (s, v) = call(&c, "POST", "/_search", &search, Some("r")).await;
    let text = v.to_json();
    assert_eq!((s, hits(&v)), (200, 1), "the role query added since: {text}");
    assert!(!text.contains("s1") && !text.contains("s2"), "a field removed since: {text}");
    assert_eq!(call(&c, "PUT", "/_security/role/r", r#"{"indices":[{"names":["other"],"privileges":["read"]}]}"#, None).await.0, 200);
    let (s, v) = call(&c, "POST", "/_search", &search, Some("r")).await;
    assert_eq!((s, v.pointer(&["error", "type"]).and_then(Value::as_str)), (403, Some("security_exception")), "{}", v.to_json());
}

/// A request a linked cluster sends for `user` over its cross-cluster key `auth`, with that
/// user's `remote_indices` on `p`.
async fn link_call(c: &Cluster, method: &str, uri: &str, body: &str, auth: &str, user: &str) -> (u16, Value) {
    let mut req = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
    req.headers.push(("Authorization".into(), auth.to_string()));
    let access = format!(r#"{{"cluster":"home","who":"user [{user}]","owner":"native:{user}","roles":[{{"r0":{{"cluster":[],"indices":[{{"names":["p"],"privileges":["read"]}}]}}}}]}}"#);
    req.headers.push(("x-roll-remote-access".into(), access));
    let r = search_es::handle(c, req).await;
    (r.status, parse(&r.body).ok().flatten().unwrap_or(Value::Null))
}

/// Two users of one linked cluster (one cross-cluster key) each have their own scrolls: one
/// opened for the first is the second's 404.
#[tokio::test]
async fn a_links_users_have_their_own_scrolls() {
    let c = cluster(r#""none""#).await;
    let (s, v) = call(&c, "POST", "/_security/cross_cluster/api_key", r#"{"name":"link","access":{"search":[{"names":["p"]}]}}"#, None).await;
    assert_eq!(s, 200, "{}", v.to_json());
    let auth = format!("ApiKey {}", v.get("encoded").and_then(Value::as_str).unwrap());
    let (s, v) = link_call(&c, "POST", "/p/_search?scroll=1m", r#"{"size":1}"#, &auth, "a").await;
    assert_eq!((s, hits(&v)), (200, 1), "{}", v.to_json());
    let page = format!(r#"{{"scroll":"1m","scroll_id":"{}"}}"#, v.get("_scroll_id").and_then(Value::as_str).unwrap());
    let (s, v) = link_call(&c, "POST", "/_search/scroll", &page, &auth, "b").await;
    assert_eq!(s, 404, "another user of the link: {}", v.to_json());
    let (s, v) = link_call(&c, "POST", "/_search/scroll", &page, &auth, "a").await;
    assert_eq!((s, hits(&v)), (200, 1), "{}", v.to_json());
}

/// OpenSearch's Search Relevance Workbench reads what its caller may read, as the plugin does:
/// a user with the Workbench's actions and read on `a` only can't make a click judgment from
/// `b`'s events, sample `ubi_queries`, reach `b` through a store search's terms lookup, or run an
/// experiment on `b` (it ends `ERROR`).
#[tokio::test]
async fn the_workbench_reads_as_its_caller() {
    let cfg = search_es::ClusterConfig { dialect: search_es::Dialect::OpenSearch, unsealed_secrets: true, ..search_es::ClusterConfig::new("srw") };
    let c = Cluster::open(cfg, std::sync::Arc::new(search_es::MemoryStore::new())).await.unwrap();
    let events = r#"{"mappings":{"properties":{"action_name":{"type":"keyword"},"query_id":{"type":"keyword"},"user_query":{"type":"keyword"},"event_attributes":{"properties":{"object":{"properties":{"object_id":{"type":"keyword"}}},"position":{"properties":{"ordinal":{"type":"integer"}}}}}}}}"#;
    let setup = [
        ("PUT", "/a/_doc/1?refresh=true", r#"{"t":"red shoe"}"#),
        ("PUT", "/b/_doc/1?refresh=true", r#"{"t":"red shoe"}"#),
        ("PUT", "/b-events", events),
        ("PUT", "/b-events/_doc/1?refresh=true", r#"{"query_id":"q","user_query":"red shoe","action_name":"impression","event_attributes":{"object":{"object_id":"1"},"position":{"ordinal":1}}}"#),
        ("PUT", "/ubi_queries/_doc/1?refresh=true", r#"{"query_id":"q","user_query":"red shoe"}"#),
        ("PUT", "/_plugins/_security/api/roles/srw", r#"{"cluster_permissions":["cluster:admin/opensearch/search_relevance/*"],"index_permissions":[{"index_patterns":["a"],"allowed_actions":["read"]}]}"#),
        ("PUT", "/_plugins/_security/api/internalusers/u", r#"{"password":"Srw-pass-1234!"}"#),
        ("PUT", "/_plugins/_security/api/rolesmapping/srw", r#"{"users":["u"]}"#),
    ];
    for (m, u, b) in setup {
        let (s, v) = call(&c, m, u, b, None).await;
        assert!(s < 300, "{m} {u}: {s} {v}");
    }
    let auth = Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("u:Srw-pass-1234!")));
    let as_u = |m: &'static str, u: &'static str, b: String| {
        let (c, auth) = (&c, auth.clone());
        async move { call_as(c, m, u, &b, auth).await }
    };
    const B: &str = "/_plugins/_search_relevance";
    let (s, v) = as_u("PUT", "/_plugins/_search_relevance/judgments", r#"{"name":"j","type":"UBI_JUDGMENT","clickModel":"coec","ubiEventsIndex":"b-events"}"#.into()).await;
    assert_eq!(s, 403, "b's events: {v}");
    let (s, v) = as_u("POST", "/_plugins/_search_relevance/query_sets", r#"{"name":"q","description":"d","sampling":"topn","querySetSize":5}"#.into()).await;
    assert_eq!(s, 403, "ubi_queries: {v}");
    let (s, v) = as_u("POST", "/_plugins/_search_relevance/query_sets/_search", r#"{"query":{"terms":{"t":{"index":"b","id":"1","path":"t"}}}}"#.into()).await;
    assert_eq!(s, 400, "a lookup into b through the store: {v}");
    // An experiment on b: made, then its searches (as u) are refused, and it ends ERROR.
    let id = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_else(|| panic!("{v}")).to_string();
    let qs = id(&as_u("PUT", "/_plugins/_search_relevance/query_sets", r#"{"name":"q","description":"d","sampling":"manual","querySetQueries":[{"queryText":"red shoe"}]}"#.into()).await.1, "query_set_id");
    let conf = id(&as_u("PUT", "/_plugins/_search_relevance/search_configurations", r#"{"name":"c","index":"b","query":"{\"query\":{\"match\":{\"t\":\"%SearchText%\"}}}"}"#.into()).await.1, "search_configuration_id");
    let j = id(&as_u("PUT", "/_plugins/_search_relevance/judgments", r#"{"name":"j","type":"IMPORT_JUDGMENT","judgmentRatings":[{"query":"red shoe","ratings":[{"docId":"1","rating":"3"}]}]}"#.into()).await.1, "judgment_id");
    let body = format!(r#"{{"querySetId":"{qs}","searchConfigurationList":["{conf}"],"judgmentList":["{j}"],"size":5,"type":"POINTWISE_EVALUATION"}}"#);
    let (s, v) = as_u("PUT", "/_plugins/_search_relevance/experiments", body).await;
    assert_eq!(s, 200, "{v}");
    let e = id(&v, "experiment_id");
    let (_, v) = call(&c, "GET", &format!("{B}/experiments/{e}"), "", None).await;
    assert_eq!(v.pointer(&["hits", "hits"]).and_then(Value::as_array).and_then(|h| h.first()).and_then(|h| h.pointer(&["_source", "status"])).and_then(Value::as_str), Some("ERROR"), "{v}");
}
