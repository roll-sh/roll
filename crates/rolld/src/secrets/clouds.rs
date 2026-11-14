//! The clouds' token endpoints for dynamic GCP and Azure credentials (roll-secrets' clouds.rs):
//! Google's STS and IAM Credentials, and Microsoft's identity platform. The hosts are fixed (the
//! tenant only names a tenant id, a pool provider and a service account, all checked by
//! roll-secrets), no redirects are followed, and an answer is read up to 64 KiB. Errors are the
//! cloud's own code and description, never a token.

use roll_secrets::Value;
use serde_json::{Value as Json, json};
use std::time::Duration;

const ANSWER_MAX: usize = 64 * 1024;

pub(crate) struct RollClouds {
    client: reqwest::Client,
    sts: String,
    iam: String,
    login: String,
}

impl RollClouds {
    pub fn new() -> RollClouds {
        RollClouds::at("https://sts.googleapis.com", "https://iamcredentials.googleapis.com", "https://login.microsoftonline.com")
    }

    /// The same, against other hosts (tests).
    pub fn at(sts: &str, iam: &str, login: &str) -> RollClouds {
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(15)).build().expect("an HTTP client");
        RollClouds { client, sts: sts.into(), iam: iam.into(), login: login.into() }
    }

    fn wait<T>(f: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
    }
}

/// An answer's JSON, at most [`ANSWER_MAX`]; a failure as the cloud's own words.
async fn answer(resp: reqwest::Response) -> Result<Json, String> {
    let status = resp.status();
    let mut resp = resp;
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.without_url().to_string())? {
        if body.len() + chunk.len() > ANSWER_MAX {
            return Err("an answer too long".into());
        }
        body.extend_from_slice(&chunk);
    }
    let v: Json = serde_json::from_slice(&body).unwrap_or_default();
    if status.is_success() {
        return Ok(v);
    }
    // Google's STS and Microsoft: {error, error_description}; IAM: {error: {status, message}}.
    let why = match &v["error"] {
        Json::String(code) => format!("{code}: {}", v["error_description"].as_str().unwrap_or_default()),
        Json::Object(e) => format!("{}: {}", e.get("status").and_then(Json::as_str).unwrap_or_default(), e.get("message").and_then(Json::as_str).unwrap_or_default()),
        _ => status.to_string(),
    };
    Err(why.chars().take(500).collect())
}

fn expiry_in(v: &Json) -> Result<i64, String> {
    let secs = v["expires_in"].as_i64().ok_or("an answer without expires_in")?;
    Ok(chrono::Utc::now().timestamp_millis() + secs * 1000)
}

impl roll_secrets::clouds::CloudTokens for RollClouds {
    fn gcp(&self, audience: &str, service_account: &str, subject_token: &Value, lifetime: u32) -> Result<(Value, i64), String> {
        Self::wait(async {
            let federated = answer(
                self.client
                    .post(format!("{}/v1/token", self.sts))
                    .json(&json!({
                        "grantType": "urn:ietf:params:oauth:grant-type:token-exchange",
                        "audience": audience,
                        "scope": "https://www.googleapis.com/auth/cloud-platform",
                        "requestedTokenType": "urn:ietf:params:oauth:token-type:access_token",
                        "subjectToken": subject_token.expose(),
                        "subjectTokenType": "urn:ietf:params:oauth:token-type:jwt",
                    }))
                    .send()
                    .await
                    .map_err(|e| format!("Google STS: {}", e.without_url()))?,
            )
            .await?;
            let federated = Value::new(federated["access_token"].as_str().ok_or("Google STS answered without a token")?.to_string());
            let sa = answer(
                self.client
                    .post(format!("{}/v1/projects/-/serviceAccounts/{service_account}:generateAccessToken", self.iam))
                    .bearer_auth(federated.expose())
                    .json(&json!({ "scope": ["https://www.googleapis.com/auth/cloud-platform"], "lifetime": format!("{lifetime}s") }))
                    .send()
                    .await
                    .map_err(|e| format!("IAM Credentials: {}", e.without_url()))?,
            )
            .await?;
            let token = sa["accessToken"].as_str().ok_or("IAM Credentials answered without a token")?;
            let expires = sa["expireTime"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).ok_or("IAM Credentials answered an expiry roll can't read")?;
            Ok((Value::new(token.to_string()), expires.timestamp_millis()))
        })
    }

    fn azure(&self, tenant: &str, client_id: &str, scope: &str, assertion: &Value) -> Result<(Value, i64), String> {
        Self::wait(async {
            let v = answer(
                self.client
                    .post(format!("{}/{tenant}/oauth2/v2.0/token", self.login))
                    .form(&[
                        ("client_id", client_id),
                        ("scope", scope),
                        ("grant_type", "client_credentials"),
                        ("client_assertion_type", "urn:ietf:params:oauth:client-assertion-type:jwt-bearer"),
                        ("client_assertion", assertion.expose()),
                    ])
                    .send()
                    .await
                    .map_err(|e| format!("Microsoft: {}", e.without_url()))?,
            )
            .await?;
            let token = v["access_token"].as_str().ok_or("Microsoft answered without a token")?;
            Ok((Value::new(token.to_string()), expiry_in(&v)?))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roll_secrets::clouds::CloudTokens;
    use std::sync::{Arc, Mutex};

    /// What Google and Microsoft are sent, from a stand-in for both.
    #[tokio::test(flavor = "multi_thread")]
    async fn roll_exchanges_its_token_the_way_each_cloud_takes_it() {
        let seen: Arc<Mutex<Vec<(String, String, String)>>> = Default::default();
        let s = seen.clone();
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri, headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let s = s.clone();
            async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                s.lock().unwrap().push((uri.path().to_string(), auth, String::from_utf8_lossy(&body).into_owned()));
                let reply = match uri.path() {
                    "/v1/token" => json!({ "access_token": "federated", "expires_in": 3600 }),
                    p if p.ends_with(":generateAccessToken") => json!({ "accessToken": "ya29.sa", "expireTime": "2030-01-01T00:00:00Z" }),
                    p if p.starts_with("/denied/") => return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({ "error": "invalid_client", "error_description": "AADSTS70021: No matching federated identity record" }))),
                    _ => json!({ "access_token": "eyJ.azure", "expires_in": 3599 }),
                };
                (axum::http::StatusCode::OK, axum::Json(reply))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let clouds = RollClouds::at(&base, &base, &base);
        let jwt = Value::new("roll.signed.jwt".to_string());

        let (token, expires) = clouds.gcp("//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/p/providers/roll", "r@p.iam.gserviceaccount.com", &jwt, 1200).unwrap();
        assert_eq!((token.expose(), expires), ("ya29.sa", 1_893_456_000_000));
        let (token, _) = clouds.azure("contoso.onmicrosoft.com", "11111111-2222-3333-4444-555555555555", "https://management.azure.com/.default", &jwt).unwrap();
        assert_eq!(token.expose(), "eyJ.azure");
        let why = clouds.azure("denied", "11111111-2222-3333-4444-555555555555", "https://management.azure.com/.default", &jwt).unwrap_err();
        assert!(why.starts_with("invalid_client: AADSTS70021"), "{why}");

        let seen = seen.lock().unwrap();
        let sts: Json = serde_json::from_str(&seen[0].2).unwrap();
        assert_eq!((sts["subjectToken"].as_str(), sts["subjectTokenType"].as_str()), (Some("roll.signed.jwt"), Some("urn:ietf:params:oauth:token-type:jwt")));
        assert_eq!((seen[1].0.as_str(), seen[1].1.as_str()), ("/v1/projects/-/serviceAccounts/r@p.iam.gserviceaccount.com:generateAccessToken", "Bearer federated"));
        assert!(seen[1].2.contains("\"lifetime\":\"1200s\""));
        assert_eq!(seen[2].0, "/contoso.onmicrosoft.com/oauth2/v2.0/token");
        assert!(seen[2].2.contains("client_assertion=roll.signed.jwt") && seen[2].2.contains("grant_type=client_credentials") && seen[2].2.contains("jwt-bearer"));
    }
}
