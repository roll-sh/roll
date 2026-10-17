//! The Confluent REST Proxy v3 subset tools use: clusters, topics, partitions, consumer groups
//! and produce (including streaming mode: several JSON records in one body). A stream is a
//! cluster; its `cluster_id` is the one Kafka clients see in Metadata.

use super::data::{Acks, Incoming, produce_records};
use super::time::{format_ms, parse_ms};
use super::{Api, ApiError, ApiResult, S, data_stream};
use crate::broker::{Stream, Topic};
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use stream_log::batch::NewRecord;

pub fn routes() -> Router<Arc<Api>> {
    Router::new()
        .route("/kafka/v3/clusters", get(clusters))
        .route("/kafka/v3/clusters/{c}", get(cluster))
        .route("/kafka/v3/clusters/{c}/topics", get(topics).post(create_topic))
        .route("/kafka/v3/clusters/{c}/topics/{t}", get(topic).delete(delete_topic))
        .route("/kafka/v3/clusters/{c}/topics/{t}/partitions", get(partitions))
        .route("/kafka/v3/clusters/{c}/topics/{t}/records", post(produce))
        .route("/kafka/v3/clusters/{c}/consumer-groups", get(groups))
}

/// Confluent's error shape.
struct V3Error(ApiError);

impl From<ApiError> for V3Error {
    fn from(e: ApiError) -> Self {
        V3Error(e)
    }
}

impl From<crate::error::KafkaError> for V3Error {
    fn from(e: crate::error::KafkaError) -> Self {
        V3Error(e.into())
    }
}

impl IntoResponse for V3Error {
    fn into_response(self) -> Response {
        let status = self.0.status;
        (status, axum::Json(json!({ "error_code": status.as_u16(), "message": self.0.message }))).into_response()
    }
}

type V3<T> = Result<T, V3Error>;

fn base(api: &Api) -> String {
    format!("{}/kafka/v3", api.b.config.http_url)
}

/// The stream behind a cluster id (its Kafka cluster id, stream id or name).
fn cluster_stream(api: &Api, headers: &HeaderMap, c: &str) -> ApiResult<Arc<Stream>> {
    let s = api
        .b
        .streams()
        .into_iter()
        .find(|s| s.cluster_id == c)
        .or_else(|| api.b.find_stream(c))
        .ok_or_else(|| ApiError::not_found(format!("cluster {c} not found")))?;
    data_stream(api, headers, &s.id)
}

fn cluster_view(api: &Api, s: &Stream) -> Value {
    let url = format!("{}/clusters/{}", base(api), s.cluster_id);
    let node = api.b.config.node_id;
    json!({
        "kind": "KafkaCluster",
        "metadata": { "self": url, "resource_name": format!("crn:///kafka={}", s.cluster_id) },
        "cluster_id": s.cluster_id,
        "controller": { "related": format!("{url}/brokers/{node}") },
        "acls": { "related": format!("{url}/acls") },
        "brokers": { "related": format!("{url}/brokers") },
        "broker_configs": { "related": format!("{url}/broker-configs") },
        "consumer_groups": { "related": format!("{url}/consumer-groups") },
        "topics": { "related": format!("{url}/topics") },
        "partition_reassignments": { "related": format!("{url}/topics/-/partitions/-/reassignment") },
    })
}

fn topic_view(api: &Api, s: &Stream, t: &Topic) -> Value {
    let url = format!("{}/clusters/{}/topics/{}", base(api), s.cluster_id, t.name);
    json!({
        "kind": "KafkaTopic",
        "metadata": { "self": url, "resource_name": format!("crn:///kafka={}/topic={}", s.cluster_id, t.name) },
        "cluster_id": s.cluster_id,
        "topic_name": t.name,
        "topic_id": t.id.to_string(),
        "is_internal": false,
        "replication_factor": 3,
        "partitions_count": t.partition_count(),
        "partitions": { "related": format!("{url}/partitions") },
        "configs": { "related": format!("{url}/configs") },
        "partition_reassignments": { "related": format!("{url}/partitions/-/reassignment") },
        "authorized_operations": [],
    })
}

fn list(kind: &str, url: String, data: Vec<Value>) -> axum::Json<Value> {
    axum::Json(json!({ "kind": kind, "metadata": { "self": url, "next": null }, "data": data }))
}

async fn clusters(State(api): S, headers: HeaderMap) -> V3<axum::Json<Value>> {
    let cred = super::principal(&api, &headers)?;
    let streams: Vec<Value> = api
        .b
        .streams()
        .iter()
        .filter(|s| match &cred {
            Some(c) => c.stream_id == s.id,
            None => !api.b.config.http_require_auth,
        })
        .map(|s| cluster_view(&api, s))
        .collect();
    if cred.is_none() && api.b.config.http_require_auth {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "an API key is required").into());
    }
    Ok(list("KafkaClusterList", format!("{}/clusters", base(&api)), streams))
}

async fn cluster(State(api): S, Path(c): Path<String>, headers: HeaderMap) -> V3<axum::Json<Value>> {
    let s = cluster_stream(&api, &headers, &c)?;
    Ok(axum::Json(cluster_view(&api, &s)))
}

async fn topics(State(api): S, Path(c): Path<String>, headers: HeaderMap) -> V3<axum::Json<Value>> {
    let s = cluster_stream(&api, &headers, &c)?;
    let data = s.topics().iter().map(|t| topic_view(&api, &s, t)).collect();
    Ok(list("KafkaTopicList", format!("{}/clusters/{}/topics", base(&api), s.cluster_id), data))
}

#[derive(Deserialize)]
struct NameValue {
    name: String,
    value: Option<String>,
}

#[derive(Deserialize)]
struct CreateTopic {
    topic_name: String,
    #[serde(default)]
    partitions_count: Option<i32>,
    #[serde(default)]
    replication_factor: Option<i16>,
    #[serde(default)]
    configs: Vec<NameValue>,
    #[serde(default)]
    validate_only: bool,
}

async fn create_topic(State(api): S, Path(c): Path<String>, headers: HeaderMap, axum::Json(req): axum::Json<CreateTopic>) -> V3<Response> {
    let s = cluster_stream(&api, &headers, &c)?;
    let configs: BTreeMap<String, String> = req.configs.into_iter().filter_map(|c| c.value.map(|v| (c.name, v))).collect();
    let t = api.b.create_topic(&s, &req.topic_name, req.partitions_count.unwrap_or(-1), req.replication_factor.unwrap_or(-1), configs, req.validate_only)?;
    let view = match t {
        Some(t) => topic_view(&api, &s, &t),
        None => json!({ "kind": "KafkaTopic", "cluster_id": s.cluster_id, "topic_name": req.topic_name }),
    };
    Ok((StatusCode::CREATED, axum::Json(view)).into_response())
}

async fn topic(State(api): S, Path((c, t)): Path<(String, String)>, headers: HeaderMap) -> V3<axum::Json<Value>> {
    let s = cluster_stream(&api, &headers, &c)?;
    let t = super::topic_of(&s, &t)?;
    Ok(axum::Json(topic_view(&api, &s, &t)))
}

async fn delete_topic(State(api): S, Path((c, t)): Path<(String, String)>, headers: HeaderMap) -> V3<StatusCode> {
    let s = cluster_stream(&api, &headers, &c)?;
    let t = super::topic_of(&s, &t)?;
    api.leases.topic_deleted(&s.id, &t.name);
    api.b.delete_topic(&s, &t).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn partitions(State(api): S, Path((c, t)): Path<(String, String)>, headers: HeaderMap) -> V3<axum::Json<Value>> {
    let s = cluster_stream(&api, &headers, &c)?;
    let t = super::topic_of(&s, &t)?;
    let url = format!("{}/clusters/{}/topics/{}/partitions", base(&api), s.cluster_id, t.name);
    let node = api.b.config.node_id;
    let data = (0..t.partition_count())
        .map(|p| {
            json!({
                "kind": "KafkaPartition",
                "metadata": { "self": format!("{url}/{p}"), "resource_name": format!("crn:///kafka={}/topic={}/partition={p}", s.cluster_id, t.name) },
                "cluster_id": s.cluster_id,
                "topic_name": t.name,
                "partition_id": p,
                "leader": { "related": format!("{url}/{p}/replicas/{node}") },
                "replicas": { "related": format!("{url}/{p}/replicas") },
                "reassignment": { "related": format!("{url}/{p}/reassignment") },
            })
        })
        .collect();
    Ok(list("KafkaPartitionList", url, data))
}

async fn groups(State(api): S, Path(c): Path<String>, headers: HeaderMap) -> V3<axum::Json<Value>> {
    let s = cluster_stream(&api, &headers, &c)?;
    let url = format!("{}/clusters/{}/consumer-groups", base(&api), s.cluster_id);
    let data = api
        .b
        .groups
        .list(&api.b, &s.id)
        .into_iter()
        .map(|(id, _, state)| {
            let info = api.b.groups.describe(&api.b, &s.id, &id);
            json!({
                "kind": "KafkaConsumerGroup",
                "metadata": { "self": format!("{url}/{id}"), "resource_name": format!("crn:///kafka={}/consumer-group={id}", s.cluster_id) },
                "cluster_id": s.cluster_id,
                "consumer_group_id": id,
                "is_simple": info.as_ref().is_none_or(|g| g.protocol_type.is_empty()),
                "partition_assignor": info.as_ref().map(|g| g.protocol.clone()).unwrap_or_default(),
                "state": state.to_ascii_uppercase(),
                "coordinator": { "related": format!("{}/clusters/{}/brokers/{}", base(&api), s.cluster_id, api.b.config.node_id) },
                "consumers": { "related": format!("{url}/{id}/consumers") },
                "lag_summary": { "related": format!("{url}/{id}/lag-summary") },
            })
        })
        .collect();
    Ok(list("KafkaConsumerGroupList", url, data))
}

#[derive(Deserialize)]
struct V3Data {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    data: Value,
}

#[derive(Deserialize)]
struct V3Header {
    name: String,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Deserialize)]
struct V3Record {
    #[serde(default)]
    partition_id: Option<i32>,
    #[serde(default)]
    headers: Vec<V3Header>,
    #[serde(default)]
    key: Option<V3Data>,
    #[serde(default)]
    value: Option<V3Data>,
    #[serde(default)]
    timestamp: Option<String>,
}

/// `(bytes, type)` for a key or value. BINARY is base64, JSON is serialised, STRING is text.
fn v3_bytes(d: Option<V3Data>) -> Result<(Option<Bytes>, &'static str), String> {
    let Some(d) = d else { return Ok((None, "BINARY")) };
    let kind = d.kind.as_deref().unwrap_or("JSON").to_ascii_uppercase();
    if d.data.is_null() {
        return Ok((None, if kind == "BINARY" { "BINARY" } else { "JSON" }));
    }
    match kind.as_str() {
        "BINARY" => {
            let s = d.data.as_str().ok_or("BINARY data must be a base64 string")?;
            Ok((Some(Bytes::from(B64.decode(s).map_err(|_| "BINARY data is not valid base64")?)), "BINARY"))
        }
        "STRING" => {
            let s = d.data.as_str().ok_or("STRING data must be a string")?;
            Ok((Some(Bytes::from(s.to_string())), "STRING"))
        }
        "JSON" => Ok((Some(Bytes::from(serde_json::to_vec(&d.data).map_err(|e| e.to_string())?)), "JSON")),
        other => Err(format!("{other} needs Schema Registry, which roll doesn't run; send BINARY, JSON or STRING")),
    }
}

/// `POST .../records`: one record per JSON object; several concatenated objects is Confluent's
/// streaming mode, answered with one result object per record.
async fn produce(State(api): S, Path((c, t)): Path<(String, String)>, headers: HeaderMap, body: Bytes) -> V3<Response> {
    let s = cluster_stream(&api, &headers, &c)?;
    let topic = match s.topic(&t) {
        Some(t) => t,
        None => return Err(ApiError::new(StatusCode::NOT_FOUND, "unknown_topic_or_partition", "This server does not host this topic-partition.").into()),
    };
    type Parsed = (Incoming, &'static str, &'static str, usize, usize);
    let mut items: Vec<Result<Parsed, String>> = vec![];
    for value in serde_json::Deserializer::from_slice(&body).into_iter::<V3Record>() {
        let r = match value {
            Ok(r) => r,
            Err(e) => {
                items.push(Err(format!("bad JSON: {e}")));
                break;
            }
        };
        let parsed = (|| {
            let (key, kt) = v3_bytes(r.key)?;
            let (value, vt) = v3_bytes(r.value)?;
            let headers = r
                .headers
                .into_iter()
                .map(|h| Ok((h.name, h.value.map(|v| B64.decode(v).map(Bytes::from)).transpose().map_err(|_| "header values are base64".to_string())?)))
                .collect::<Result<Vec<_>, String>>()?;
            let timestamp = r.timestamp.map(|ts| parse_ms(&ts).ok_or(format!("bad timestamp {ts}"))).transpose()?;
            let (ks, vs) = (key.as_ref().map_or(0, |k| k.len()), value.as_ref().map_or(0, |v| v.len()));
            Ok((Incoming { partition: r.partition_id, record: NewRecord { key, value, headers, timestamp } }, kt, vt, ks, vs))
        })();
        items.push(parsed);
    }
    if items.is_empty() {
        return Err(ApiError::bad("no records in the request").into());
    }
    let mut out: Vec<Value> = vec![Value::Null; items.len()];
    let mut ok = vec![];
    let mut meta = vec![];
    for (i, item) in items.into_iter().enumerate() {
        match item {
            Ok((inc, kt, vt, ks, vs)) => {
                ok.push(inc);
                meta.push((i, kt, vt, ks, vs));
            }
            Err(msg) => out[i] = json!({ "error_code": 400, "message": msg }),
        }
    }
    let results = produce_records(&api, &s, &topic, ok, Acks::All, Duration::from_secs(30)).await;
    for (r, (i, kt, vt, ks, vs)) in results.into_iter().zip(meta) {
        out[i] = match r {
            Ok(p) => json!({
                "error_code": 200,
                "cluster_id": s.cluster_id,
                "topic_name": topic.name,
                "partition_id": p.partition,
                "offset": p.offset,
                "timestamp": format_ms(p.timestamp),
                "key": if ks > 0 { json!({ "type": kt, "size": ks }) } else { Value::Null },
                "value": if vs > 0 { json!({ "type": vt, "size": vs }) } else { Value::Null },
            }),
            Err(e) => json!({ "error_code": e.status.as_u16(), "message": e.message }),
        };
    }
    if out.len() == 1 {
        let status = StatusCode::from_u16(out[0]["error_code"].as_u64().unwrap_or(200) as u16).unwrap_or(StatusCode::OK);
        return Ok((status, axum::Json(out.remove(0))).into_response());
    }
    let body: String = out.iter().map(|v| v.to_string() + "\n").collect();
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}
