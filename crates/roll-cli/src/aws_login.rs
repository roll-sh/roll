//! An AWS identity's login (docs/secrets.md, "AWS IAM"): the machine's own AWS credentials sign an
//! `sts:GetCallerIdentity` that roll sends to STS, as Vault's AWS auth (IAM method) does. Nothing
//! is sent to AWS from here except to get credentials; the signed request goes to roll.
//!
//! Credentials come from the standard chain: the environment, the shared files' profile (static
//! keys), a web identity token (IRSA), the container endpoint (ECS task roles, EKS Pod Identity),
//! then the instance's metadata service (IMDSv2). Signing is Signature Version 4 by hand.

use anyhow::{Context, bail};
use hmac::Mac as _;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::time::Duration;
use zeroize::Zeroizing;

pub const BODY: &str = "Action=GetCallerIdentity&Version=2011-06-15";
const CONTENT_TYPE: &str = "application/x-www-form-urlencoded; charset=utf-8";

pub struct Credentials {
    pub key: String,
    pub secret: Zeroizing<String>,
    pub token: Option<String>,
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], msg: &str) -> Vec<u8> {
    let mut m = hmac::Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    m.update(msg.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// The `Authorization` header of a Signature Version 4 request: `headers` are lowercase names and
/// values, all of them signed; `query` is already canonical.
#[allow(clippy::too_many_arguments)]
pub fn authorization(c: &Credentials, region: &str, service: &str, method: &str, path: &str, query: &str, headers: &[(&str, &str)], payload: &str, amz_date: &str) -> String {
    let mut headers = headers.to_vec();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed: Vec<&str> = headers.iter().map(|(k, _)| *k).collect();
    let signed = signed.join(";");
    let request = format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{}", hex(&Sha256::digest(payload.as_bytes())));
    let day = &amz_date[..8];
    let scope = format!("{day}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex(&Sha256::digest(request.as_bytes())));
    let key = Zeroizing::new(format!("AWS4{}", c.secret.as_str()));
    let k = hmac(&hmac(&hmac(&hmac(key.as_bytes(), day), region), service), "aws4_request");
    format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={}", c.key, hex(&hmac(&k, &to_sign)))
}

/// STS's endpoint for `region` and the region it signs for: the global endpoint without one.
pub fn sts_endpoint(region: Option<&str>) -> (String, String) {
    match region {
        None => ("sts.amazonaws.com".into(), "us-east-1".into()),
        Some(r) if r.starts_with("cn-") => (format!("sts.{r}.amazonaws.com.cn"), r.into()),
        Some(r) => (format!("sts.{r}.amazonaws.com"), r.into()),
    }
}

/// A signed `GetCallerIdentity` for roll to send on: `{method, url, headers, body}`, naming
/// `server_id` in `X-Roll-Server-ID` (signed, so it can't be replayed at another server).
pub fn signed_request(c: &Credentials, region: Option<&str>, server_id: &str, amz_date: &str) -> Value {
    let (host, signing_region) = sts_endpoint(region);
    let mut headers = vec![("content-type", CONTENT_TYPE), ("host", host.as_str()), ("x-amz-date", amz_date), ("x-roll-server-id", server_id)];
    if let Some(t) = &c.token {
        headers.push(("x-amz-security-token", t.as_str()));
    }
    let auth = authorization(c, &signing_region, "sts", "POST", "/", "", &headers, BODY, amz_date);
    let mut out = serde_json::Map::new();
    for (k, v) in headers {
        out.insert(k.to_string(), json!(v));
    }
    out.insert("authorization".into(), json!(auth));
    json!({ "method": "POST", "url": format!("https://{host}/"), "headers": out, "body": BODY })
}

/// Now as `YYYYMMDDTHHMMSSZ`.
fn amz_now() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

/// The region from `AWS_REGION`, `AWS_DEFAULT_REGION` or the profile's config.
fn region() -> Option<String> {
    env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION")).or_else(|| profile_value(&config_file(), &config_section(), "region"))
}

fn profile_name() -> String {
    env("AWS_PROFILE").or_else(|| env("AWS_DEFAULT_PROFILE")).unwrap_or_else(|| "default".into())
}

fn home_file(var: &str, rel: &str) -> Option<std::path::PathBuf> {
    env(var).map(Into::into).or_else(|| env("HOME").map(|h| std::path::Path::new(&h).join(rel)))
}

fn config_file() -> Option<std::path::PathBuf> {
    home_file("AWS_CONFIG_FILE", ".aws/config")
}

fn config_section() -> String {
    match profile_name().as_str() {
        "default" => "default".into(),
        p => format!("profile {p}"),
    }
}

/// `key` in `[section]` of an AWS shared file.
fn profile_value(file: &Option<std::path::PathBuf>, section: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(file.as_ref()?).ok()?;
    let mut inside = false;
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            inside = name.trim() == section;
        } else if inside
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim().to_string()).filter(|v| !v.is_empty());
        }
    }
    None
}

/// An element's text in an STS answer.
fn element(doc: &str, tag: &str) -> Option<String> {
    let start = doc.find(&format!("<{tag}>"))? + tag.len() + 2;
    let len = doc[start..].find(&format!("</{tag}>"))?;
    Some(doc[start..start + len].trim().to_string())
}

/// What the container endpoint and IMDS answer.
fn from_json(v: &Value, what: &str) -> anyhow::Result<Credentials> {
    let s = |k: &str| v[k].as_str().map(str::to_string);
    Ok(Credentials { key: s("AccessKeyId").with_context(|| format!("{what} answered no AccessKeyId"))?, secret: Zeroizing::new(s("SecretAccessKey").with_context(|| format!("{what} answered no SecretAccessKey"))?), token: s("Token") })
}

/// The container endpoint's address may only be local, or HTTPS (as the AWS SDKs allow it).
fn container_url_ok(u: &url::Url) -> bool {
    if u.scheme() == "https" {
        return true;
    }
    u.scheme() == "http"
        && match u.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip == std::net::Ipv4Addr::new(169, 254, 170, 2) || ip == std::net::Ipv4Addr::new(169, 254, 170, 23),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback() || ip == "fd00:ec2::23".parse::<std::net::Ipv6Addr>().expect("an address"),
            Some(url::Host::Domain(d)) => d == "localhost",
            None => false,
        }
}

/// The machine's AWS credentials, from the standard chain.
pub async fn credentials(http: &reqwest::Client) -> anyhow::Result<Credentials> {
    if let (Some(key), Some(secret)) = (env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY")) {
        return Ok(Credentials { key, secret: Zeroizing::new(secret), token: env("AWS_SESSION_TOKEN") });
    }
    let named = env("AWS_PROFILE").is_some();
    let shared = home_file("AWS_SHARED_CREDENTIALS_FILE", ".aws/credentials");
    let (cfg, section) = (config_file(), config_section());
    let value = |k: &str| profile_value(&shared, &profile_name(), k).or_else(|| profile_value(&cfg, &section, k));
    if let (Some(key), Some(secret)) = (value("aws_access_key_id"), value("aws_secret_access_key")) {
        return Ok(Credentials { key, secret: Zeroizing::new(secret), token: value("aws_session_token") });
    }
    if named && env("AWS_WEB_IDENTITY_TOKEN_FILE").is_none() {
        bail!("AWS profile {} has no access keys: roll reads a profile's static keys (for SSO, `eval \"$(aws configure export-credentials --format env)\"` first)", profile_name());
    }
    // IRSA: the pod's projected token for the role, exchanged with STS (unsigned).
    if let (Some(file), Some(role)) = (env("AWS_WEB_IDENTITY_TOKEN_FILE"), env("AWS_ROLE_ARN")) {
        let token = std::fs::read_to_string(&file).with_context(|| format!("reading AWS_WEB_IDENTITY_TOKEN_FILE {file}"))?;
        let session = env("AWS_ROLE_SESSION_NAME").unwrap_or_else(|| format!("roll-{}", chrono::Utc::now().timestamp()));
        let (host, _) = sts_endpoint(region().as_deref());
        let form = [("Action", "AssumeRoleWithWebIdentity"), ("Version", "2011-06-15"), ("RoleArn", role.as_str()), ("RoleSessionName", session.as_str()), ("WebIdentityToken", token.trim())];
        let body: String = form.iter().map(|(k, v)| format!("{k}={}", urlencoding::encode(v))).collect::<Vec<_>>().join("&");
        let r = http.post(format!("https://{host}/")).header("content-type", CONTENT_TYPE).body(body).send().await.context("asking STS for the web identity's credentials")?;
        let status = r.status();
        let doc = r.text().await?;
        if !status.is_success() {
            bail!("STS refused the web identity ({status}): {}", element(&doc, "Code").unwrap_or_default());
        }
        let creds = element(&doc, "Credentials").context("STS answered no credentials for the web identity")?;
        let get = |k: &str| element(&creds, k).with_context(|| format!("STS's credentials have no {k}"));
        return Ok(Credentials { key: get("AccessKeyId")?, secret: Zeroizing::new(get("SecretAccessKey")?), token: Some(get("SessionToken")?) });
    }
    // ECS task roles (relative) and EKS Pod Identity (full, with a token).
    let container = env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").map(|p| format!("http://169.254.170.2{p}")).or_else(|| env("AWS_CONTAINER_CREDENTIALS_FULL_URI"));
    if let Some(raw) = container {
        let u = url::Url::parse(&raw).with_context(|| format!("the container credentials endpoint {raw}"))?;
        if !container_url_ok(&u) {
            bail!("the container credentials endpoint is local or https, not {raw}");
        }
        let mut req = http.get(u).timeout(Duration::from_secs(5));
        let auth = match env("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE") {
            Some(f) => Some(std::fs::read_to_string(&f).with_context(|| format!("reading AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE {f}"))?.trim().to_string()),
            None => env("AWS_CONTAINER_AUTHORIZATION_TOKEN"),
        };
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        let r = req.send().await.context("asking the container endpoint for credentials")?;
        anyhow::ensure!(r.status().is_success(), "the container credentials endpoint answered {}", r.status());
        return from_json(&r.json().await?, "the container endpoint");
    }
    if env("AWS_EC2_METADATA_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
        bail!("no AWS credentials (and the instance metadata service is off)");
    }
    imds(http).await.context("no AWS credentials in the environment, a profile, a web identity or a container, and the instance metadata service")
}

/// IMDSv2: a session token, the instance profile's role, its credentials.
async fn imds(http: &reqwest::Client) -> anyhow::Result<Credentials> {
    let base = env("AWS_EC2_METADATA_SERVICE_ENDPOINT").unwrap_or_else(|| "http://169.254.169.254".into());
    let base = base.trim_end_matches('/');
    let t = Duration::from_secs(2);
    let token = http.put(format!("{base}/latest/api/token")).header("x-aws-ec2-metadata-token-ttl-seconds", "300").timeout(t).send().await?.error_for_status()?.text().await?;
    let get = |p: &str| http.get(format!("{base}/latest/meta-data/iam/security-credentials/{p}")).header("x-aws-ec2-metadata-token", token.as_str()).timeout(t).send();
    let roles = get("").await?.error_for_status()?.text().await?;
    let role = roles.lines().next().map(str::trim).filter(|r| !r.is_empty()).context("the instance has no instance profile")?;
    from_json(&get(role).await?.error_for_status()?.json().await?, "the instance metadata service")
}

/// The login's `aws` field: a fresh signed request with the machine's credentials, naming
/// `ROLL_AWS_SERVER_ID`, else the API's URL.
pub async fn login_request(http: &reqwest::Client, api_base: &str) -> anyhow::Result<Value> {
    let creds = credentials(http).await?;
    let server_id = env("ROLL_AWS_SERVER_ID").unwrap_or_else(|| api_base.trim_end_matches('/').to_string());
    Ok(signed_request(&creds, region().as_deref(), &server_id, &amz_now()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's published example (the IAM ListUsers request in "Examples of the complete Signature
    /// Version 4 signing process"), with its documented example keys.
    #[test]
    fn signs_as_aws_published_example() {
        let c = Credentials { key: "AKIDEXAMPLE".into(), secret: Zeroizing::new("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into()), token: None };
        let headers = [("content-type", CONTENT_TYPE), ("host", "iam.amazonaws.com"), ("x-amz-date", "20150830T123600Z")];
        let auth = authorization(&c, "us-east-1", "iam", "GET", "/", "Action=ListUsers&Version=2010-05-08", &headers, "", "20150830T123600Z");
        assert_eq!(auth, "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7");
    }

    #[test]
    fn a_login_request_signs_the_server_id_and_the_session_token() {
        let c = Credentials { key: "AKIAIOSFODNN7EXAMPLE".into(), secret: Zeroizing::new("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into()), token: Some("EXAMPLETOKEN".into()) };
        let r = signed_request(&c, Some("eu-west-1"), "https://roll.example.com", "20261118T120000Z");
        assert_eq!(r["url"], "https://sts.eu-west-1.amazonaws.com/");
        assert_eq!(r["body"], BODY);
        let auth = r["headers"]["authorization"].as_str().unwrap();
        assert!(auth.contains("/20261118/eu-west-1/sts/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-security-token;x-roll-server-id, "), "{auth}");
        assert_eq!(sts_endpoint(None), ("sts.amazonaws.com".to_string(), "us-east-1".to_string()));
        assert_eq!(sts_endpoint(Some("cn-north-1")).0, "sts.cn-north-1.amazonaws.com.cn");
    }
}
