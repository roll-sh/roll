//! A small Google Cloud Storage client for buckets a tenant names (the `gcs` snapshot
//! repository), over the JSON API: objects are read (whole, or streamed and ranged), written
//! (optionally only when absent or at a generation, `ifGenerationMatch`), written in parts with
//! resumable uploads, deleted and listed.
//!
//! Requests carry an OAuth access token for the repository's service account: a JWT signed with
//! the account's key (RS256, `jsonwebtoken`) is exchanged at the account's `token_uri`, and the
//! token is kept until five minutes before it expires ([`TOKENS`], keyed by a digest of the
//! account, never by anything printable). A repository without a service account reads and
//! writes anonymously: roll's own credentials (instance metadata, application default
//! credentials) are never used for a tenant's bucket.
//!
//! The endpoint and the token URI are the tenant's choice, so the client is built as AGENTS.md
//! requires for any such URL, through the node's egress check ([`crate::egress`]): names
//! resolve to public addresses only, never to one the node denies; private and denied IP
//! literals and `localhost` are refused; no
//! redirect is followed, no proxy is used, and every response body is read up to a bound. A
//! resumable upload's session URL comes back from the endpoint and is used only on the
//! endpoint's own origin. Errors carry no URL (a session URL is a capability) and no credential.

use crate::s3::{ATTEMPTS, Download, ERROR_BODY_MAX, Error, LIST_ENTRIES_MAX, LIST_KEY_BYTES_MAX, LIST_PAGES_MAX, READ_IDLE, REQUEST_MAX, err, read_bounded, uri_encode};
use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Google's JSON API and token endpoints.
pub const DEFAULT_ENDPOINT: &str = "https://storage.googleapis.com";
pub const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/devstorage.full_control";
/// A resumable upload's chunks other than the last are multiples of this.
pub const CHUNK_ALIGN: u64 = 256 << 10;
/// The largest service account key file accepted.
pub const CREDENTIALS_MAX: usize = 64 << 10;
/// A token is fetched again this long before it expires.
const TOKEN_MARGIN: Duration = Duration::from_secs(300);
/// Cached tokens, at most.
const TOKENS_MAX: usize = 4096;

/// A service account (the `credentials_file` JSON). Never printed.
#[derive(Clone)]
pub struct ServiceAccount {
    pub client_email: String,
    key: jsonwebtoken::EncodingKey,
    key_id: Option<String>,
    pub token_uri: reqwest::Url,
    /// What identifies the account in the token cache.
    digest: [u8; 32],
}

impl std::fmt::Debug for ServiceAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ServiceAccount(..)")
    }
}

impl ServiceAccount {
    /// Reads a service account key file. `token_uri` (the client setting) overrides the file's.
    /// No message carries anything from the file.
    pub fn parse(json: &str, token_uri: Option<&str>, egress: &crate::egress::Egress) -> Result<ServiceAccount, String> {
        const BAD: &str = "the credentials_file isn't a service account key file";
        if json.len() > CREDENTIALS_MAX {
            return Err(format!("the credentials_file is larger than [{CREDENTIALS_MAX}] bytes"));
        }
        let v: serde_json::Value = serde_json::from_str(json).map_err(|_| BAD.to_string())?;
        let field = |k: &str| v.get(k).and_then(serde_json::Value::as_str).filter(|s| !s.is_empty());
        if field("type") != Some("service_account") {
            return Err(format!("{BAD}: its [type] must be [service_account]"));
        }
        let client_email = field("client_email").ok_or_else(|| format!("{BAD}: it has no [client_email]"))?.to_string();
        let pem = field("private_key").ok_or_else(|| format!("{BAD}: it has no [private_key]"))?;
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).map_err(|_| format!("{BAD}: its [private_key] isn't an RSA private key"))?;
        let raw_uri = token_uri.filter(|u| !u.is_empty()).or(field("token_uri")).unwrap_or(DEFAULT_TOKEN_URI);
        let uri = url(raw_uri, true, egress).map_err(|e| format!("invalid token_uri: {e}"))?;
        let mut h = Sha256::new();
        for part in [client_email.as_str(), pem, uri.as_str()] {
            h.update(part.as_bytes());
            h.update([0]);
        }
        Ok(ServiceAccount { client_email, key, key_id: field("private_key_id").map(str::to_string), token_uri: uri, digest: h.finalize().into() })
    }
}

/// A URL a tenant names: http(s), a host, no credentials, query or fragment; a path only when
/// `path` allows one. An address written out that `egress` refuses (private, denied) is refused.
fn url(raw: &str, path: bool, egress: &crate::egress::Egress) -> Result<reqwest::Url, String> {
    let u = reqwest::Url::parse(raw.trim()).map_err(|_| format!("[{raw}] isn't a URL"))?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none_or(str::is_empty) {
        return Err(format!("[{raw}] isn't an http or https URL"));
    }
    if !u.username().is_empty() || u.password().is_some() || u.query().is_some() || u.fragment().is_some() || (!path && !matches!(u.path(), "" | "/")) {
        return Err(format!("[{raw}] must be a scheme, host and port only"));
    }
    match egress.check_url(&u) {
        Err(crate::egress::Refused::Private(_)) => Err(format!("[{raw}] is a private address")),
        Err(e) => Err(format!("[{raw}]: {e}")),
        Ok(()) => Ok(u),
    }
}

/// The JSON API's base (`endpoint`, Google's by default).
pub fn endpoint(raw: Option<&str>, egress: &crate::egress::Egress) -> Result<reqwest::Url, String> {
    let raw = raw.map(str::trim).filter(|e| !e.is_empty()).unwrap_or(DEFAULT_ENDPOINT);
    let mut u = url(raw, false, egress).map_err(|e| format!("invalid endpoint {e}"))?;
    u.set_path("");
    Ok(u)
}

/// GCS's bucket naming rules: 3-222 lowercase letters, digits, dashes, underscores and dots
/// (dot-separated parts of at most 63), starting and ending with a letter or digit.
pub fn valid_bucket(b: &str) -> bool {
    let bytes = b.as_bytes();
    (3..=222).contains(&bytes.len())
        && bytes.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_' | b'.'))
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && b.split('.').all(|p| !p.is_empty() && p.len() <= 63)
}

/// Access tokens by account digest, with when they expire.
type Tokens = HashMap<[u8; 32], (String, Instant)>;
static TOKENS: LazyLock<parking_lot::Mutex<Tokens>> = LazyLock::new(Default::default);

/// A GCS bucket.
pub struct Client {
    http: reqwest::Client,
    endpoint: reqwest::Url,
    bucket: String,
    account: Option<ServiceAccount>,
    user_agent: Option<String>,
}

impl Client {
    /// The URLs it connects to as written (names reach the egress resolver; literals don't, so
    /// callers check these with [`crate::egress::Egress::check_url_now`]).
    pub fn endpoints(&self) -> Vec<&reqwest::Url> {
        std::iter::once(&self.endpoint).chain(self.account.as_ref().map(|a| &a.token_uri)).collect()
    }

    /// A client for `bucket` at `endpoint`. Addresses `egress` refuses are refused (as literals
    /// here, resolved ones at connect time).
    pub fn new(endpoint: reqwest::Url, bucket: &str, account: Option<ServiceAccount>, application_name: Option<&str>, egress: &crate::egress::Egress) -> Result<Client, Error> {
        if !valid_bucket(bucket) {
            return Err(err("InvalidBucketName", format!("invalid bucket name [{bucket}]")));
        }
        for u in std::iter::once(&endpoint).chain(account.as_ref().map(|a| &a.token_uri)) {
            match egress.check_url(u) {
                Err(crate::egress::Refused::Private(_)) => return Err(err("PrivateAddress", "the endpoint is a private address")),
                Err(e) => return Err(err("DeniedAddress", format!("the endpoint: {e}"))),
                Ok(()) => {}
            }
        }
        let b = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).read_timeout(READ_IDLE).redirect(reqwest::redirect::Policy::none()).no_proxy().dns_resolver(egress.resolver());
        let http = b.build().map_err(|e| err("ClientError", e.to_string()))?;
        // `application_name` goes in the User-Agent, as Google's client puts it there.
        let user_agent = application_name.filter(|n| !n.is_empty() && n.len() <= 256 && n.bytes().all(|c| (0x20..0x7f).contains(&c))).map(|n| format!("{n} roll-search"));
        Ok(Client { http, endpoint, bucket: bucket.to_string(), account, user_agent })
    }

    /// An access token for the service account, from the cache while it has five minutes left.
    async fn token(&self) -> Result<Option<String>, Error> {
        let Some(a) = &self.account else { return Ok(None) };
        if let Some((t, until)) = TOKENS.lock().get(&a.digest)
            && *until > Instant::now() + TOKEN_MARGIN
        {
            return Ok(Some(t.clone()));
        }
        let now = chrono::Utc::now().timestamp();
        let claims = serde_json::json!({"iss": a.client_email, "scope": SCOPE, "aud": a.token_uri.as_str(), "iat": now, "exp": now + 3600});
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = a.key_id.clone();
        let assertion = jsonwebtoken::encode(&header, &claims, &a.key).map_err(|_| err("CredentialsError", "the service account's key couldn't sign a token request"))?;
        let body = format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={assertion}");
        let fetched = self.http.post(a.token_uri.clone()).timeout(Duration::from_secs(60)).header("content-type", "application/x-www-form-urlencoded").body(body).send().await;
        let r = fetched.map_err(|e| err("TokenRequestFailed", crate::s3::transport(&e.without_url())))?;
        let status = r.status().as_u16();
        let raw = read_bounded(r, ERROR_BODY_MAX).await?;
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap_or_default();
        let text = |k: &str| v.get(k).and_then(serde_json::Value::as_str).map(str::to_string);
        if status != 200 {
            let why = text("error_description").or_else(|| text("error")).unwrap_or_default();
            return Err(Error { status: Some(status), code: "TokenRequestFailed".into(), message: format!("the token endpoint refused the service account: {}", clip(&why)) });
        }
        let Some(token) = text("access_token").filter(|t| !t.is_empty() && t.len() <= 8192 && t.bytes().all(|c| (0x21..0x7f).contains(&c))) else {
            return Err(err("TokenRequestFailed", "the token endpoint answered no access token"));
        };
        let ttl = v.get("expires_in").and_then(serde_json::Value::as_u64).unwrap_or(3600).min(24 * 3600);
        let mut cache = TOKENS.lock();
        if cache.len() >= TOKENS_MAX {
            let now = Instant::now();
            cache.retain(|_, (_, until)| *until > now);
            if cache.len() >= TOKENS_MAX {
                cache.clear();
            }
        }
        cache.insert(a.digest, (token.clone(), Instant::now() + Duration::from_secs(ttl)));
        Ok(Some(token))
    }

    fn at(&self, path: &str, query: &[(&str, &str)]) -> reqwest::Url {
        let mut u = self.endpoint.clone();
        u.set_path(path);
        if !query.is_empty() {
            u.query_pairs_mut().extend_pairs(query);
        }
        u
    }

    /// `storage/v1/b/<bucket>/o/<key>`.
    fn object(&self, key: &str, query: &[(&str, &str)]) -> reqwest::Url {
        self.at(&format!("/storage/v1/b/{}/o/{}", self.bucket, uri_encode(key, true)), query)
    }

    fn upload(&self, query: &[(&str, &str)]) -> reqwest::Url {
        self.at(&format!("/upload/storage/v1/b/{}/o", self.bucket), query)
    }

    async fn send(&self, method: reqwest::Method, url: reqwest::Url, extra: &[(&str, String)], body: Bytes, total: Option<Duration>) -> Result<reqwest::Response, Error> {
        let mut last = err("RequestFailed", "no attempt was made");
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(100 << attempt)).await;
            }
            let mut rb = self.http.request(method.clone(), url.clone());
            if let Some(t) = total {
                rb = rb.timeout(t);
            }
            if let Some(t) = self.token().await? {
                rb = rb.bearer_auth(t);
            }
            if let Some(ua) = &self.user_agent {
                rb = rb.header("user-agent", ua.as_str());
            }
            for (k, v) in extra {
                rb = rb.header(*k, v.as_str());
            }
            match rb.body(body.clone()).send().await {
                Ok(r) if matches!(r.status().as_u16(), 429 | 500 | 502 | 503 | 504) && attempt + 1 < ATTEMPTS => last = failure(r).await,
                Ok(r) if r.status().as_u16() == 401 && attempt + 1 < ATTEMPTS && self.account.is_some() => {
                    // A token revoked early: fetch another.
                    if let Some(a) = &self.account {
                        TOKENS.lock().remove(&a.digest);
                    }
                    last = failure(r).await;
                }
                Ok(r) => return Ok(r),
                Err(e) if attempt + 1 < ATTEMPTS && !e.is_builder() => last = err("RequestFailed", crate::s3::transport(&e.without_url())),
                Err(e) => return Err(err("RequestFailed", crate::s3::transport(&e.without_url()))),
            }
        }
        Err(last)
    }

    /// The object at `key`, or `None` when there is none. More than `max` bytes is an error.
    pub async fn get(&self, key: &str, max: usize) -> Result<Option<Bytes>, Error> {
        Ok(self.get_tagged(key, max).await?.map(|(b, _)| b))
    }

    /// The object at `key` and its generation, or `None` when there is none.
    pub async fn get_tagged(&self, key: &str, max: usize) -> Result<Option<(Bytes, String)>, Error> {
        let r = self.send(reqwest::Method::GET, self.object(key, &[("alt", "media")]), &[], Bytes::new(), Some(REQUEST_MAX)).await?;
        match r.status().as_u16() {
            200 => {
                let generation = r.headers().get("x-goog-generation").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                read_bounded(r, max).await.map(|b| Some((b, generation)))
            }
            404 => Ok(None),
            _ => Err(failure(r).await),
        }
    }

    /// Writes the object at `key`.
    pub async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        let r = self.send(reqwest::Method::POST, self.upload(&[("uploadType", "media"), ("name", key)]), &[], body, Some(REQUEST_MAX)).await?;
        match r.status().is_success() {
            true => Ok(()),
            false => Err(failure(r).await),
        }
    }

    /// Writes the object at `key` only when there is none: `false` when one exists.
    pub async fn put_new(&self, key: &str, body: Bytes) -> Result<bool, Error> {
        Ok(self.put_if(key, body, None).await?.is_some())
    }

    /// Writes the object at `key` only at generation `generation` (`ifGenerationMatch`), or,
    /// with `None`, only when there is none: the new generation, or `None` when the condition
    /// failed.
    pub async fn put_if(&self, key: &str, body: Bytes, generation: Option<&str>) -> Result<Option<String>, Error> {
        let at = generation.filter(|g| !g.is_empty()).unwrap_or("0");
        let r = self.send(reqwest::Method::POST, self.upload(&[("uploadType", "media"), ("name", key), ("ifGenerationMatch", at)]), &[], body, Some(REQUEST_MAX)).await?;
        match r.status().as_u16() {
            200..=299 => {
                let raw = read_bounded(r, ERROR_BODY_MAX).await?;
                let v: serde_json::Value = serde_json::from_slice(&raw).unwrap_or_default();
                let generation = match v.get("generation") {
                    Some(serde_json::Value::String(g)) => g.clone(),
                    Some(serde_json::Value::Number(n)) => n.to_string(),
                    _ => String::new(),
                };
                Ok(Some(generation))
            }
            412 | 409 => Ok(None),
            _ => Err(failure(r).await),
        }
    }

    /// The object at `key` (bytes `start..end` of it with `range`) as it arrives, or `None` when
    /// there is none. Only an idle read times out.
    pub async fn open(&self, key: &str, range: Option<(u64, u64)>) -> Result<Option<Download>, Error> {
        let extra: Vec<(&str, String)> = range.map(|(a, b)| ("range", format!("bytes={a}-{}", b.saturating_sub(1)))).into_iter().collect();
        let r = self.send(reqwest::Method::GET, self.object(key, &[("alt", "media")]), &extra, Bytes::new(), None).await?;
        match r.status().as_u16() {
            200 | 206 => Ok(Some(Download(r))),
            404 => Ok(None),
            _ => Err(failure(r).await),
        }
    }

    /// Deletes the object at `key` only at `generation`: whether it's gone (`false`: it changed).
    pub async fn delete_if(&self, key: &str, generation: &str) -> Result<bool, Error> {
        let r = self.send(reqwest::Method::DELETE, self.object(key, &[("ifGenerationMatch", generation)]), &[], Bytes::new(), Some(REQUEST_MAX)).await?;
        match r.status().as_u16() {
            200..=299 | 404 => Ok(true),
            412 => Ok(false),
            _ => Err(failure(r).await),
        }
    }

    /// Deletes the object at `key` (none there is fine).
    pub async fn delete(&self, key: &str) -> Result<(), Error> {
        let r = self.send(reqwest::Method::DELETE, self.object(key, &[]), &[], Bytes::new(), Some(REQUEST_MAX)).await?;
        match r.status().as_u16() {
            200..=299 | 404 => Ok(()),
            _ => Err(failure(r).await),
        }
    }

    /// Every object under `prefix`, as `(key, size)`, bounded as [`crate::s3::Client::list`]'s.
    pub async fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>, Error> {
        let mut out = Vec::new();
        self.list_while(prefix, &mut |k, size| {
            out.push((k.to_string(), size));
            true
        })
        .await?;
        Ok(out)
    }

    /// Every object under `prefix` in key order, handed to `f` until it answers `false`;
    /// whether it did. Bounded as [`Self::list`] is.
    pub async fn list_while(&self, prefix: &str, f: &mut (dyn FnMut(&str, u64) -> bool + Send)) -> Result<bool, Error> {
        const PAGE_MAX: usize = 4 << 20;
        let mut count = 0usize;
        let mut key_bytes = 0usize;
        let mut token: Option<String> = None;
        for _ in 0..LIST_PAGES_MAX {
            let mut q = vec![("prefix", prefix), ("maxResults", "1000")];
            if let Some(t) = &token {
                q.push(("pageToken", t.as_str()));
            }
            let r = self.send(reqwest::Method::GET, self.at(&format!("/storage/v1/b/{}/o", self.bucket), &q), &[], Bytes::new(), Some(REQUEST_MAX)).await?;
            if !r.status().is_success() {
                return Err(failure(r).await);
            }
            let page = read_bounded(r, PAGE_MAX).await?;
            let v: serde_json::Value = serde_json::from_slice(&page).map_err(|_| err("BadResponse", "a listing isn't JSON"))?;
            for item in v.get("items").and_then(serde_json::Value::as_array).into_iter().flatten() {
                let Some(name) = item.get("name").and_then(serde_json::Value::as_str) else { continue };
                let size = match item.get("size") {
                    Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
                    Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
                    _ => 0,
                };
                key_bytes += name.len();
                if count >= LIST_ENTRIES_MAX || key_bytes > LIST_KEY_BYTES_MAX {
                    return Err(err("TooLarge", format!("the listing of [{prefix}] holds more than [{LIST_ENTRIES_MAX}] objects or [{LIST_KEY_BYTES_MAX}] bytes of keys")));
                }
                // A server answering more than was asked for is ignored past the prefix.
                if name.starts_with(prefix) {
                    count += 1;
                    if !f(name, size) {
                        return Ok(true);
                    }
                }
            }
            let next = v.get("nextPageToken").and_then(serde_json::Value::as_str).filter(|t| !t.is_empty()).map(str::to_string);
            if next.is_none() || next == token {
                return Ok(false);
            }
            token = next;
        }
        Err(err("TooLarge", format!("the listing of [{prefix}] is more than [{LIST_PAGES_MAX}] pages")))
    }

    /// Starts a resumable upload of `key`: its session URL, on the endpoint's own origin.
    pub async fn begin(&self, key: &str) -> Result<String, Error> {
        let r = self.send(reqwest::Method::POST, self.upload(&[("uploadType", "resumable"), ("name", key)]), &[("content-type", "application/json".into())], Bytes::from_static(b"{}"), Some(REQUEST_MAX)).await?;
        if !r.status().is_success() {
            return Err(failure(r).await);
        }
        let location = r.headers().get("location").and_then(|v| v.to_str().ok()).and_then(|l| reqwest::Url::parse(l).ok());
        match location {
            Some(u) if u.origin() == self.endpoint.origin() => Ok(u.to_string()),
            Some(_) => Err(err("BadResponse", "the resumable upload's session is on another origin than the endpoint")),
            None => Err(err("BadResponse", "a resumable upload answered no session")),
        }
    }

    /// Sends bytes `offset..offset + body.len()` of resumable upload `session`; `total` is the
    /// object's size when these are its last bytes (which completes it).
    pub async fn upload_chunk(&self, session: &str, offset: u64, body: Bytes, total: Option<u64>) -> Result<(), Error> {
        let url = self.session(session)?;
        let n = body.len() as u64;
        let range = match (n, total) {
            (0, Some(t)) => format!("bytes */{t}"),
            (0, None) => return Ok(()),
            (_, Some(t)) => format!("bytes {offset}-{}/{t}", offset + n - 1),
            (_, None) => format!("bytes {offset}-{}/*", offset + n - 1),
        };
        let r = self.send(reqwest::Method::PUT, url, &[("content-range", range)], body, Some(REQUEST_MAX)).await?;
        match (r.status().as_u16(), total) {
            (308, None) | (200 | 201, Some(_)) => Ok(()),
            _ => Err(failure(r).await),
        }
    }

    /// Cancels resumable upload `session` (one already gone is fine).
    pub async fn cancel(&self, session: &str) -> Result<(), Error> {
        let r = self.send(reqwest::Method::DELETE, self.session(session)?, &[], Bytes::new(), Some(REQUEST_MAX)).await?;
        match r.status().as_u16() {
            200..=299 | 404 | 410 | 499 => Ok(()),
            _ => Err(failure(r).await),
        }
    }

    fn session(&self, session: &str) -> Result<reqwest::Url, Error> {
        match reqwest::Url::parse(session) {
            Ok(u) if u.origin() == self.endpoint.origin() => Ok(u),
            _ => Err(err("BadSession", "not a resumable upload session of this bucket")),
        }
    }
}

/// At most 512 characters of what a server said.
fn clip(s: &str) -> String {
    s.chars().take(512).collect()
}

/// An error response: GCS's reason and message, from at most [`ERROR_BODY_MAX`] bytes.
async fn failure(r: reqwest::Response) -> Error {
    let status = r.status().as_u16();
    let body = read_bounded(r, ERROR_BODY_MAX).await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let e = v.get("error");
    let code = e
        .and_then(|e| e.get("errors"))
        .and_then(|es| es.get(0))
        .and_then(|e| e.get("reason"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("Http{status}"));
    let message = match e.and_then(|e| e.get("message")).and_then(serde_json::Value::as_str) {
        Some(m) => clip(m),
        None => clip(String::from_utf8_lossy(&body).trim()),
    };
    Error { status: Some(status), code, message }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_urls_tenants_may_give() {
        let e = |private: bool| crate::egress::Egress::of(|_| None, private).unwrap();
        assert!(valid_bucket("my-bucket_1.backups") && !valid_bucket("UP") && !valid_bucket("ab") && !valid_bucket("a..b") && !valid_bucket("x/y"));
        assert_eq!(endpoint(None, &e(false)).unwrap().as_str(), "https://storage.googleapis.com/");
        assert_eq!(endpoint(Some("http://gcs.example.com:4443"), &e(false)).unwrap().as_str(), "http://gcs.example.com:4443/");
        for bad in ["http://127.0.0.1:4443", "http://localhost", "https://u:p@gcs.example.com", "https://gcs.example.com/x", "ftp://gcs.example.com"] {
            assert!(endpoint(Some(bad), &e(false)).is_err(), "{bad}");
        }
        assert!(endpoint(Some("http://127.0.0.1:4443"), &e(true)).is_ok());
        // A key file that isn't one fails without quoting it.
        for bad in [r#"{"type":"authorized_user","private_key":"SECRETVALUE"}"#, "SECRETVALUE", r#"{"type":"service_account","client_email":"a@b","private_key":"SECRETVALUE"}"#] {
            let e = ServiceAccount::parse(bad, None, &e(false)).unwrap_err();
            assert!(!e.contains("SECRETVALUE"), "{e}");
        }
    }
}
