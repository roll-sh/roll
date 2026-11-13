//! Cluster metadata on object storage ([`search_server::ObjectStores`] over `search-store`):
//! states survive unloading and a node restart, branches copy them, deletes remove them.

mod common;

use base64::Engine;
use search_es::EsRequest;
use search_server::{ClusterSpec, Node, NodeConfig, ObjectStores, StaticCatalog, StoreProvider};
use std::sync::Arc;

fn basic(user: &str, pw: &str) -> String {
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pw}")))
}

fn req(cluster: &str, method: &str, uri: &str, body: &str) -> EsRequest {
    let mut r = EsRequest::json(method, uri, body.as_bytes().to_vec());
    r.headers.push(("authorization".into(), basic(cluster, "pw")));
    r
}

async fn call(node: &Arc<Node>, cluster: &str, method: &str, uri: &str, body: &str) -> (u16, String) {
    let r = node.handle(req(cluster, method, uri, body)).await;
    (r.status, String::from_utf8_lossy(&r.body).into_owned())
}

async fn open_store(objects: search_store::Objects, tag: &str) -> search_store::Store {
    let cache = search_store::test_dir().join(format!("server-cache-{tag}-{}", std::process::id()));
    search_store::Store::open(objects, cache, search_store::StoreConfig::default()).await.unwrap()
}

fn node(store: &search_store::Store) -> (Arc<Node>, Arc<StaticCatalog>) {
    let catalog = Arc::new(StaticCatalog::new());
    for c in ["prod", "preview"] {
        catalog.insert(ClusterSpec::new(c).with_password("pw"));
    }
    let node = Node::new(catalog.clone(), Arc::new(ObjectStores::new(store.clone())), NodeConfig::default());
    (node, catalog)
}

#[tokio::test]
async fn metadata_survives_unload_restart_and_branches() {
    let objects = search_store::test_objects("search-server-meta");
    let store = open_store(objects.clone(), "a").await;
    let (n1, _) = node(&store);

    let (s, b) = call(&n1, "prod", "PUT", "/books", r#"{"settings":{"number_of_replicas":0},"mappings":{"properties":{"title":{"type":"text"}}},"aliases":{"library":{}}}"#).await;
    assert_eq!(s, 200, "{b}");
    let (s, b) = call(&n1, "prod", "PUT", "/_index_template/logs", r#"{"index_patterns":["logs-*"],"priority":1}"#).await;
    assert_eq!(s, 200, "{b}");
    let (_, before) = call(&n1, "prod", "GET", "/books", "").await;

    // Unload and wake: the state comes back from the store.
    assert!(n1.unload("prod").await);
    let (s, after) = call(&n1, "prod", "GET", "/books", "").await;
    assert_eq!(s, 200, "{after}");
    assert_eq!(after, before);

    // Another node (a restart, or the next owner) reads the same state.
    let store2 = open_store(objects.clone(), "b").await;
    let (n2, _) = node(&store2);
    assert!(n1.unload("prod").await);
    let (s, b) = call(&n2, "prod", "GET", "/_index_template/logs?filter_path=index_templates.name", "").await;
    assert_eq!((s, b.as_str()), (200, r#"{"index_templates":[{"name":"logs"}]}"#));

    // A branch starts as a copy and then lives its own life.
    n2.branch("prod", "preview").await.unwrap();
    let (s, b) = call(&n2, "preview", "GET", "/library/_alias", "").await;
    assert_eq!((s, b.as_str()), (200, r#"{"books":{"aliases":{"library":{}}}}"#));
    let (s, _) = call(&n2, "preview", "DELETE", "/books", "").await;
    assert_eq!(s, 200);
    let (s, _) = call(&n2, "prod", "GET", "/books", "").await;
    assert_eq!(s, 200, "deleting in the branch leaves the parent alone");

    // Deleting a cluster removes its metadata: it wakes empty.
    assert!(n2.unload("preview").await);
    ObjectStores::new(store2.clone()).delete("preview").await.unwrap();
    let (s, b) = call(&n2, "preview", "GET", "/_cat/indices?h=index", "").await;
    assert_eq!((s, b.as_str()), (200, ""));
}
