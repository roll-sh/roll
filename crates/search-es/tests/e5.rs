//! The E5 endpoint (`.multilingual-e5-small-elasticsearch` and `elasticsearch` service endpoints
//! of the model) on a cluster whose node runs the model: the REST answers Elasticsearch 9.5.5
//! gave with one ML node (`tests/fixtures/e5/es-9.5.5.json`: the requests and its answers, the
//! embeddings kept as their length), and what reaches the node's embedder: the
//! `passage: ` prefix at ingest, `query: ` at search, none otherwise. The embedder here is a
//! fake that records its inputs; the model's own agreement with Elasticsearch is search-embed's
//! `tests/agreement.rs`.

use search_es::json::{Value, parse};
use search_es::xpack::inference::local::{EmbedError, Embedder};
use search_es::{Cluster, ClusterConfig, EsRequest, MemoryStore};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Fake {
    sent: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Embedder for Fake {
    fn available(&self) -> bool {
        true
    }

    async fn embed(&self, _cluster: &str, texts: Vec<String>, timeout: Duration) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.iter().any(|t| t.contains("<queue full>")) {
            return Err(EmbedError::QueueFull);
        }
        if texts.iter().any(|t| t.contains("<slow>")) {
            return Err(EmbedError::TimedOut(timeout));
        }
        self.sent.lock().unwrap().extend(texts.iter().cloned());
        // Unit vectors that differ by text.
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; 384];
                v[t.len() % 384] = 0.6;
                v[(t.len() * 7 + 1) % 384] += 0.8;
                let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                v.iter().map(|x| x / n).collect()
            })
            .collect())
    }
}

/// Arrays of 100 numbers or more become `{"<floats>": n}`, as in the fixture.
fn shrink(v: &mut Value) {
    match v {
        Value::Arr(a) if a.len() >= 100 && a.iter().all(|x| x.as_f64().is_some()) => *v = parse(format!(r#"{{"<floats>":{}}}"#, a.len()).as_bytes()).unwrap().unwrap(),
        Value::Arr(a) => a.iter_mut().for_each(shrink),
        Value::Obj(m) => m.values_mut().for_each(shrink),
        _ => {}
    }
}

/// `took` and scores (the fake's vectors aren't the model's) are masked.
fn mask(v: &mut Value) {
    if let Value::Obj(m) = v {
        for (k, x) in m.iter_mut() {
            if matches!(k.as_str(), "took" | "_score" | "max_score") && !x.is_null() {
                *x = Value::from("<masked>");
            } else {
                mask(x);
            }
        }
    } else if let Value::Arr(a) = v {
        a.iter_mut().for_each(mask);
    }
}

#[tokio::test]
async fn the_e5_endpoint_answers_as_elasticsearch_with_an_ml_node() {
    let fake = Arc::new(Fake::default());
    let mut cfg = ClusterConfig::new("c");
    cfg.embedder = Some(fake.clone());
    let c = Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap();
    let raw = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/e5/es-9.5.5.json")).unwrap();
    let mut fixture = parse(&raw).unwrap().unwrap();
    // The fixture's node was Linux x86-64, where Elasticsearch's preconfigured endpoint names the
    // model's 8-bit build; roll runs the float build and names it, as Elasticsearch does on any
    // other platform.
    let Some(Value::Arr(steps)) = fixture.as_object_mut().and_then(|f| f.get_mut("steps")) else { panic!("no steps") };
    for step in steps.iter_mut() {
        if step.get("req").and_then(Value::as_str) == Some("GET /_inference/.multilingual-e5-small-elasticsearch")
            && let Some(Value::Arr(eps)) = step.as_object_mut().and_then(|s| s.get_mut("response")).and_then(|r| r.as_object_mut()).and_then(|r| r.get_mut("endpoints"))
            && let Some(Value::Obj(ss)) = eps[0].as_object_mut().and_then(|e| e.get_mut("service_settings"))
        {
            ss.insert("model_id".into(), Value::from(".multilingual-e5-small"));
        }
    }
    for step in fixture.get("steps").and_then(Value::as_array).unwrap() {
        let req = step.get("req").and_then(Value::as_str).unwrap();
        let (method, uri) = req.split_once(' ').unwrap();
        let r = match step.get("body") {
            Some(b) => EsRequest::json(method, uri, b.to_json()),
            None => EsRequest::new(method, uri),
        };
        fake.sent.lock().unwrap().clear();
        let resp = search_es::handle(&c, r).await;
        let mut got = resp.body_json();
        shrink(&mut got);
        mask(&mut got);
        let mut want = step.get("response").cloned().unwrap();
        mask(&mut want);
        assert_eq!(resp.status as i64, step.get("status").and_then(Value::as_i64).unwrap(), "{req}: {}", got.to_json());
        assert_eq!(got.to_json(), want.to_json(), "{req}");
        if let Some(sent) = step.get("sent").and_then(Value::as_array) {
            let sent: Vec<String> = sent.iter().map(Value::to_es_string).collect();
            assert_eq!(*fake.sent.lock().unwrap(), sent, "{req}: what the embedder got");
        }
    }
}

#[tokio::test]
async fn without_the_model_e5_answers_as_a_cluster_without_ml_nodes() {
    let c = Cluster::open(ClusterConfig::new("c"), Arc::new(MemoryStore::new())).await.unwrap();
    let resp = search_es::handle(&c, EsRequest::json("POST", "/_inference/.multilingual-e5-small-elasticsearch", r#"{"input":"hi"}"#.to_string())).await;
    assert_eq!(resp.status, 429);
    assert!(String::from_utf8_lossy(&resp.body).contains("no ML nodes with sufficient capacity"));
    let list = search_es::handle(&c, EsRequest::new("GET", "/_inference/.multilingual-e5-small-elasticsearch")).await.body_json();
    let ep = &list.get("endpoints").and_then(Value::as_array).unwrap()[0];
    assert_eq!(ep.pointer(&["service_settings", "model_id"]).and_then(Value::as_str), Some(".multilingual-e5-small"));
}
