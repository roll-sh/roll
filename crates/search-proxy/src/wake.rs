//! Starting search nodes when none runs (the fleet scaled to zero).
//!
//! [`EcsWaker`] raises the node service's desired count through the ECS API: `DescribeServices`
//! first, so a wake never *lowers* a count that autoscaling or another proxy already raised,
//! then `UpdateService`. Calls are SigV4-signed with the task role's credentials (or the usual
//! environment variables), which keeps the proxy image free of the AWS CLI and SDK. The
//! endpoint can be overridden (`AWS_ENDPOINT_URL_ECS`), which the tests use for a fake ECS.
//! [`CommandWaker`] runs a shell command instead (other platforms, scripts).

use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Starts nodes when none is running.
pub trait Waker: Send + Sync + 'static {
    fn wake(&self) -> BoxFuture<anyhow::Result<()>>;
}

/// Runs a shell command, e.g. `aws ecs update-service --cluster roll --service search --desired-count 1`.
pub struct CommandWaker(pub String);

impl Waker for CommandWaker {
    fn wake(&self) -> BoxFuture<anyhow::Result<()>> {
        let cmd = self.0.clone();
        Box::pin(async move {
            let status = tokio::process::Command::new("sh").arg("-c").arg(&cmd).status().await?;
            anyhow::ensure!(status.success(), "wake command exited with {status}");
            Ok(())
        })
    }
}

#[derive(Clone, Debug)]
pub struct Credentials {
    pub key: String,
    pub secret: String,
    pub token: Option<String>,
}

/// Scales an ECS service up to `desired` tasks.
pub struct EcsWaker {
    pub cluster: String,
    pub service: String,
    pub desired: u32,
    pub region: String,
    /// `https://ecs.<region>.amazonaws.com` unless overridden.
    pub endpoint: String,
    credentials: Option<Credentials>,
    http: reqwest::Client,
}

impl EcsWaker {
    pub fn new(cluster: String, service: String, desired: u32, region: String) -> EcsWaker {
        let endpoint = std::env::var("AWS_ENDPOINT_URL_ECS").unwrap_or_else(|_| format!("https://ecs.{region}.amazonaws.com"));
        EcsWaker { cluster, service, desired: desired.max(1), region, endpoint, credentials: None, http: reqwest::Client::new() }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> EcsWaker {
        self.endpoint = endpoint.into();
        self
    }

    /// Fixed credentials instead of the environment / task role.
    pub fn with_credentials(mut self, c: Credentials) -> EcsWaker {
        self.credentials = Some(c);
        self
    }

    async fn call(&self, action: &str, body: &Value) -> anyhow::Result<Value> {
        let creds = match &self.credentials {
            Some(c) => c.clone(),
            None => credentials(&self.http).await?,
        };
        let url = reqwest::Url::parse(&self.endpoint)?;
        let host = match url.port() {
            Some(p) => format!("{}:{p}", url.host_str().unwrap_or_default()),
            None => url.host_str().unwrap_or_default().to_string(),
        };
        let body = body.to_string();
        let mut headers = vec![
            ("content-type".to_string(), "application/x-amz-json-1.1".to_string()),
            ("host".to_string(), host),
            ("x-amz-date".to_string(), amz_date(now_s())),
            ("x-amz-target".to_string(), format!("AmazonEC2ContainerServiceV20141113.{action}")),
        ];
        if let Some(t) = &creds.token {
            headers.push(("x-amz-security-token".into(), t.clone()));
        }
        let req = Request { method: "POST", path: "/", query: "", body: body.as_bytes() };
        let auth = sign(&creds, &self.region, "ecs", &req, &mut headers);
        let mut r = self.http.post(url).header("authorization", auth).body(body);
        for (k, v) in &headers {
            if k != "host" {
                r = r.header(k, v);
            }
        }
        let res = r.send().await?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        anyhow::ensure!(status.is_success(), "ecs {action}: {status}: {text}");
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

impl Waker for EcsWaker {
    fn wake(&self) -> BoxFuture<anyhow::Result<()>> {
        let me = EcsWaker {
            cluster: self.cluster.clone(),
            service: self.service.clone(),
            desired: self.desired,
            region: self.region.clone(),
            endpoint: self.endpoint.clone(),
            credentials: self.credentials.clone(),
            http: self.http.clone(),
        };
        Box::pin(async move {
            let d = me.call("DescribeServices", &json!({ "cluster": me.cluster, "services": [me.service] })).await?;
            let current = d["services"][0]["desiredCount"].as_u64();
            if current.is_some_and(|c| c >= me.desired as u64) {
                tracing::debug!(current, "search node service already scaled up");
                return Ok(());
            }
            me.call("UpdateService", &json!({ "cluster": me.cluster, "service": me.service, "desiredCount": me.desired })).await?;
            tracing::info!(service = %me.service, desired = me.desired, "woke the search node service");
            Ok(())
        })
    }
}

/// Environment variables, then the ECS container credentials endpoint (the task role).
pub async fn credentials(http: &reqwest::Client) -> anyhow::Result<Credentials> {
    if let (Ok(key), Ok(secret)) = (std::env::var("AWS_ACCESS_KEY_ID"), std::env::var("AWS_SECRET_ACCESS_KEY")) {
        return Ok(Credentials { key, secret, token: std::env::var("AWS_SESSION_TOKEN").ok() });
    }
    let url = if let Ok(rel) = std::env::var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
        format!("http://169.254.170.2{rel}")
    } else if let Ok(full) = std::env::var("AWS_CONTAINER_CREDENTIALS_FULL_URI") {
        full
    } else {
        anyhow::bail!("no AWS credentials: set AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY or run as an ECS task with a role");
    };
    let mut req = http.get(url);
    if let Ok(t) = std::env::var("AWS_CONTAINER_AUTHORIZATION_TOKEN") {
        req = req.header("authorization", t);
    } else if let Ok(f) = std::env::var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE") {
        req = req.header("authorization", std::fs::read_to_string(f)?.trim().to_string());
    }
    let v: Value = req.send().await?.error_for_status()?.json().await?;
    let s = |k: &str| v[k].as_str().map(String::from);
    Ok(Credentials {
        key: s("AccessKeyId").ok_or_else(|| anyhow::anyhow!("credentials endpoint: no AccessKeyId"))?,
        secret: s("SecretAccessKey").ok_or_else(|| anyhow::anyhow!("credentials endpoint: no SecretAccessKey"))?,
        token: s("Token"),
    })
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `YYYYMMDDTHHMMSSZ` for Unix seconds (proleptic Gregorian, from days since the epoch).
pub fn amz_date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes keys of any length");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// What SigV4 signs besides the headers.
pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// Already canonical (sorted, encoded).
    pub query: &'a str,
    pub body: &'a [u8],
}

/// AWS Signature Version 4: returns the `Authorization` header value. `headers` (lowercase
/// names) must include `host` and `x-amz-date`; they are sorted in place.
pub fn sign(creds: &Credentials, region: &str, service: &str, req: &Request<'_>, headers: &mut [(String, String)]) -> String {
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let date_time = headers.iter().find(|(k, _)| k == "x-amz-date").map(|(_, v)| v.as_str()).unwrap_or_default();
    let date = &date_time[..8.min(date_time.len())];
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical = format!("{}\n{}\n{}\n{canonical_headers}\n{signed}\n{}", req.method, req.path, req.query, hex::encode(Sha256::digest(req.body)));
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{date_time}\n{scope}\n{}", hex::encode(Sha256::digest(canonical.as_bytes())));
    let k = hmac(format!("AWS4{}", creds.secret).as_bytes(), date);
    let k = hmac(&k, region);
    let k = hmac(&k, service);
    let k = hmac(&k, "aws4_request");
    format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={}", creds.key, hex::encode(hmac(&k, &to_sign)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        assert_eq!(amz_date(1_709_164_800), "20240229T000000Z");
        assert_eq!(amz_date(4_102_444_799), "20991231T235959Z");
    }

    /// The `get-vanilla-query` style example from AWS's Signature Version 4 documentation
    /// (IAM ListUsers).
    #[test]
    fn aws_documentation_example() {
        let creds = Credentials { key: "AKIDEXAMPLE".into(), secret: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(), token: None };
        let mut headers = vec![
            ("x-amz-date".to_string(), "20150830T123600Z".to_string()),
            ("host".to_string(), "iam.amazonaws.com".to_string()),
            ("content-type".to_string(), "application/x-www-form-urlencoded; charset=utf-8".to_string()),
        ];
        let req = Request { method: "GET", path: "/", query: "Action=ListUsers&Version=2010-05-08", body: b"" };
        assert_eq!(
            sign(&creds, "us-east-1", "iam", &req, &mut headers),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    #[tokio::test]
    async fn command_waker() {
        assert!(CommandWaker("true".into()).wake().await.is_ok());
        assert!(CommandWaker("exit 3".into()).wake().await.is_err());
    }
}
