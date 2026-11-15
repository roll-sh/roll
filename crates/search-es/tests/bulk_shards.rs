//! `_bulk` applies index items prepared ahead with the other items of their shard, the shards in
//! parallel. Whatever the interleaving, every item's result and every document's final version,
//! sequence number and source must be what applying the items one at a time in request order
//! gives: same-id items, creates that conflict, compare-and-set writes, deletes and updates
//! between them, dynamic mapping updates and several indices with several shards each.

use search_es::json::{Map, Value, parse};
use search_es::{Cluster, EsRequest};

async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> (u16, Value) {
    let mut r = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
    if uri.contains("_bulk") {
        r.headers = vec![("content-type".into(), "application/x-ndjson".into())];
    }
    let resp = search_es::handle(c, r).await;
    (resp.status, parse(&resp.body).ok().flatten().unwrap_or(Value::Null))
}

/// A deterministic mix of bulk items (action line, optional source line).
fn items(seed: u64, n: usize) -> Vec<(String, Option<String>)> {
    let mut x = seed | 1;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut out = Vec::new();
    for i in 0..n {
        let index = if next().is_multiple_of(3) { "b" } else { "a" };
        // Few ids, so items of one id meet in one shard's queue.
        let id = next() % 40;
        let doc = |next: &mut dyn FnMut() -> u64| {
            // Now and then a field the mapping doesn't have yet: a dynamic mapping update.
            let extra = if next().is_multiple_of(50) { format!(",\"f{}\":{}", next() % 7, i) } else { String::new() };
            Some(format!("{{\"n\":{i},\"k\":\"w{}\"{extra}}}", next() % 5))
        };
        let item = match next() % 20 {
            0 => (format!("{{\"delete\":{{\"_index\":\"{index}\",\"_id\":\"{id}\"}}}}"), None),
            1 => (format!("{{\"update\":{{\"_index\":\"{index}\",\"_id\":\"{id}\"}}}}"), Some(format!("{{\"doc\":{{\"u\":{i}}}}}"))),
            2 | 3 => (format!("{{\"create\":{{\"_index\":\"{index}\",\"_id\":\"{id}\"}}}}"), doc(&mut next)),
            4 => (format!("{{\"index\":{{\"_index\":\"{index}\",\"_id\":\"{id}\",\"if_seq_no\":{},\"if_primary_term\":1}}}}", next() % 30), doc(&mut next)),
            _ => (format!("{{\"index\":{{\"_index\":\"{index}\",\"_id\":\"{id}\"}}}}"), doc(&mut next)),
        };
        out.push(item);
    }
    out
}

fn body(items: &[(String, Option<String>)]) -> String {
    let mut b = String::new();
    for (a, s) in items {
        b.push_str(a);
        b.push('\n');
        if let Some(s) = s {
            b.push_str(s);
            b.push('\n');
        }
    }
    b
}

async fn cluster() -> Cluster {
    let c = Cluster::in_memory("bulk-shards").await;
    for i in ["a", "b"] {
        let (s, _) = call(&c, "PUT", &format!("/{i}"), r#"{"settings":{"number_of_shards":3},"mappings":{"properties":{"n":{"type":"long"},"k":{"type":"keyword"}}}}"#).await;
        assert_eq!(s, 200);
    }
    c
}

/// Every document of both indices: id, version, seq no, primary term and source.
async fn state(c: &Cluster) -> Vec<Value> {
    call(c, "POST", "/a,b/_refresh", "").await;
    let (_, r) = call(c, "POST", "/a,b/_search", r#"{"size":200,"seq_no_primary_term":true,"version":true,"sort":["_index","n"]}"#).await;
    let Some(Value::Arr(hits)) = r.get("hits").and_then(|h| h.get("hits")).cloned() else { panic!("{r:?}") };
    hits.into_iter()
        .map(|mut h| {
            if let Value::Obj(m) = &mut h {
                m.shift_remove("sort");
            }
            h
        })
        .collect()
}

/// The items of a bulk response, without the index uuids (each cluster has its own).
fn item_results(r: &Value) -> Vec<Value> {
    fn strip(v: &mut Value) {
        match v {
            Value::Obj(m) => {
                m.shift_remove("index_uuid");
                m.values_mut().for_each(strip);
            }
            Value::Arr(a) => a.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let Some(Value::Arr(mut items)) = r.get("items").cloned() else { panic!("{r:?}") };
    items.iter_mut().for_each(strip);
    items
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shard_queues_match_one_item_at_a_time() {
    for seed in [3u64, 17, 99] {
        let its = items(seed, 600);
        // One request: the index items are prepared ahead and queued per shard.
        let together = cluster().await;
        let (s, r) = call(&together, "POST", "/_bulk", &body(&its)).await;
        assert_eq!(s, 200);
        let got = item_results(&r);
        // One request per item: nothing is prepared ahead or queued.
        let alone = cluster().await;
        let mut want = Vec::new();
        let mut errors = false;
        for it in &its {
            let (s, r) = call(&alone, "POST", "/_bulk", &body(std::slice::from_ref(it))).await;
            assert_eq!(s, 200);
            errors |= r.get("errors") == Some(&Value::Bool(true));
            want.extend(item_results(&r));
        }
        assert_eq!(r.get("errors"), Some(&Value::Bool(errors)), "seed {seed}");
        assert_eq!(got.len(), want.len());
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            assert_eq!(g, w, "seed {seed}, item {i}: {:?}", its[i]);
        }
        assert_eq!(state(&together).await, state(&alone).await, "seed {seed}");
        // The mix exercised what it is meant to.
        let status = |v: &Value| -> i64 { v.as_object().and_then(|m: &Map| m.values().next()).and_then(|b| b.get("status")).and_then(Value::as_i64).unwrap_or(0) };
        assert!(got.iter().any(|v| status(v) == 409), "no conflicts in seed {seed}");
        assert!(got.iter().filter(|v| status(v) == 200 || status(v) == 201).count() > 300, "seed {seed}");
    }
}
