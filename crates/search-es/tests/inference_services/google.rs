//! `googlevertexai` (service account → OAuth token, embeddings, rerank, Gemini completions and
//! chat) and `googleaistudio` (embeddings, completions); create, infer, update, delete and the
//! error cases.

use crate::*;
use base64::Engine;

#[tokio::test]
async fn google_services_match_elasticsearch_and_hide_their_credentials() {
    let r = replay("google", false).await;
    assert_none(&r.problems);
    assert_secrets_hidden(&r, &["PRIVATE KEY", "private_key", "gs-key-1", "gs-key-2", "gs-key-3", "gs-new"]);
}

/// Vertex AI trades a JWT signed with the service account's key for a token at Google's fixed
/// token endpoint, once per endpoint while the token lasts.
#[tokio::test]
async fn vertex_ai_signs_a_jwt_and_reuses_the_token() {
    let m = mock().await;
    let c = cluster(&m).await;
    let put = format!(r#"{{"service":"googlevertexai","service_settings":{{"service_account_json":{},"project_id":"p","model_id":"m"}}}}"#, service_account());
    let _ = call(&c, "PUT", "/_inference/rerank/vx", Some(&put)).await;
    let _ = call(&c, "POST", "/_inference/rerank/vx", Some(r#"{"input":["a"],"query":"q"}"#)).await;
    let seen = m.take();
    let tokens: Vec<&Seen> = seen.iter().filter(|s| s.path == "/token").collect();
    assert_eq!(tokens.len(), 1, "{seen:?}");
    assert_eq!(tokens[0].header("content-type"), Some("application/x-www-form-urlencoded; charset=UTF-8"));
    let jwt = tokens[0].body.strip_prefix("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion=").unwrap();
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(jwt.split('.').nth(1).unwrap()).unwrap();
    let claims = search_es::json::parse(&claims).unwrap().unwrap();
    assert_eq!(claims.get("iss").map(|v| v.to_es_string()).as_deref(), Some("rinf@rinf-proj.iam.gserviceaccount.com"));
    assert_eq!(claims.get("aud").map(|v| v.to_es_string()).as_deref(), Some("https://oauth2.googleapis.com/token"));
    assert_eq!(claims.get("scope").map(|v| v.to_es_string()).as_deref(), Some("https://www.googleapis.com/auth/cloud-platform"));
    assert!(seen.iter().filter(|s| s.path != "/token").all(|s| s.header("authorization") == Some("Bearer ya29.mock-token")));
}
