//! Each upstream kind's URL, auth and framing against mocks: Bedrock (SigV4 and its event
//! stream), Vertex (the service account's token flow), Azure, and `Gateway::test`.

mod common;

use agent_proto::{UpstreamKind, UpstreamSpec};
use axum::body::Bytes;
use base64::Engine;
use common::*;
use model_gateway::upstream::{eventstream, sigv4};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// AWS's worked example (Signature Version 4, "Example: GET request", IAM ListUsers).
#[test]
fn sigv4_matches_aws_published_example() {
    let creds = sigv4::Credentials { access_key: format!("AKID{}", "EXAMPLE"), secret_key: format!("wJalrXUtnFEMI/K7MDENG+bPxRfiCY{}", "EXAMPLEKEY"), token: None };
    let mut headers = vec![
        ("x-amz-date".to_string(), "20150830T123600Z".to_string()),
        ("host".to_string(), "iam.amazonaws.com".to_string()),
        ("content-type".to_string(), "application/x-www-form-urlencoded; charset=utf-8".to_string()),
    ];
    let req = sigv4::Request { method: "GET", path: "/", query: "Action=ListUsers&Version=2010-05-08", body: b"" };
    let auth = sigv4::sign(&creds, "us-east-1", "iam", &req, &mut headers);
    assert_eq!(
        auth,
        format!(
            "AWS4-HMAC-SHA256 Credential=AKID{}/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7",
            "EXAMPLE"
        )
    );
    assert_eq!(sigv4::amz_date(1_440_938_160), "20150830T123600Z");
    // A model id's `:` is encoded once in the path and again in what's signed.
    assert_eq!(sigv4::uri_encode("anthropic.claude-x-v1:0"), "anthropic.claude-x-v1%3A0");
    // Key pairs parse; anything else is a Bedrock API key.
    let pair = sigv4::Credentials::parse("AKIAIOSFODNN7EXAMPLE:secret:token").unwrap();
    assert_eq!((pair.access_key.as_str(), pair.token.as_deref()), ("AKIAIOSFODNN7EXAMPLE", Some("token")));
    assert!(sigv4::Credentials::parse("ABSKQmVkcm9ja0FQSUtleS1leGFtcGxl").is_none());
}

const ACCESS: &str = "AKIAIOSFODNN7EXAMPLE";
const SECRET: &str = "bedrock-test-secret";

fn bedrock(base: &str) -> UpstreamSpec {
    let mut u = upstream("bedrock", UpstreamKind::Bedrock, base);
    u.region = Some("us-east-1".into());
    u.models = vec!["anthropic.claude-*".into(), "us.anthropic.claude-*".into()];
    u
}

/// Whether a request a mock saw carries a valid SigV4 signature for `service`.
fn signed_right(s: &Seen, service: &str) -> bool {
    let creds = sigv4::Credentials { access_key: ACCESS.into(), secret_key: SECRET.into(), token: None };
    let mut headers = vec![("host".to_string(), s.header("host").unwrap().to_string()), ("x-amz-date".to_string(), s.header("x-amz-date").unwrap().to_string())];
    if let Some(ct) = s.header("content-type") {
        headers.push(("content-type".into(), ct.into()));
    }
    let (path, query) = s.uri.split_once('?').unwrap_or((&s.uri, ""));
    let want = sigv4::sign(&creds, "us-east-1", service, &sigv4::Request { method: s.method.as_str(), path, query, body: &s.body }, &mut headers);
    s.header("authorization") == Some(want.as_str())
}

/// A Messages stream's events as Bedrock frames them.
fn bedrock_frames(model: &str) -> (Vec<u8>, String) {
    let mut frames = Vec::new();
    let mut sse = String::new();
    for e in messages_stream(model) {
        let data = e.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
        let v: Value = serde_json::from_str(data).unwrap();
        let payload = json!({ "bytes": base64::engine::general_purpose::STANDARD.encode(data), "p": "abcd" }).to_string();
        frames.extend(eventstream::encode(&[(":event-type", "chunk"), (":content-type", "application/json"), (":message-type", "event")], payload.as_bytes()));
        sse.push_str(&format!("event: {}\ndata: {data}\n\n", v["type"].as_str().unwrap()));
    }
    (frames, sse)
}

#[tokio::test]
async fn bedrock_signs_with_sigv4_and_decodes_its_event_stream() {
    let (frames, want_sse) = bedrock_frames("claude-x");
    let mock = Mock::start(move |s| {
        if !signed_right(s, "bedrock") {
            return status(403);
        }
        match s.uri.as_str() {
            "/model/us.anthropic.claude-x-v1%3A0/invoke-with-response-stream" => {
                let chunks = frames.chunks(13).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
                stream("application/vnd.amazon.eventstream", std::time::Duration::ZERO, chunks)
            }
            "/model/us.anthropic.claude-x-v1%3A0/invoke" => json(200, messages_answer("claude-x", 4, 2)),
            "/foundation-models" => json(200, json!({ "modelSummaries": [{ "modelId": "a" }, { "modelId": "b" }] })),
            _ => status(404),
        }
    })
    .await;
    let g = Gw::start(vec![bedrock(&mock.url(""))], vec![], &[("bedrock", &format!("{ACCESS}:{SECRET}"))]).await;
    let r = g
        .http
        .post(g.url("/v1/messages"))
        .bearer_auth(&g.key)
        .header("anthropic-beta", "context-1m-2025-08-07, interleaved-thinking-2025-05-14")
        .json(&messages("us.anthropic.claude-x-v1:0", true))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), want_sse);
    let s = &mock.seen()[0];
    let body = s.json();
    assert_eq!(body["anthropic_version"], "bedrock-2023-05-31");
    assert_eq!(body["anthropic_beta"], json!(["context-1m-2025-08-07", "interleaved-thinking-2025-05-14"]));
    assert!(body.get("model").is_none() && body.get("stream").is_none(), "{body}");
    assert_eq!(s.header("accept"), Some("application/vnd.amazon.eventstream"));

    let r = g.post("/v1/messages", &messages("us.anthropic.claude-x-v1:0", false)).await;
    assert_eq!(r.json::<Value>().await.unwrap()["usage"]["input_tokens"], 4);

    let checks = g.gw.test(None).await;
    assert_eq!((checks[0].ok, checks[0].models), (true, 2), "{checks:?}");
    assert_eq!(mock.count(), 3);
    assert!(mock.seen().iter().all(|s| s.header("x-amz-date").is_some()));
}

#[tokio::test]
async fn bedrock_takes_an_api_key_as_a_bearer_token_and_reports_exceptions() {
    let throttle = eventstream::encode(&[(":message-type", "exception"), (":exception-type", "throttlingException")], br#"{"message":"Too many requests"}"#);
    let mock = Mock::start(move |s| {
        if s.header("authorization") != Some("Bearer ABSKbedrockapikey") {
            return status(403);
        }
        bytes(200, "application/vnd.amazon.eventstream", throttle.clone())
    })
    .await;
    let g = Gw::start(vec![bedrock(&mock.url(""))], vec![], &[("bedrock", "ABSKbedrockapikey")]).await;
    // A throttling exception before anything reached the client is a 429.
    let r = g.post("/v1/messages", &messages("anthropic.claude-x", true)).await;
    assert_eq!(r.status(), 429);
    assert!(r.text().await.unwrap().contains("throttlingException"));
    assert!(g.gw.status().await.upstreams[0].cooling_until.is_some());
}

/// A service account key, and the PEM of its public half.
fn service_account() -> (String, String) {
    use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
    let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
    let private = key.to_pkcs8_pem(LineEnding::LF).unwrap().to_string();
    let public = key.to_public_key().to_public_key_pem(LineEnding::LF).unwrap();
    let sa = json!({
        "type": "service_account", "project_id": "p", "private_key_id": "kid-1", "private_key": private,
        "client_email": "gateway@p.iam.gserviceaccount.com", "token_uri": "https://oauth2.googleapis.com/token"
    });
    (sa.to_string(), public)
}

#[tokio::test]
async fn vertex_trades_a_service_account_jwt_for_a_cached_token() {
    let (sa, public) = service_account();
    let token_uri = Arc::new(Mutex::new(String::new()));
    let uri = token_uri.clone();
    let mock = Mock::start(move |s| {
        if s.uri == "/token" {
            let form: Vec<(String, String)> = url::form_urlencoded::parse(&s.body).into_owned().collect();
            let get = |k: &str| form.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
            assert_eq!(get("grant_type"), "urn:ietf:params:oauth:grant-type:jwt-bearer");
            let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
            v.set_audience(&[uri.lock().unwrap().clone()]);
            let key = jsonwebtoken::DecodingKey::from_rsa_pem(public.as_bytes()).unwrap();
            let Ok(claims) = jsonwebtoken::decode::<Value>(&get("assertion"), &key, &v) else { return status(401) };
            assert_eq!(claims.claims["iss"], "gateway@p.iam.gserviceaccount.com");
            assert_eq!(claims.claims["scope"], "https://www.googleapis.com/auth/cloud-platform");
            assert_eq!(claims.header.kid.as_deref(), Some("kid-1"));
            return json(200, json!({ "access_token": "ya29.test-token", "expires_in": 3599, "token_type": "Bearer" }));
        }
        if s.header("authorization") != Some("Bearer ya29.test-token") {
            return status(401);
        }
        match s.uri.as_str() {
            "/v1/projects/p/locations/us-east5/publishers/anthropic/models/claude-x:streamRawPredict" => sse(&messages_stream("claude-x")),
            "/v1/projects/p/locations/us-east5/publishers/anthropic/models/claude-x:rawPredict" => json(200, messages_answer("claude-x", 1, 1)),
            "/v1/projects/p/locations/us-east5/endpoints/openapi/chat/completions" => json(200, json!({ "id": "c", "object": "chat.completion", "model": "google/gemini-x", "choices": [] })),
            _ => status(404),
        }
    })
    .await;
    *token_uri.lock().unwrap() = mock.url("/token");
    let mut vertex = upstream("vertex", UpstreamKind::Vertex, &mock.url(""));
    vertex.region = Some("us-east5".into());
    vertex.project = Some("p".into());
    let g = Gw::start(vec![vertex], vec![], &[("vertex", &sa)]).await;
    g.gw.set_vertex_token_uri_for_tests(Some(mock.url("/token")));

    let r = g.post("/v1/messages", &messages("claude-x", true)).await;
    assert_eq!(r.text().await.unwrap(), messages_stream("claude-x").concat());
    assert_eq!(g.post("/v1/messages", &messages("claude-x", false)).await.status(), 200);
    let r = g.post("/v1/chat/completions", &json!({ "model": "google/gemini-x", "messages": [] })).await;
    assert_eq!(r.status(), 200);
    let seen = mock.seen();
    assert_eq!(seen.iter().filter(|s| s.uri == "/token").count(), 1, "the token wasn't cached");
    let claude = seen.iter().find(|s| s.uri.ends_with(":streamRawPredict")).unwrap().json();
    assert_eq!(claude["anthropic_version"], "vertex-2023-10-16");
    assert!(claude.get("model").is_none());
    assert_eq!(claude["stream"], true);
    let checks = g.gw.test(None).await;
    assert!(checks[0].ok, "{checks:?}");
}

#[tokio::test]
async fn vertex_refuses_a_key_that_names_another_token_endpoint() {
    let (sa, _) = service_account();
    let mut v: Value = serde_json::from_str(&sa).unwrap();
    v["token_uri"] = json!("https://evil.example/token");
    let mut vertex = upstream("vertex", UpstreamKind::Vertex, "https://vertex.example");
    vertex.region = Some("us-east5".into());
    vertex.project = Some("p".into());
    let g = Gw::start(vec![vertex], vec![], &[("vertex", &v.to_string())]).await;
    let checks = g.gw.test(Some("vertex")).await;
    assert!(!checks[0].ok && checks[0].error.as_deref().unwrap().contains("token_uri"), "{checks:?}");
}

#[tokio::test]
async fn azure_uses_its_v1_paths_and_api_key_header() {
    let mock = Mock::start(|s| {
        if s.header("api-key") != Some("azure-key") || s.header("authorization").is_some() {
            return status(401);
        }
        match s.uri.as_str() {
            "/openai/v1/chat/completions" => json(200, json!({ "id": "c", "object": "chat.completion", "choices": [] })),
            "/openai/v1/responses" => json(200, json!({ "id": "r", "object": "response", "status": "completed", "output": [] })),
            "/openai/v1/models" => json(200, json!({ "data": [{ "id": "my-gpt-deployment" }] })),
            _ => status(404),
        }
    })
    .await;
    let g = Gw::start(vec![upstream("azure", UpstreamKind::AzureOpenai, &mock.url(""))], vec![], &[("azure", "azure-key")]).await;
    assert_eq!(g.post("/v1/chat/completions", &json!({ "model": "my-gpt-deployment", "messages": [] })).await.status(), 200);
    assert_eq!(g.post("/v1/responses", &json!({ "model": "my-gpt-deployment", "input": "hi" })).await.status(), 200);
    let checks = g.gw.test(None).await;
    assert_eq!((checks[0].ok, checks[0].models), (true, 1), "{checks:?}");
}

#[tokio::test]
async fn test_asks_each_upstream_for_its_models_and_never_generates() {
    let mock = Mock::start(|s| match (s.uri.as_str(), s.header("x-api-key"), s.header("authorization")) {
        ("/v1/models?limit=1000", Some("good"), _) => json(200, json!({ "data": [{ "id": "claude-x" }] })),
        ("/v1/models", _, Some("Bearer good")) => json(200, json!({ "data": [{ "id": "gpt-x" }, { "id": "gpt-y" }] })),
        _ => status(401),
    })
    .await;
    let ups = vec![
        upstream("ant", UpstreamKind::Anthropic, &mock.url("")),
        upstream("oai", UpstreamKind::Openai, &mock.url("/v1")),
        upstream("bad", UpstreamKind::Openrouter, &mock.url("/v1")),
        upstream("nokey", UpstreamKind::Openai, &mock.url("/v1")),
    ];
    let g = Gw::start(ups, vec![], &[("ant", "good"), ("oai", "good"), ("bad", "wrong")]).await;
    let checks = g.gw.test(None).await;
    let got: Vec<(&str, bool, u32)> = checks.iter().map(|c| (c.upstream.as_str(), c.ok, c.models)).collect();
    assert_eq!(got, vec![("ant", true, 1), ("oai", true, 2), ("bad", false, 0), ("nokey", false, 0)]);
    assert!(checks[2].error.as_deref().unwrap().contains("401"));
    assert!(checks[3].error.as_deref().unwrap().contains("no key"));
    assert!(mock.seen().iter().all(|s| s.method == "GET"));
    assert_eq!(g.gw.test(Some("oai")).await.len(), 1);
}
