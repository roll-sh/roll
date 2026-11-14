//! The inference API against a mock model server: OpenAI's, Cohere's and Anthropic's wire shapes
//! (no test calls a real model API). The request shapes the mock checks are the ones
//! Elasticsearch 9.5.5 sends (captured with the same mock), and so are the answers asserted.

use base64::Engine;
use search_es::json::{Value, parse};
use search_es::{Cluster, ClusterConfig, EsRequest, MemoryStore};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A request the mock received.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Answers as the services do; `/fail401/...` answers OpenAI's invalid key error.
fn answer(path: &str, body: &Value) -> (u16, String, String) {
    let json = |s: u16, v: String| (s, "application/json".to_string(), v);
    if path.contains("fail401") {
        return json(401, r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#.into());
    }
    let texts = |k: &str| -> Vec<String> {
        match body.get(k) {
            Some(Value::Arr(a)) => a.iter().map(Value::to_es_string).collect(),
            Some(v) => vec![v.to_es_string()],
            None => vec![],
        }
    };
    if path.ends_with("/embeddings") {
        // Base64 little-endian floats, as `encoding_format: base64` asks.
        let data: Vec<String> = texts("input")
            .iter()
            .enumerate()
            .map(|(i, t)| {
                // Unit length, as OpenAI's are (its endpoints default to `dot_product`).
                let raw = [t.len() as f32, 0.5, -0.25];
                let norm = raw.iter().map(|v| v * v).sum::<f32>().sqrt();
                let floats = raw.map(|v| v / norm);
                let raw: Vec<u8> = floats.iter().flat_map(|f| f.to_le_bytes()).collect();
                format!(r#"{{"object":"embedding","index":{i},"embedding":"{}"}}"#, base64::engine::general_purpose::STANDARD.encode(raw))
            })
            .collect();
        return json(200, format!(r#"{{"object":"list","data":[{}],"model":"m"}}"#, data.join(",")));
    }
    if path.ends_with("/chat/completions") {
        if body.get("stream").and_then(Value::as_bool) == Some(true) {
            let ev = |d: &str| format!("data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{d}\"}},\"finish_reason\":null}}]}}\n\n");
            return (200, "text/event-stream".into(), format!("{}{}data: [DONE]\n\n", ev("he"), ev("llo")));
        }
        return json(200, r#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}]}"#.into());
    }
    if path.ends_with("/v2/embed") {
        let n = texts("texts").len();
        let one = "[0.1,0.2]";
        return json(200, format!(r#"{{"id":"e","embeddings":{{"float":[{}]}},"texts":[]}}"#, vec![one; n].join(",")));
    }
    if path.ends_with("/v2/rerank") {
        // The last document first.
        let n = texts("documents").len();
        let results: Vec<String> = (0..n).rev().map(|i| format!(r#"{{"index":{i},"relevance_score":{}}}"#, if i + 1 == n { 0.9 } else { 0.1 })).collect();
        return json(200, format!(r#"{{"id":"r","results":[{}]}}"#, results.join(",")));
    }
    if path.ends_with("/v1/messages") {
        return json(200, r#"{"id":"m","type":"message","role":"assistant","content":[{"type":"text","text":"bonjour"}],"stop_reason":"end_turn"}"#.into());
    }
    json(404, r#"{"error":{"message":"no such path"}}"#.into())
}

/// A one-request-per-connection HTTP/1.1 mock on a free loopback port.
async fn mock() -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head_end, len) = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_string();
                        let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                        break (i + 4, len);
                    }
                };
                while buf.len() < head_end + len {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.lines();
                let path = lines.next().unwrap_or_default().split(' ').nth(1).unwrap_or_default().to_string();
                let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
                let body = parse(&buf[head_end..]).ok().flatten().unwrap_or(Value::Null);
                let (status, ct, out) = answer(&path, &body);
                log.lock().unwrap().push(Seen { path, headers, body });
                let resp = format!("HTTP/1.1 {status} X\r\ncontent-type: {ct}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}", out.len());
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), seen)
}

/// A cluster with a sealing key whose endpoints may call the mock.
async fn cluster(mock: &str, extra: &[(&str, &str)]) -> Cluster {
    let mut cfg = ClusterConfig::new("c");
    cfg.sealer = search_es::seal::Sealer::from_secret("0123456789abcdef0123456789abcdef-inference");
    for (k, v) in [("roll.inference.allow_private_hosts", "true"), ("roll.inference.cohere.url", mock), ("roll.inference.anthropic.url", mock)] {
        cfg.node_settings.push((k.into(), v.into()));
    }
    for (k, v) in extra {
        cfg.node_settings.push((k.to_string(), v.to_string()));
    }
    Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap()
}

async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> (u16, Value, String) {
    let r = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
    let resp = search_es::handle(c, r).await;
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    (resp.status, resp.body_json(), text)
}

fn seen(log: &Arc<Mutex<Vec<Seen>>>) -> Vec<Seen> {
    std::mem::take(&mut *log.lock().unwrap())
}

#[tokio::test]
async fn openai_endpoints_embed_complete_and_stream_without_showing_their_key() {
    let (m, log) = mock().await;
    let c = cluster(&m, &[]).await;
    let put = format!(r#"{{"service":"openai","service_settings":{{"api_key":"sk-secret-1","model_id":"emb","url":"{m}/v1/embeddings"}}}}"#);
    let (s, v, text) = call(&c, "PUT", "/_inference/text_embedding/emb", &put).await;
    assert_eq!(s, 200, "{v:?}");
    // The validation call learned the model's three dimensions; the key is nowhere.
    assert_eq!(v.pointer(&["service_settings", "dimensions"]).and_then(Value::as_i64), Some(3));
    assert_eq!(v.pointer(&["service_settings", "similarity"]).and_then(Value::as_str), Some("dot_product"));
    assert!(!text.contains("sk-secret-1"));
    let validation = seen(&log);
    assert_eq!(validation[0].header("authorization"), Some("Bearer sk-secret-1"));
    assert_eq!(validation[0].body.to_json(), r#"{"input":["how big"],"model":"emb","encoding_format":"base64"}"#);
    // Stored sealed, printed nowhere.
    let st = c.state();
    let stored = st.metadata.xpack.get("inference_secret", "emb").unwrap().get("api_key").unwrap().to_es_string();
    assert!(search_es::seal::is_sealed(&stored));
    let (_, _, listed) = call(&c, "GET", "/_inference/_all", "").await;
    assert!(!listed.contains("sk-secret-1") && !listed.contains("api_key"));

    let (s, v, _) = call(&c, "POST", "/_inference/text_embedding/emb", r#"{"input":["ab","abcd"]}"#).await;
    assert_eq!(s, 200);
    let first = v.pointer(&["text_embedding"]).and_then(|a| a.as_array()).map(|a| a.len());
    assert_eq!(first, Some(2), "{v:?}");
    let e = v.pointer(&["text_embedding"]).unwrap().as_array().unwrap()[0].get("embedding").unwrap().as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect::<Vec<_>>();
    assert!((e[0] - 0.96308684).abs() < 1e-6 && e.len() == 3, "{e:?}");
    seen(&log);

    let put = format!(r#"{{"service":"openai","service_settings":{{"api_key":"sk-2","model_id":"gpt","url":"{m}/v1/chat/completions"}}}}"#);
    assert_eq!(call(&c, "PUT", "/_inference/completion/chat", &put).await.0, 200);
    let (_, v, _) = call(&c, "POST", "/_inference/chat", r#"{"input":"hi"}"#).await;
    assert_eq!(v.to_json(), r#"{"completion":[{"result":"hello"}]}"#);
    let (s, _, sse) = call(&c, "POST", "/_inference/completion/chat/_stream", r#"{"input":"hi"}"#).await;
    assert_eq!(s, 200);
    assert_eq!(sse, "\u{feff}event: message\ndata: {\"completion\":[{\"delta\":\"he\"},{\"delta\":\"llo\"}]}\n\n\u{feff}event: message\ndata: [DONE]\n\n");
    let streamed = seen(&log).pop().unwrap();
    assert_eq!(streamed.body.get("stream_options").map(Value::to_json).as_deref(), Some(r#"{"include_usage":true}"#));

    // An upstream refusal becomes Elasticsearch's error, wrapped as a failed validation.
    let bad = format!(r#"{{"service":"openai","service_settings":{{"api_key":"k","model_id":"m","url":"{m}/fail401/v1/embeddings"}}}}"#);
    let (s, v, _) = call(&c, "PUT", "/_inference/text_embedding/bad", &bad).await;
    assert_eq!(s, 400);
    assert_eq!(
        v.pointer(&["error", "caused_by", "reason"]).and_then(Value::as_str),
        Some("Received an authentication error status code for request from inference entity id [bad] status [401]. Error message: [Incorrect API key provided]")
    );
    assert_eq!(call(&c, "GET", "/_inference/bad", "").await.0, 404);
}

#[tokio::test]
async fn cohere_and_anthropic_speak_their_own_apis() {
    let (m, log) = mock().await;
    let c = cluster(&m, &[]).await;
    let put = r#"{"service":"cohere","service_settings":{"api_key":"co-key","model_id":"embed-v4"}}"#;
    let (s, v, _) = call(&c, "PUT", "/_inference/text_embedding/co", put).await;
    assert_eq!(s, 200, "{v:?}");
    assert_eq!(v.pointer(&["service_settings", "dimensions"]).and_then(Value::as_i64), Some(2));
    let r = seen(&log).remove(0);
    assert_eq!(r.path, "/v2/embed");
    assert_eq!(r.header("authorization"), Some("Bearer co-key"));
    assert_eq!(r.header("request-source"), Some("unspecified:elasticsearch"));
    assert_eq!(r.body.to_json(), r#"{"texts":["how big"],"model":"embed-v4","embedding_types":["float"]}"#);

    assert_eq!(call(&c, "PUT", "/_inference/rerank/co-rr", r#"{"service":"cohere","service_settings":{"api_key":"co-key","model_id":"rerank-v3"}}"#).await.0, 200);
    let (_, v, _) = call(&c, "POST", "/_inference/rerank/co-rr", r#"{"input":["a","b"],"query":"q","top_n":2}"#).await;
    assert_eq!(v.to_json(), r#"{"rerank":[{"index":1,"relevance_score":0.9},{"index":0,"relevance_score":0.1}]}"#);
    assert_eq!(seen(&log).pop().unwrap().body.to_json(), r#"{"model":"rerank-v3","query":"q","documents":["a","b"],"top_n":2}"#);

    let put = r#"{"service":"anthropic","service_settings":{"api_key":"an-key","model_id":"claude"},"task_settings":{"max_tokens":64}}"#;
    assert_eq!(call(&c, "PUT", "/_inference/completion/an", put).await.0, 200);
    let (_, v, _) = call(&c, "POST", "/_inference/an", r#"{"input":"salut"}"#).await;
    assert_eq!(v.to_json(), r#"{"completion":[{"result":"bonjour"}]}"#);
    let r = seen(&log).pop().unwrap();
    assert_eq!((r.path.as_str(), r.header("x-api-key"), r.header("anthropic-version")), ("/v1/messages", Some("an-key"), Some("2023-06-01")));
    assert_eq!(r.body.to_json(), r#"{"messages":[{"role":"user","content":"salut"}],"model":"claude","max_tokens":64}"#);
}

#[tokio::test]
async fn endpoints_cannot_reach_private_addresses_unless_the_node_allows_it() {
    let (m, log) = mock().await;
    let mut cfg = ClusterConfig::new("c");
    cfg.node_settings.push(("roll.inference.cohere.url".into(), m.clone()));
    let c = Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap();
    let put = format!(r#"{{"service":"openai","service_settings":{{"api_key":"k","model_id":"m","url":"{m}/v1/embeddings"}}}}"#);
    let (s, v, _) = call(&c, "PUT", "/_inference/text_embedding/e", &put).await;
    assert_eq!(s, 400);
    assert!(v.pointer(&["error", "caused_by", "reason"]).and_then(Value::as_str).unwrap().contains("is a private address"), "{v:?}");
    // Not even the node's own override for a service's host lifts the rule.
    let (s, _, _) = call(&c, "PUT", "/_inference/text_embedding/co", r#"{"service":"cohere","service_settings":{"api_key":"k","model_id":"m"}}"#).await;
    assert_eq!(s, 400);
    assert!(seen(&log).is_empty());
}

#[tokio::test]
async fn the_test_plugin_services_exist_only_on_test_clusters() {
    let (m, _) = mock().await;
    let put = r#"{"service":"test_service","service_settings":{"model":"my_model","api_key":"abc64"}}"#;
    let c = cluster(&m, &[]).await;
    let (s, v, _) = call(&c, "PUT", "/_inference/sparse_embedding/sparse", put).await;
    assert_eq!((s, v.pointer(&["error", "reason"]).and_then(Value::as_str)), (400, Some("Unknown service [test_service]")));
    let c = cluster(&m, &[("roll.inference.test_services", "true")]).await;
    assert_eq!(call(&c, "PUT", "/_inference/sparse_embedding/sparse", put).await.0, 200);
    // `generateEmbedding`: |"hello".hashCode()| + 1 + position, in float arithmetic (which
    // rounds all five to the same float).
    let (_, v, _) = call(&c, "POST", "/_inference/sparse", r#"{"input":"hello"}"#).await;
    assert_eq!(v.pointer(&["sparse_embedding"]).unwrap().to_json(), r#"[{"is_truncated":false,"embedding":{"feature_0":9.916232E7,"feature_1":9.916232E7,"feature_2":9.916232E7,"feature_3":9.916232E7,"feature_4":9.916232E7}}]"#);
}

async fn call_as(c: &Cluster, auth: &str, method: &str, uri: &str, body: &str) -> (u16, Value, String) {
    let mut r = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
    if body.contains('\n') {
        r.headers = vec![("content-type".into(), "application/x-ndjson".into())];
    }
    r.headers.push(("authorization".into(), format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(auth))));
    let resp = search_es::handle(c, r).await;
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    (resp.status, resp.body_json(), text)
}

/// A writer with only index privileges embeds and searches `semantic_text` fields (the cluster
/// calls the endpoint, as Elasticsearch's inference filter and semantic query do), while the
/// inference API itself stays closed to it. A bulk request embeds all its documents in one call;
/// an update re-embeds only text it changed.
#[tokio::test]
async fn semantic_text_is_embedded_in_batches_and_searchable_by_a_writer_without_inference_privileges() {
    let (m, log) = mock().await;
    let c = cluster(&m, &[]).await;
    let put = format!(r#"{{"service":"openai","service_settings":{{"api_key":"sk","model_id":"emb","url":"{m}/v1/embeddings"}}}}"#);
    assert_eq!(call(&c, "PUT", "/_inference/text_embedding/emb", &put).await.0, 200);
    assert_eq!(call(&c, "PUT", "/docs", r#"{"mappings":{"properties":{"body":{"type":"semantic_text","inference_id":"emb"},"tag":{"type":"keyword"}}}}"#).await.0, 200);
    let role = r#"{"indices":[{"names":["docs"],"privileges":["read","write"]}]}"#;
    assert_eq!(call(&c, "PUT", "/_security/role/docs_writer", role).await.0, 200);
    assert_eq!(call(&c, "PUT", "/_security/user/w", r#"{"password":"writer-pass-1","roles":["docs_writer"]}"#).await.0, 200);
    seen(&log);
    let w = "w:writer-pass-1";

    let bulk = "{\"index\":{\"_index\":\"docs\",\"_id\":\"1\"}}\n{\"body\":\"ab\",\"tag\":\"x\"}\n{\"index\":{\"_index\":\"docs\",\"_id\":\"2\"}}\n{\"body\":\"abcd\",\"tag\":\"y\"}\n";
    let (s, v, _) = call_as(&c, w, "POST", "/_bulk?refresh=true", bulk).await;
    assert_eq!((s, v.get("errors").and_then(Value::as_bool)), (200, Some(false)), "{v:?}");
    let calls = seen(&log);
    assert_eq!(calls.len(), 1, "one call for the whole bulk");
    assert_eq!(calls[0].body.get("input").map(Value::to_json).as_deref(), Some(r#"["ab","abcd"]"#));

    // Embeddings are left out of `_source` unless asked for.
    let (_, v, _) = call_as(&c, w, "GET", "/docs/_doc/1", "").await;
    assert!(v.pointer(&["_source", "_inference_fields"]).is_none(), "{v:?}");
    let (_, v, _) = call_as(&c, w, "POST", "/docs/_search", r#"{"_source":{"exclude_vectors":false},"query":{"ids":{"values":["1"]}}}"#).await;
    let chunk = v.pointer(&["hits", "hits"]).and_then(|h| h.as_array()).and_then(|h| h.first()).and_then(|h| h.pointer(&["_source", "_inference_fields", "body", "inference", "chunks", "body"]));
    let chunk = chunk.and_then(Value::as_array).and_then(|c| c.first()).unwrap();
    assert_eq!((chunk.get("start_offset").and_then(Value::as_i64), chunk.get("end_offset").and_then(Value::as_i64)), (Some(0), Some(2)));
    assert_eq!(chunk.get("embeddings").and_then(Value::as_array).map(Vec::len), Some(3));

    // The query text is embedded once; the closer document ranks first.
    let (s, v, _) = call_as(&c, w, "POST", "/docs/_search", r#"{"query":{"semantic":{"field":"body","query":"wxyz"}}}"#).await;
    assert_eq!(s, 200, "{v:?}");
    let ids: Vec<String> = v.pointer(&["hits", "hits"]).and_then(|h| h.as_array()).unwrap().iter().map(|h| h.get("_id").unwrap().to_es_string()).collect();
    assert_eq!(ids, vec!["2", "1"]);
    assert_eq!(seen(&log).len(), 1);

    // `_msearch` embeds each search's query too.
    let msearch = "{\"index\":\"docs\"}\n{\"query\":{\"semantic\":{\"field\":\"body\",\"query\":\"ab\"}}}\n";
    let (s, v, _) = call_as(&c, w, "POST", "/_msearch", msearch).await;
    let top = v.pointer(&["responses"]).and_then(Value::as_array).and_then(|r| r.first()).and_then(|r| r.pointer(&["hits", "hits"])).and_then(Value::as_array).and_then(|h| h.first()).and_then(|h| h.get("_id")).map(Value::to_es_string);
    assert_eq!((s, top.as_deref()), (200, Some("1")), "{v:?}");
    seen(&log);

    // An update that keeps the text keeps its embeddings; one that changes it re-embeds.
    assert_eq!(call_as(&c, w, "POST", "/docs/_update/1", r#"{"doc":{"tag":"z"}}"#).await.0, 200);
    assert!(seen(&log).is_empty());
    assert_eq!(call_as(&c, w, "POST", "/docs/_update/1", r#"{"doc":{"body":"abc"}}"#).await.0, 200);
    assert_eq!(seen(&log).len(), 1);

    // The inference API needs its own privileges.
    let (s, _, _) = call_as(&c, w, "POST", "/_inference/emb", r#"{"input":"hi"}"#).await;
    assert_eq!(s, 403);
}

/// Retrievers over inference: `rrf`'s simplified `query`/`fields` form fuses a lexical query on
/// the text fields with the `semantic_text` field's semantic query, and `text_similarity_reranker`
/// reorders the window by a rerank endpoint (the mock ranks the last document first).
#[tokio::test]
async fn the_simplified_rrf_form_and_the_text_similarity_reranker() {
    let (m, log) = mock().await;
    let c = cluster(&m, &[]).await;
    let emb = format!(r#"{{"service":"openai","service_settings":{{"api_key":"sk","model_id":"emb","url":"{m}/v1/embeddings"}}}}"#);
    assert_eq!(call(&c, "PUT", "/_inference/text_embedding/emb", &emb).await.0, 200);
    assert_eq!(call(&c, "PUT", "/_inference/rerank/rr", r#"{"service":"cohere","service_settings":{"api_key":"k","model_id":"rerank-v3"}}"#).await.0, 200);
    let mapping = r#"{"mappings":{"properties":{"body":{"type":"semantic_text","inference_id":"emb"},"title":{"type":"text"}}}}"#;
    assert_eq!(call(&c, "PUT", "/docs", mapping).await.0, 200);
    for (id, body, title) in [("1", "ab", "red fox"), ("2", "abcd", "blue fox"), ("3", "abcdef", "green owl")] {
        let doc = format!(r#"{{"body":"{body}","title":"{title}"}}"#);
        assert_eq!(call(&c, "PUT", &format!("/docs/_doc/{id}?refresh=true"), &doc).await.0, 201);
    }
    seen(&log);
    let ids = |v: &Value| -> Vec<String> { v.pointer(&["hits", "hits"]).and_then(Value::as_array).unwrap().iter().map(|h| h.get("_id").unwrap().to_es_string()).collect() };

    // `fox` matches 1 and 2 lexically; the semantic query on `body` ranks 2 first.
    let (s, v, _) = call(&c, "POST", "/docs/_search", r#"{"retriever":{"rrf":{"query":"wxyz","fields":["title","body"]}}}"#).await;
    assert_eq!(s, 200, "{v:?}");
    assert_eq!(ids(&v).first().map(String::as_str), Some("2"), "{v:?}");
    let (s, v, _) = call(&c, "POST", "/docs/_search", r#"{"retriever":{"rrf":{"query":"","fields":["title"]}}}"#).await;
    assert_eq!((s, v.pointer(&["error", "reason"]).and_then(Value::as_str)), (400, Some("Validation Failed: 1: [rrf] [query] cannot be empty;")));
    seen(&log);

    let rerank = r#"{"size":2,"retriever":{"text_similarity_reranker":{"retriever":{"standard":{"query":{"match_all":{}}}},"field":"title","inference_id":"rr","inference_text":"owl","rank_window_size":3}}}"#;
    let (s, v, _) = call(&c, "POST", "/docs/_search", rerank).await;
    assert_eq!(s, 200, "{v:?}");
    // Tied scores keep the child's order.
    assert_eq!(ids(&v), vec!["3", "1"]);
    let calls = seen(&log);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body.get("query").map(Value::to_es_string).as_deref(), Some("owl"));
}

/// Field level security covers `semantic_text` fields: a role whose grant leaves a field out
/// can't query its chunks semantically or read them through the `semantic` highlighter, and a
/// field it may read doesn't show the text of a `copy_to` source it may not.
#[tokio::test]
async fn field_level_security_hides_semantic_text_from_queries_and_highlights() {
    let (m, _log) = mock().await;
    let c = cluster(&m, &[]).await;
    let put = format!(r#"{{"service":"openai","service_settings":{{"api_key":"sk","model_id":"emb","url":"{m}/v1/embeddings"}}}}"#);
    assert_eq!(call(&c, "PUT", "/_inference/text_embedding/emb", &put).await.0, 200);
    let mapping = r#"{"mappings":{"properties":{"body":{"type":"semantic_text","inference_id":"emb"},"secret":{"type":"text","copy_to":"body"},"tag":{"type":"keyword"}}}}"#;
    assert_eq!(call(&c, "PUT", "/docs", mapping).await.0, 200);
    assert_eq!(call(&c, "PUT", "/docs/_doc/1?refresh=true", r#"{"body":"public words","secret":"launch codes","tag":"x"}"#).await.0, 201);
    for (role, fls) in [("no_body", r#"{"grant":["*"],"except":["body","secret"]}"#), ("no_secret", r#"{"grant":["body","tag"]}"#)] {
        let r = format!(r#"{{"indices":[{{"names":["docs"],"privileges":["read"],"field_security":{fls}}}]}}"#);
        assert_eq!(call(&c, "PUT", &format!("/_security/role/{role}"), &r).await.0, 200);
        assert_eq!(call(&c, "PUT", &format!("/_security/user/{role}"), &format!(r#"{{"password":"reader-pass-1","roles":["{role}"]}}"#)).await.0, 200);
    }
    let hits = |v: &Value| v.pointer(&["hits", "hits"]).and_then(Value::as_array).cloned().unwrap_or_default();

    // Without `body`: its chunks match nothing and aren't highlighted.
    let (s, v, _) = call_as(&c, "no_body:reader-pass-1", "POST", "/docs/_search", r#"{"query":{"semantic":{"field":"body","query":"words"}}}"#).await;
    assert_eq!((s, hits(&v).len()), (200, 0), "{v:?}");
    let hl = r#"{"query":{"match_all":{}},"highlight":{"fields":{"body":{}}}}"#;
    let (s, v, _) = call_as(&c, "no_body:reader-pass-1", "POST", "/docs/_search", hl).await;
    assert_eq!(s, 200, "{v:?}");
    assert!(hits(&v)[0].get("highlight").is_none(), "{v:?}");

    // With `body` but not `secret`: body's own chunks, never the secret's text.
    let (s, v, _) = call_as(&c, "no_secret:reader-pass-1", "POST", "/docs/_search", r#"{"query":{"semantic":{"field":"body","query":"words"}}}"#).await;
    assert_eq!((s, hits(&v).len()), (200, 1), "{v:?}");
    let (s, v, text) = call_as(&c, "no_secret:reader-pass-1", "POST", "/docs/_search", hl).await;
    assert_eq!(s, 200, "{v:?}");
    assert!(!text.contains("launch codes"), "{text}");
    assert_eq!(hits(&v)[0].pointer(&["highlight", "body"]).map(Value::to_json).as_deref(), Some(r#"["public words"]"#), "{v:?}");
}

/// An `api_key` that looks sealed is sealed again, never kept as given: a client that copied a
/// sealed key (from a snapshot, from another cluster on the node) can't plant it in an endpoint
/// pointing at its own server for the node to open and send there.
#[tokio::test]
async fn a_planted_sealed_key_is_not_opened_for_the_client() {
    let (m, log) = mock().await;
    let c = cluster(&m, &[]).await;
    let planted = search_es::seal::seal(&c, "inference api key", "sk-victim");
    let put = format!(r#"{{"service":"openai","service_settings":{{"api_key":"{planted}","model_id":"emb","url":"{m}/v1/embeddings"}}}}"#);
    assert_eq!(call(&c, "PUT", "/_inference/text_embedding/thief", &put).await.0, 200);
    let calls = seen(&log);
    assert!(!calls.is_empty());
    for s in calls {
        assert_ne!(s.header("authorization"), Some("Bearer sk-victim"));
    }
}
