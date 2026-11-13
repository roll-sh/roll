//! AWS credentials from a container credentials endpoint named by a full URL
//! (`AWS_CONTAINER_CREDENTIALS_FULL_URI`), for the S3 client.
//!
//! `object_store` reads the ECS task-role endpoint (`AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`, at
//! the agent's fixed link-local address) itself, but takes a full URI only together with an
//! authorization token *file* (EKS Pod Identity). Every other runtime that sets a full URI (ECS
//! Anywhere, Greengrass, local emulators, or a token passed in `AWS_CONTAINER_AUTHORIZATION_TOKEN`)
//! silently fell through to the EC2 instance metadata service: the wrong role, or none.
//!
//! [`ContainerCredentials::from_env`] takes over exactly where the AWS SDKs' default chain would
//! reach the container provider with a full URI (no static keys, no web identity, no relative
//! URI), so the order stays the SDKs': environment keys, web identity, container (relative URI
//! first), instance metadata. The authorization header is the token file's content, re-read at
//! every fetch since EKS rotates it, else `AWS_CONTAINER_AUTHORIZATION_TOKEN`, else none. As the
//! search proxy's own task-role path does, any host is accepted: the operator sets the variable.
//!
//! Credentials are cached until five minutes before they expire. A refresh that fails while the
//! cached ones are still valid keeps them (the endpoint restarting must not stall uploads); one
//! fetch runs at a time, concurrent callers wait for it.

use object_store::aws::AwsCredential;
use object_store::client::{HttpClient, HttpConnector, HttpRequest, HttpRequestBody, ReqwestConnector};
use object_store::{ClientOptions, CredentialProvider, HeaderValue};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Refresh this long before the credentials expire.
const REFRESH_BEFORE: Duration = Duration::from_secs(300);
const ATTEMPTS: u32 = 3;

/// The endpoint and where its authorization token comes from.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    pub url: String,
    pub token_file: Option<String>,
    pub token: Option<String>,
}

impl Endpoint {
    /// The endpoint the AWS SDKs would use for a full URI, from an environment lookup; `None`
    /// when an earlier provider of the chain applies or no full URI is set.
    pub fn select(env: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let set = |k: &str| env(k).is_some();
        if set("AWS_ACCESS_KEY_ID") || set("AWS_SECRET_ACCESS_KEY") {
            return None;
        }
        if set("AWS_WEB_IDENTITY_TOKEN_FILE") && set("AWS_ROLE_ARN") {
            return None;
        }
        if set("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
            return None;
        }
        let url = env("AWS_CONTAINER_CREDENTIALS_FULL_URI").filter(|u| !u.is_empty())?;
        let nonempty = |k: &str| env(k).filter(|v| !v.is_empty());
        Some(Self { url, token_file: nonempty("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE"), token: nonempty("AWS_CONTAINER_AUTHORIZATION_TOKEN") })
    }

    async fn authorization(&self) -> anyhow::Result<Option<String>> {
        let token = match &self.token_file {
            Some(f) => Some(tokio::fs::read_to_string(f).await.map_err(|e| anyhow::anyhow!("reading the container authorization token file {f}: {e}"))?),
            None => self.token.clone(),
        };
        let token = token.map(|t| t.trim().to_string());
        // A token with a line break would let it smuggle headers; the SDKs refuse it too.
        anyhow::ensure!(!token.as_deref().is_some_and(|t| t.contains(['\r', '\n'])), "the container authorization token contains a line break");
        Ok(token)
    }
}

#[derive(Debug)]
pub struct ContainerCredentials {
    endpoint: Endpoint,
    client: HttpClient,
    cache: tokio::sync::Mutex<Option<(Arc<AwsCredential>, SystemTime)>>,
}

impl ContainerCredentials {
    /// The provider for this process's environment, if it names a full container credentials
    /// URI that `object_store` wouldn't use on its own (see the module docs).
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        match Endpoint::select(|k| std::env::var(k).ok()) {
            Some(e) => Ok(Some(Self::new(e)?)),
            None => Ok(None),
        }
    }

    pub fn new(endpoint: Endpoint) -> anyhow::Result<Self> {
        let options = ClientOptions::new().with_allow_http(true).with_connect_timeout(Duration::from_secs(2)).with_timeout(Duration::from_secs(5));
        Ok(Self { endpoint, client: ReqwestConnector::default().connect(&options)?, cache: tokio::sync::Mutex::new(None) })
    }

    async fn fetch(&self) -> anyhow::Result<(AwsCredential, SystemTime)> {
        let mut req = HttpRequest::new(HttpRequestBody::empty());
        *req.uri_mut() = self.endpoint.url.parse()?;
        if let Some(t) = self.endpoint.authorization().await? {
            let mut v = HeaderValue::from_str(&t)?;
            v.set_sensitive(true);
            req.headers_mut().insert("authorization", v);
        }
        let res = self.client.execute(req).await?;
        let status = res.status();
        let body = res.into_body().bytes().await?;
        anyhow::ensure!(status.is_success(), "container credentials endpoint: {status}: {}", String::from_utf8_lossy(&body[..body.len().min(512)]));
        parse(&body)
    }

    async fn fetch_with_retries(&self) -> anyhow::Result<(AwsCredential, SystemTime)> {
        let mut last = None;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(100 << attempt)).await;
            }
            match self.fetch().await {
                Ok(c) => return Ok(c),
                Err(e) => last = Some(e),
            }
        }
        Err(last.expect("at least one attempt"))
    }
}

/// The endpoint's JSON (the ECS task credentials shape): `AccessKeyId`, `SecretAccessKey`,
/// `Token`, `Expiration` (RFC 3339).
fn parse(body: &[u8]) -> anyhow::Result<(AwsCredential, SystemTime)> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| anyhow::anyhow!("container credentials endpoint: {e}"))?;
    let s = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(String::from);
    let missing = |k: &str| anyhow::anyhow!("container credentials endpoint: no {k}");
    let expiration = s("Expiration").ok_or_else(|| missing("Expiration"))?;
    let at = chrono::DateTime::parse_from_rfc3339(&expiration).map_err(|e| anyhow::anyhow!("container credentials endpoint: Expiration {expiration:?}: {e}"))?;
    let expires = SystemTime::UNIX_EPOCH + Duration::from_millis(at.timestamp_millis().max(0) as u64);
    let credential = AwsCredential {
        key_id: s("AccessKeyId").ok_or_else(|| missing("AccessKeyId"))?,
        secret_key: s("SecretAccessKey").ok_or_else(|| missing("SecretAccessKey"))?,
        token: s("Token"),
    };
    Ok((credential, expires))
}

#[async_trait::async_trait]
impl CredentialProvider for ContainerCredentials {
    type Credential = AwsCredential;

    async fn get_credential(&self) -> object_store::Result<Arc<AwsCredential>> {
        let mut cache = self.cache.lock().await;
        let now = SystemTime::now();
        if let Some((c, expires)) = &*cache
            && now + REFRESH_BEFORE < *expires
        {
            return Ok(c.clone());
        }
        match self.fetch_with_retries().await {
            Ok((c, expires)) => {
                let c = Arc::new(c);
                *cache = Some((c.clone(), expires));
                Ok(c)
            }
            Err(e) => match &*cache {
                Some((c, expires)) if now < *expires => {
                    tracing::warn!(error = %e, "refreshing container credentials failed; keeping the current ones until they expire");
                    Ok(c.clone())
                }
                _ => Err(object_store::Error::Generic { store: "S3", source: e.into() }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    const FULL: &str = "AWS_CONTAINER_CREDENTIALS_FULL_URI";

    #[test]
    fn takes_over_only_where_the_sdk_chain_reaches_a_full_uri() {
        let url = "http://127.0.0.1:51679/creds";
        let plain = Endpoint { url: url.into(), token_file: None, token: None };
        assert_eq!(Endpoint::select(env(&[(FULL, url)])), Some(plain.clone()));
        assert_eq!(
            Endpoint::select(env(&[(FULL, url), ("AWS_CONTAINER_AUTHORIZATION_TOKEN", "tok")])),
            Some(Endpoint { token: Some("tok".into()), ..plain.clone() })
        );
        assert_eq!(
            Endpoint::select(env(&[(FULL, url), ("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", "/t"), ("AWS_CONTAINER_AUTHORIZATION_TOKEN", "tok")])),
            Some(Endpoint { token_file: Some("/t".into()), token: Some("tok".into()), ..plain.clone() })
        );
        // Earlier providers of the chain win, as in the SDKs and object_store.
        assert_eq!(Endpoint::select(env(&[(FULL, url), ("AWS_ACCESS_KEY_ID", "k"), ("AWS_SECRET_ACCESS_KEY", "s")])), None);
        assert_eq!(Endpoint::select(env(&[(FULL, url), ("AWS_WEB_IDENTITY_TOKEN_FILE", "/w"), ("AWS_ROLE_ARN", "arn")])), None);
        assert_eq!(Endpoint::select(env(&[(FULL, url), ("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/v2/credentials/x")])), None);
        // A web identity file without a role is not a provider; the full URI still applies.
        assert_eq!(Endpoint::select(env(&[(FULL, url), ("AWS_WEB_IDENTITY_TOKEN_FILE", "/w")])), Some(plain));
        assert_eq!(Endpoint::select(env(&[(FULL, "")])), None);
        assert_eq!(Endpoint::select(env(&[])), None);
    }

    #[test]
    fn parses_the_task_credentials_shape() {
        let (c, exp) = parse(br#"{"AccessKeyId":"AKID","SecretAccessKey":"SECRET","Token":"TOKEN","Expiration":"2030-01-02T03:04:05Z","RoleArn":"arn:x"}"#).unwrap();
        assert_eq!((c.key_id.as_str(), c.secret_key.as_str(), c.token.as_deref()), ("AKID", "SECRET", Some("TOKEN")));
        assert_eq!(exp, SystemTime::UNIX_EPOCH + Duration::from_secs(1_893_553_445));
        let (c, _) = parse(br#"{"AccessKeyId":"A","SecretAccessKey":"S","Expiration":"2030-01-02T03:04:05+01:00"}"#).unwrap();
        assert_eq!(c.token, None);
        for bad in [&br#"{"SecretAccessKey":"S","Expiration":"2030-01-02T03:04:05Z"}"#[..], br#"{"AccessKeyId":"A","SecretAccessKey":"S"}"#, br#"{"AccessKeyId":"A","SecretAccessKey":"S","Expiration":"soon"}"#, b"<html>"] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    /// A credentials endpoint answering with `answers` in turn (the last one repeats), recording
    /// each request's `authorization` header.
    struct Fake {
        url: String,
        hits: Arc<AtomicUsize>,
        auth: Arc<Mutex<Vec<Option<String>>>>,
    }

    async fn fake(answers: Vec<(u16, String)>) -> Fake {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/credentials", l.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let auth = Arc::new(Mutex::new(Vec::new()));
        let (h, a) = (hits.clone(), auth.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let header = head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("authorization")).map(|(_, v)| v.trim().to_string()));
                a.lock().unwrap().push(header);
                let i = h.fetch_add(1, Ordering::SeqCst);
                let (status, body) = &answers[i.min(answers.len() - 1)];
                let res = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(res.as_bytes()).await;
            }
        });
        Fake { url, hits, auth }
    }

    fn creds(key: &str, expires_in: i64) -> (u16, String) {
        let at = chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp() + expires_in, 0).unwrap().to_rfc3339();
        (200, format!(r#"{{"AccessKeyId":"{key}","SecretAccessKey":"secret","Token":"session","Expiration":"{at}"}}"#))
    }

    #[tokio::test]
    async fn fetches_with_the_token_and_caches_until_close_to_expiry() {
        let f = fake(vec![creds("FIRST", 3600)]).await;
        let p = ContainerCredentials::new(Endpoint { url: f.url.clone(), token_file: None, token: Some("env-token".into()) }).unwrap();
        let a = p.get_credential().await.unwrap();
        assert_eq!((a.key_id.as_str(), a.token.as_deref()), ("FIRST", Some("session")));
        let b = p.get_credential().await.unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(f.hits.load(Ordering::SeqCst), 1);
        assert_eq!(*f.auth.lock().unwrap(), vec![Some("env-token".to_string())]);

        // Within five minutes of expiry: fetched again on every use until renewed.
        let f = fake(vec![creds("OLD", 120), creds("NEW", 3600)]).await;
        let p = ContainerCredentials::new(Endpoint { url: f.url.clone(), token_file: None, token: None }).unwrap();
        assert_eq!(p.get_credential().await.unwrap().key_id, "OLD");
        assert_eq!(p.get_credential().await.unwrap().key_id, "NEW");
        assert_eq!(p.get_credential().await.unwrap().key_id, "NEW");
        assert_eq!(f.hits.load(Ordering::SeqCst), 2);
        assert_eq!(*f.auth.lock().unwrap(), vec![None, None]);
    }

    #[tokio::test]
    async fn the_token_file_is_read_at_every_fetch() {
        let dir = std::env::temp_dir().join(format!("roll-container-creds-{}-{}", std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("token");
        std::fs::write(&file, "first-token\n").unwrap();
        let f = fake(vec![creds("A", 60)]).await;
        let file_s = file.to_string_lossy().to_string();
        let p = ContainerCredentials::new(Endpoint { url: f.url.clone(), token_file: Some(file_s), token: Some("ignored".into()) }).unwrap();
        p.get_credential().await.unwrap();
        std::fs::write(&file, "rotated-token").unwrap();
        p.get_credential().await.unwrap();
        assert_eq!(*f.auth.lock().unwrap(), vec![Some("first-token".to_string()), Some("rotated-token".to_string())]);
        std::fs::write(&file, "bad\r\nx-injected: 1").unwrap();
        assert!(p.get_credential().await.is_ok(), "cached credentials still valid");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn retries_and_keeps_valid_credentials_when_a_refresh_fails() {
        // Two failures, then an answer: one call succeeds after retrying.
        let f = fake(vec![(500, "{}".into()), (503, "{}".into()), creds("LATE", 3600)]).await;
        let p = ContainerCredentials::new(Endpoint { url: f.url.clone(), token_file: None, token: None }).unwrap();
        assert_eq!(p.get_credential().await.unwrap().key_id, "LATE");
        assert_eq!(f.hits.load(Ordering::SeqCst), 3);

        // Near expiry, the endpoint failing: the cached (still valid) credentials are kept.
        let f = fake(vec![creds("KEPT", 120), (500, "{}".into())]).await;
        let p = ContainerCredentials::new(Endpoint { url: f.url.clone(), token_file: None, token: None }).unwrap();
        assert_eq!(p.get_credential().await.unwrap().key_id, "KEPT");
        assert_eq!(p.get_credential().await.unwrap().key_id, "KEPT");
        assert_eq!(f.hits.load(Ordering::SeqCst), 1 + ATTEMPTS as usize);

        // Nothing cached: the error reaches the caller, with the endpoint's answer.
        let f = fake(vec![(401, r#"{"message":"bad token"}"#.into())]).await;
        let p = ContainerCredentials::new(Endpoint { url: f.url.clone(), token_file: None, token: None }).unwrap();
        let e = p.get_credential().await.unwrap_err().to_string();
        assert!(e.contains("401") && e.contains("bad token"), "{e}");
    }
}
