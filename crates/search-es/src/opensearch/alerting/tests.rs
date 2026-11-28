//! One test per monitor type, end to end (schedules driven through [`super::run_due`] at the
//! times the job records say), and the private-address refusal of a channel.

use super::*;
use crate::cluster::{ClusterConfig, Dialect, MemoryStore};
use crate::rest::EsRequest;
use std::sync::Arc;

async fn cluster(allow_private: bool) -> Cluster {
    let mut cfg = ClusterConfig::new("c");
    cfg.dialect = Dialect::OpenSearch;
    cfg.unsealed_secrets = true;
    if allow_private {
        cfg.node_settings.push((crate::opensearch::notifications::ALLOW_PRIVATE.into(), "true".into()));
    }
    Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap()
}

async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> (u16, Value) {
    let req = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
    let r = crate::rest::handle(c, req).await;
    (r.status, crate::json::parse(&r.body).ok().flatten().unwrap_or(Value::Null))
}

async fn ok(c: &Cluster, method: &str, uri: &str, body: &str) -> Value {
    let (s, v) = call(c, method, uri, body).await;
    assert!(s < 300, "{method} {uri}: {s} {}", v.to_json());
    v
}

/// A webhook receiver on 127.0.0.1: each request's body.
async fn receiver() -> (u16, tokio::sync::mpsc::UnboundedReceiver<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                    if body.len() >= len {
                        let _ = tx.send(body.to_string());
                        break;
                    }
                }
                match sock.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok").await;
        }
    });
    (port, rx)
}

fn next_run_of(c: &Cluster, id: &str) -> i64 {
    c.state().metadata.xpack.get(JOBS, id).and_then(|j| j.get("next_run")).and_then(Value::as_i64).expect("scheduled")
}

async fn alerts_of(c: &Cluster, id: &str) -> Vec<Value> {
    ok(c, "GET", &format!("/_plugins/_alerting/monitors/alerts?monitorId={id}&sortString=start_time"), "").await.get("alerts").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn states(alerts: &[Value]) -> Vec<String> {
    alerts.iter().map(|a| a.get("state").map(Value::to_es_string).unwrap_or_default()).collect()
}

/// A scheduled query-level monitor: the alert opens, its webhook action is sent once and then
/// throttled, it's acknowledged, and it completes into the history when the condition stops.
#[tokio::test]
async fn query_level_monitor_alerts_throttles_acknowledges_and_completes() {
    let c = cluster(true).await;
    let (port, mut rx) = receiver().await;
    ok(&c, "POST", "/_plugins/_notifications/configs", &format!(r#"{{"config_id":"hook","config":{{"name":"h","config_type":"webhook","webhook":{{"url":"http://127.0.0.1:{port}/in"}}}}}}"#)).await;
    ok(&c, "PUT", "/logs/_doc/1?refresh=true", r#"{"level":"error"}"#).await;
    let monitor = r#"{"name":"m","enabled":true,"schedule":{"period":{"interval":1,"unit":"MINUTES"}},
        "inputs":[{"search":{"indices":["logs"],"query":{"size":0,"query":{"term":{"level":"error"}}}}}],
        "triggers":[{"query_level_trigger":{"id":"t","name":"errors","severity":"1","condition":{"script":{"source":"ctx.results[0].hits.total.value > 0"}},
          "actions":[{"id":"a","name":"page","destination_id":"hook","message_template":{"source":"{{ctx.monitor.name}}: {{ctx.results.0.hits.total.value}}"},"throttle_enabled":true,"throttle":{"value":10,"unit":"MINUTES"}}]}}]}"#;
    let id = ok(&c, "POST", "/_plugins/_alerting/monitors", monitor).await.get("_id").map(Value::to_es_string).unwrap();
    let due = next_run_of(&c, &id);
    assert_eq!(run_due(&c, due - 1).await, 0, "not before it's due");
    assert_eq!(run_due(&c, due).await, 1);
    assert_eq!(rx.try_recv().as_deref(), Ok("m: 1"));
    assert_eq!(states(&alerts_of(&c, &id).await), ["ACTIVE"]);
    // The next run is throttled: nothing sent, the throttle counted.
    let due = next_run_of(&c, &id);
    assert_eq!(run_due(&c, due).await, 1);
    assert!(rx.try_recv().is_err(), "throttled");
    let a = &alerts_of(&c, &id).await[0];
    assert_eq!(a.pointer(&["action_execution_results"]).and_then(|r| r.as_array()).and_then(|r| r.first()).and_then(|r| r.get("throttled_count")).and_then(Value::as_i64), Some(1));
    let alert = a.get("id").map(Value::to_es_string).unwrap();
    let acked = ok(&c, "POST", &format!("/_plugins/_alerting/monitors/{id}/_acknowledge/alerts"), &format!(r#"{{"alerts":["{alert}"]}}"#)).await;
    assert_eq!(acked.get("success").and_then(Value::as_array).map(Vec::len), Some(1));
    // No errors left: the alert completes into the history.
    ok(&c, "DELETE", "/logs/_doc/1?refresh=true", "").await;
    let due = next_run_of(&c, &id);
    assert_eq!(run_due(&c, due).await, 1);
    assert_eq!(states(&alerts_of(&c, &id).await), ["COMPLETED"]);
    assert!(c.state().metadata.indices.values().any(|i| i.aliases.contains_key(HISTORY_WRITE)));
}

/// A bucket-level monitor: one alert per selected bucket; a bucket no longer selected completes.
#[tokio::test]
async fn bucket_level_monitor_keeps_an_alert_per_bucket() {
    let c = cluster(false).await;
    ok(&c, "POST", "/_bulk?refresh=true", "{\"index\":{\"_index\":\"logs\",\"_id\":\"1\"}}\n{\"level\":\"warn\",\"n\":5}\n{\"index\":{\"_index\":\"logs\",\"_id\":\"2\"}}\n{\"level\":\"error\",\"n\":3}\n{\"index\":{\"_index\":\"logs\",\"_id\":\"3\"}}\n{\"level\":\"info\",\"n\":1}\n").await;
    let monitor = r#"{"name":"b","monitor_type":"bucket_level_monitor","enabled":false,"schedule":{"period":{"interval":5,"unit":"MINUTES"}},
        "inputs":[{"search":{"indices":["logs"],"query":{"size":0,"aggregations":{"by":{"composite":{"sources":[{"level":{"terms":{"field":"level.keyword"}}}]},"aggs":{"total":{"sum":{"field":"n"}}}}}}}}],
        "triggers":[{"bucket_level_trigger":{"id":"bt","name":"big","severity":"2","condition":{"buckets_path":{"total":"total"},"parent_bucket_path":"by","script":{"source":"params.total > 2"}},"actions":[]}}]}"#;
    let id = ok(&c, "POST", "/_plugins/_alerting/monitors", monitor).await.get("_id").map(Value::to_es_string).unwrap();
    let r = ok(&c, "POST", &format!("/_plugins/_alerting/monitors/{id}/_execute"), "").await;
    let buckets = r.pointer(&["trigger_results", "bt", "agg_result_buckets"]).and_then(Value::as_object).map(|m| m.keys().cloned().collect::<Vec<_>>());
    assert_eq!(buckets, Some(vec!["error".to_string(), "warn".to_string()]));
    assert_eq!(states(&alerts_of(&c, &id).await), ["ACTIVE", "ACTIVE"]);
    ok(&c, "DELETE", "/logs/_doc/2?refresh=true", "").await;
    ok(&c, "POST", &format!("/_plugins/_alerting/monitors/{id}/_execute"), "").await;
    let mut st = states(&alerts_of(&c, &id).await);
    st.sort();
    assert_eq!(st, ["ACTIVE", "COMPLETED"]);
}

/// A scheduled document-level monitor sees only the documents written since its last run.
#[tokio::test]
async fn doc_level_monitor_alerts_on_new_documents_only() {
    let c = cluster(false).await;
    ok(&c, "PUT", "/logs/_doc/old?refresh=true", r#"{"level":"error"}"#).await;
    let monitor = r#"{"name":"d","monitor_type":"doc_level_monitor","enabled":true,"schedule":{"period":{"interval":1,"unit":"MINUTES"}},
        "inputs":[{"doc_level_input":{"indices":["logs"],"queries":[{"id":"q1","name":"errors","query":"level:error","tags":["sev1"]}]}}],
        "triggers":[{"document_level_trigger":{"id":"dt","name":"dt","severity":"1","condition":{"script":{"source":"query[tag=sev1]"}},"actions":[]}}]}"#;
    let id = ok(&c, "POST", "/_plugins/_alerting/monitors", monitor).await.get("_id").map(Value::to_es_string).unwrap();
    ok(&c, "PUT", "/logs/_doc/new?refresh=true", r#"{"level":"error"}"#).await;
    ok(&c, "PUT", "/logs/_doc/quiet?refresh=true", r#"{"level":"info"}"#).await;
    let dry = ok(&c, "POST", &format!("/_plugins/_alerting/monitors/{id}/_execute?dryrun=true"), "").await;
    assert_eq!(dry.pointer(&["trigger_results", "dt", "triggeredDocs"]).map(Value::to_json).as_deref(), Some(r#"["new|logs"]"#), "{} {:?}", dry.to_json(), c.state().metadata.xpack.get(JOBS, &id));
    assert_eq!(run_due(&c, next_run_of(&c, &id)).await, 1);
    let a = alerts_of(&c, &id).await;
    assert_eq!(a.iter().map(|a| a.get("related_doc_ids").map(Value::to_json).unwrap_or_default()).collect::<Vec<_>>(), [r#"["new|logs"]"#]);
    assert_eq!(run_due(&c, next_run_of(&c, &id)).await, 1);
    assert_eq!(alerts_of(&c, &id).await.len(), 1, "nothing new");
}

/// A channel to a private address is refused when the action runs; nothing is sent.
#[tokio::test]
async fn webhook_to_a_private_address_is_refused() {
    let c = cluster(false).await;
    let (port, mut rx) = receiver().await;
    ok(&c, "POST", "/_plugins/_notifications/configs", &format!(r#"{{"config_id":"hook","config":{{"name":"h","config_type":"webhook","webhook":{{"url":"http://127.0.0.1:{port}/in"}}}}}}"#)).await;
    let monitor = r#"{"name":"m","enabled":false,"schedule":{"period":{"interval":1,"unit":"MINUTES"}},"inputs":[],
        "triggers":[{"query_level_trigger":{"id":"t","name":"t","severity":"1","condition":{"script":{"source":"true"}},
          "actions":[{"id":"a","name":"a","destination_id":"hook","message_template":{"source":"x"}}]}}]}"#;
    let id = ok(&c, "POST", "/_plugins/_alerting/monitors", monitor).await.get("_id").map(Value::to_es_string).unwrap();
    let r = ok(&c, "POST", &format!("/_plugins/_alerting/monitors/{id}/_execute"), "").await;
    let err = r.pointer(&["trigger_results", "t", "action_results", "a", "error"]).map(Value::to_es_string).unwrap_or_default();
    assert!(err.contains("private address"), "{}", r.to_json());
    assert!(rx.try_recv().is_err());
    let (s, _) = call(&c, "POST", "/_plugins/_notifications/feature/test/hook", "").await;
    assert_eq!(s, 500);
}
