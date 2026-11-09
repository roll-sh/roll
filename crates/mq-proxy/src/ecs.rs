//! Wake an ECS service (set its desired count) with a SigV4-signed `UpdateService` call, using
//! the task role's credentials. Keeps the proxy image free of the AWS CLI and SDK.

use crate::{BoxFuture, Waker};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct EcsWaker {
    pub cluster: String,
    pub service: String,
    pub desired: u32,
    pub region: String,
    http: reqwest::Client,
}

impl EcsWaker {
    pub fn new(cluster: String, service: String, desired: u32, region: String) -> Self {
        EcsWaker { cluster, service, desired, region, http: reqwest::Client::new() }
    }
}

impl Waker for EcsWaker {
    fn wake(&self) -> BoxFuture<anyhow::Result<()>> {
        let http = self.http.clone();
        let region = self.region.clone();
        let body = json!({ "cluster": self.cluster, "service": self.service, "desiredCount": self.desired }).to_string();
        Box::pin(async move {
            let creds = credentials(&http).await?;
            let host = format!("ecs.{region}.amazonaws.com");
            let target = "AmazonEC2ContainerServiceV20141113.UpdateService";
            let mut headers = vec![
                ("content-type".to_string(), "application/x-amz-json-1.1".to_string()),
                ("host".to_string(), host.clone()),
                ("x-amz-date".to_string(), amz_date(now_s())),
                ("x-amz-target".to_string(), target.to_string()),
            ];
            if let Some(t) = &creds.token {
                headers.push(("x-amz-security-token".into(), t.clone()));
            }
            let req = Request { method: "POST", path: "/", query: "", body: body.as_bytes() };
            let auth = sign(&creds, &region, "ecs", &req, &mut headers);
            let mut req = http.post(format!("https://{host}/")).header("authorization", auth).body(body);
            for (k, v) in &headers {
                if k != "host" {
                    req = req.header(k, v);
                }
            }
            let res = req.send().await?;
            let status = res.status();
            anyhow::ensure!(status.is_success(), "ecs UpdateService: {status}: {}", res.text().await.unwrap_or_default());
            Ok(())
        })
    }
}

pub struct Credentials {
    pub key: String,
    pub secret: String,
    pub token: Option<String>,
}

/// Environment variables, then the ECS task role endpoint.
async fn credentials(http: &reqwest::Client) -> anyhow::Result<Credentials> {
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
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `YYYYMMDDTHHMMSSZ`.
pub fn amz_date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant).
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
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("hmac takes any key length");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// What gets signed: the request line, the (canonical) query string and the body.
pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub body: &'a [u8],
}

/// AWS Signature Version 4: returns the `Authorization` header. `headers` must include `host`
/// and `x-amz-date`; they are sorted in place.
pub fn sign(creds: &Credentials, region: &str, service: &str, req: &Request<'_>, headers: &mut [(String, String)]) -> String {
    let Request { method, path, query, body } = req;
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let date_time = headers.iter().find(|(k, _)| k == "x-amz-date").map(|(_, v)| v.clone()).unwrap_or_default();
    let date = &date_time[..8.min(date_time.len())];
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    let signed = signed.join(";");
    let canonical = format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{}", hex::encode(Sha256::digest(body)));
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{date_time}\n{scope}\n{}", hex::encode(Sha256::digest(canonical.as_bytes())));
    let k = hmac(format!("AWS4{}", creds.secret).as_bytes(), date);
    let k = hmac(&k, region);
    let k = hmac(&k, service);
    let k = hmac(&k, "aws4_request");
    let signature = hex::encode(hmac(&k, &to_sign));
    format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}", creds.key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        assert_eq!(amz_date(1_709_164_800), "20240229T000000Z");
    }

    /// The worked example from AWS's Signature Version 4 documentation.
    #[test]
    fn aws_documentation_example() {
        let creds = Credentials { key: "AKIDEXAMPLE".into(), secret: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(), token: None };
        let mut headers = vec![
            ("x-amz-date".to_string(), "20150830T123600Z".to_string()),
            ("host".to_string(), "iam.amazonaws.com".to_string()),
            ("content-type".to_string(), "application/x-www-form-urlencoded; charset=utf-8".to_string()),
        ];
        let req = Request { method: "GET", path: "/", query: "Action=ListUsers&Version=2010-05-08", body: b"" };
        let auth = sign(&creds, "us-east-1", "iam", &req, &mut headers);
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }
}
