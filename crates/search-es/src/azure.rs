//! A small Azure Blob Storage client for containers a tenant names (the `azure` snapshot
//! repository): blobs are read (whole, or streamed and ranged), written as block blobs
//! (optionally only when absent or unchanged, by `If-None-Match`/`If-Match`), written in blocks
//! (Put Block, then Put Block List), deleted and listed.
//!
//! Requests are signed with the account's key (Shared Key) or carry its SAS token; with neither
//! they're anonymous. roll's own credentials (managed identities and the like) are never used.
//! The account's blob endpoint is `https://<account>.blob.<endpoint_suffix>` (`core.windows.net`
//! by default), or `endpoint` as given, with a secondary one for `location_mode` reads.
//!
//! The endpoints are the tenant's choice, so the client is built as AGENTS.md requires for any
//! such URL, through the node's egress check ([`crate::egress`]): names resolve to public
//! addresses only, never to one the node denies; private and denied IP literals and `localhost`
//! are refused; no redirect is
//! followed, no proxy is used, and every response body is read up to a bound. Errors carry no
//! URL (a SAS token is in it) and no credential.

use crate::s3::{ATTEMPTS, Download, ERROR_BODY_MAX, Error, LIST_ENTRIES_MAX, LIST_KEY_BYTES_MAX, LIST_PAGES_MAX, READ_IDLE, REQUEST_MAX, elements, err, etag_of, read_bounded, uri_encode, xml_unescape};
use base64::Engine;
use bytes::Bytes;
use hkdf::hmac::{Hmac, Mac};
use sha2::Sha256;
use std::time::Duration;

/// The REST API version requests name.
const VERSION: &str = "2023-11-03";
pub const DEFAULT_SUFFIX: &str = "core.windows.net";
/// A block blob holds at most this many blocks.
pub const MAX_BLOCKS: u64 = 50_000;

/// How a request is authorized. Never printed.
#[derive(Clone)]
pub enum Auth {
    /// Shared Key: the account key, decoded.
    Key(Vec<u8>),
    /// A SAS token's query string, without the `?`.
    Sas(String),
    Anonymous,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Auth(..)")
    }
}

impl Auth {
    /// The authorization a client's `key` and `sas_token` give (one or neither): the messages
    /// are Elasticsearch's (`AzureStorageSettings`), and carry neither value.
    pub fn new(account: &str, key: Option<&str>, sas: Option<&str>) -> Result<Auth, String> {
        match (key.filter(|k| !k.is_empty()), sas.filter(|s| !s.is_empty())) {
            (Some(_), Some(_)) => Err(format!("Both a secret as well as a shared access token were set for account [{account}]")),
            (Some(k), None) => base64::engine::general_purpose::STANDARD.decode(k.trim()).map(Auth::Key).map_err(|_| format!("the key of account [{account}] isn't base64")),
            (None, Some(s)) => {
                let s = s.trim().trim_start_matches('?');
                // It goes in every URL as it is.
                match !s.is_empty() && s.len() <= 4096 && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"%&=-._~:+/,".contains(&c)) {
                    true => Ok(Auth::Sas(s.to_string())),
                    false => Err(format!("the shared access token of account [{account}] isn't a query string")),
                }
            }
            (None, None) => Ok(Auth::Anonymous),
        }
    }
}

/// `location_mode`: where reads go, in order; writes always go to the primary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocationMode {
    PrimaryOnly,
    SecondaryOnly,
    PrimaryThenSecondary,
    SecondaryThenPrimary,
}

impl LocationMode {
    /// As Elasticsearch reads it (`LocationMode.valueOf(value.toUpperCase(Locale.ROOT))`).
    pub fn parse(v: &str) -> Result<LocationMode, String> {
        match v.to_ascii_uppercase().as_str() {
            "PRIMARY_ONLY" => Ok(LocationMode::PrimaryOnly),
            "SECONDARY_ONLY" => Ok(LocationMode::SecondaryOnly),
            "PRIMARY_THEN_SECONDARY" => Ok(LocationMode::PrimaryThenSecondary),
            "SECONDARY_THEN_PRIMARY" => Ok(LocationMode::SecondaryThenPrimary),
            other => Err(format!("No enum constant org.elasticsearch.repositories.azure.LocationMode.{other}")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            LocationMode::PrimaryOnly => "PRIMARY_ONLY",
            LocationMode::SecondaryOnly => "SECONDARY_ONLY",
            LocationMode::PrimaryThenSecondary => "PRIMARY_THEN_SECONDARY",
            LocationMode::SecondaryThenPrimary => "SECONDARY_THEN_PRIMARY",
        }
    }

    fn uses_secondary(self) -> bool {
        self != LocationMode::PrimaryOnly
    }
}

/// A blob endpoint a tenant names: http(s), a host, maybe a path (an emulator's account), no
/// credentials, query or fragment.
fn endpoint_url(raw: &str, egress: &crate::egress::Egress) -> Result<reqwest::Url, String> {
    let u = reqwest::Url::parse(raw.trim()).map_err(|_| format!("invalid endpoint [{raw}]"))?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none_or(str::is_empty) {
        return Err(format!("invalid endpoint [{raw}]"));
    }
    if !u.username().is_empty() || u.password().is_some() || u.query().is_some() || u.fragment().is_some() {
        return Err(format!("endpoint [{raw}] must be a scheme, host, port and path only"));
    }
    match egress.check_url(&u) {
        Err(crate::egress::Refused::Private(_)) => Err(format!("endpoint [{raw}] is a private address")),
        Err(e) => Err(format!("endpoint [{raw}]: {e}")),
        Ok(()) => Ok(u),
    }
}

/// The account's primary and secondary blob endpoints, from the client's `endpoint_suffix`,
/// `endpoint` and `secondary_endpoint`, with Elasticsearch's checks (`AzureStorageSettings`,
/// `AzureClientProvider`). `None` for the secondary: there is none.
pub fn endpoints(account: &str, suffix: Option<&str>, primary: Option<&str>, secondary: Option<&str>, mode: LocationMode, egress: &crate::egress::Egress) -> Result<(reqwest::Url, Option<reqwest::Url>), String> {
    let given = |v: Option<&str>| v.map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let (suffix, primary, secondary) = (given(suffix), given(primary), given(secondary));
    if suffix.is_some() && primary.is_some() {
        return Err(format!("Both an endpoint suffix as well as a primary endpoint were set for account [{account}]"));
    }
    if suffix.is_some() && secondary.is_some() {
        return Err(format!("Both an endpoint suffix as well as a secondary endpoint were set for account [{account}]"));
    }
    if primary.is_none() && secondary.is_some() {
        return Err(format!("A primary endpoint is required when setting a secondary endpoint for account [{account}]"));
    }
    let (primary, secondary) = match primary {
        Some(p) => (endpoint_url(&p, egress)?, secondary.map(|s| endpoint_url(&s, egress)).transpose()?),
        None => {
            if !valid_account(account) {
                return Err(format!("invalid account name [{account}]"));
            }
            let suffix = suffix.unwrap_or_else(|| DEFAULT_SUFFIX.into());
            if suffix.is_empty() || suffix.len() > 200 || !suffix.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-') || suffix.starts_with('.') {
                return Err(format!("invalid endpoint_suffix [{suffix}]"));
            }
            let p = endpoint_url(&format!("https://{account}.blob.{suffix}"), egress)?;
            let s = endpoint_url(&format!("https://{account}-secondary.blob.{suffix}"), egress)?;
            (p, Some(s))
        }
    };
    if mode.uses_secondary() && secondary.is_none() {
        return Err("Unable to configure an AzureClient using a secondary location without a secondary endpoint".into());
    }
    Ok((primary, secondary))
}

/// Storage account names: 3-24 lowercase letters and digits (an emulator's too).
pub fn valid_account(a: &str) -> bool {
    (3..=24).contains(&a.len()) && a.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// Container names: 3-63 lowercase letters, digits and single dashes, starting and ending with a
/// letter or digit.
pub fn valid_container(c: &str) -> bool {
    let b = c.as_bytes();
    (3..=63).contains(&b.len()) && b.iter().all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || *x == b'-') && b[0].is_ascii_alphanumeric() && b[b.len() - 1].is_ascii_alphanumeric() && !c.contains("--")
}

/// An Azure container.
pub struct Client {
    http: reqwest::Client,
    account: String,
    auth: Auth,
    primary: reqwest::Url,
    secondary: Option<reqwest::Url>,
    mode: LocationMode,
    container: String,
}

/// Whether a request may go to the secondary endpoint.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Read,
    Write,
}

impl Client {
    /// The URLs it connects to as written (names reach the egress resolver; literals don't, so
    /// callers check these with [`crate::egress::Egress::check_url_now`]).
    pub fn endpoints(&self) -> Vec<&reqwest::Url> {
        std::iter::once(&self.primary).chain(self.secondary.as_ref()).collect()
    }

    /// A client for `container`. Addresses `egress` refuses are refused (as literals here,
    /// resolved ones at connect time).
    pub fn new(account: &str, auth: Auth, primary: reqwest::Url, secondary: Option<reqwest::Url>, mode: LocationMode, container: &str, egress: &crate::egress::Egress) -> Result<Client, Error> {
        if !valid_container(container) {
            return Err(err("InvalidContainerName", format!("invalid container name [{container}]")));
        }
        for u in std::iter::once(&primary).chain(secondary.as_ref()) {
            match egress.check_url(u) {
                Err(crate::egress::Refused::Private(_)) => return Err(err("PrivateAddress", "the endpoint is a private address")),
                Err(e) => return Err(err("DeniedAddress", format!("the endpoint: {e}"))),
                Ok(()) => {}
            }
        }
        let b = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).read_timeout(READ_IDLE).redirect(reqwest::redirect::Policy::none()).no_proxy().dns_resolver(egress.resolver());
        let http = b.build().map_err(|e| err("ClientError", e.to_string()))?;
        Ok(Client { http, account: account.to_string(), auth, primary, secondary, mode, container: container.to_string() })
    }

    pub fn location_mode(&self) -> LocationMode {
        self.mode
    }

    /// Where a request of `kind` goes, in order.
    fn targets(&self, kind: Kind) -> Vec<&reqwest::Url> {
        let (p, s) = (Some(&self.primary), self.secondary.as_ref());
        let order = match (kind, self.mode) {
            (Kind::Write, _) | (_, LocationMode::PrimaryOnly) => [p, None],
            (_, LocationMode::SecondaryOnly) => [s, None],
            (_, LocationMode::PrimaryThenSecondary) => [p, s],
            (_, LocationMode::SecondaryThenPrimary) => [s, p],
        };
        order.into_iter().flatten().collect()
    }

    /// The URL of blob `key` (the container itself when empty) at `base`, and its path.
    fn url(&self, base: &reqwest::Url, key: &str, query: &[(&str, &str)]) -> reqwest::Url {
        let root = base.path().trim_end_matches('/');
        let path = match key.is_empty() {
            true => format!("{root}/{}", self.container),
            false => format!("{root}/{}/{}", self.container, uri_encode(key, false)),
        };
        let mut u = base.clone();
        u.set_path(&path);
        let mut q: Vec<String> = query.iter().map(|(k, v)| format!("{}={}", uri_encode(k, true), uri_encode(v, true))).collect();
        if let Auth::Sas(s) = &self.auth {
            q.push(s.clone());
        }
        u.set_query((!q.is_empty()).then(|| q.join("&")).as_deref());
        u
    }

    /// Shared Key's `Authorization` header for a request (`headers`: lowercase names).
    fn sign(&self, key: &[u8], method: &str, url: &reqwest::Url, query: &[(&str, &str)], headers: &[(String, String)], length: usize) -> String {
        let h = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or("");
        let length = if length == 0 { String::new() } else { length.to_string() };
        let mut ms: Vec<&(String, String)> = headers.iter().filter(|(k, _)| k.starts_with("x-ms-")).collect();
        ms.sort_by(|a, b| a.0.cmp(&b.0));
        let canonical_headers: String = ms.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
        let mut resource = format!("/{}{}", self.account, url.path());
        let mut q: Vec<(String, &str)> = query.iter().map(|(k, v)| (k.to_ascii_lowercase(), *v)).collect();
        q.sort();
        for (k, v) in q {
            resource.push_str(&format!("\n{k}:{v}"));
        }
        let to_sign = format!(
            "{method}\n{}\n{}\n{length}\n{}\n{}\n\n{}\n{}\n{}\n{}\n{}\n{canonical_headers}{resource}",
            h("content-encoding"),
            h("content-language"),
            h("content-md5"),
            h("content-type"),
            h("if-modified-since"),
            h("if-match"),
            h("if-none-match"),
            h("if-unmodified-since"),
            h("range"),
        );
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
        m.update(to_sign.as_bytes());
        let sig = base64::engine::general_purpose::STANDARD.encode(m.finalize().into_bytes());
        format!("SharedKey {}:{sig}", self.account)
    }

    #[allow(clippy::too_many_arguments)]
    async fn send(&self, kind: Kind, method: reqwest::Method, key: &str, query: &[(&str, &str)], extra: &[(&str, String)], body: Bytes, total: Option<Duration>) -> Result<reqwest::Response, Error> {
        let mut last = err("RequestFailed", "no attempt was made");
        let targets = self.targets(kind);
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(100 << attempt)).await;
            }
            for (i, base) in targets.iter().enumerate() {
                let more = attempt + 1 < ATTEMPTS || i + 1 < targets.len();
                let url = self.url(base, key, query);
                let mut headers: Vec<(String, String)> = vec![
                    ("x-ms-date".into(), chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()),
                    ("x-ms-version".into(), VERSION.into()),
                ];
                for (k, v) in extra {
                    headers.push((k.to_ascii_lowercase(), v.clone()));
                }
                if let Auth::Key(k) = &self.auth {
                    let auth = self.sign(k, method.as_str(), &url, query, &headers, body.len());
                    headers.push(("authorization".into(), auth));
                }
                let mut rb = self.http.request(method.clone(), url);
                if let Some(t) = total {
                    rb = rb.timeout(t);
                }
                for (k, v) in &headers {
                    rb = rb.header(k.as_str(), v.as_str());
                }
                match rb.body(body.clone()).send().await {
                    Ok(r) if matches!(r.status().as_u16(), 500 | 502 | 503 | 504) && more => last = failure(r, &self.auth).await,
                    Ok(r) => return Ok(r),
                    Err(e) if more && !e.is_builder() => last = err("RequestFailed", crate::s3::transport(&e.without_url())),
                    Err(e) => return Err(err("RequestFailed", crate::s3::transport(&e.without_url()))),
                }
            }
        }
        Err(last)
    }

    async fn fail(&self, r: reqwest::Response) -> Error {
        failure(r, &self.auth).await
    }

    /// The blob at `key`, or `None` when there is none. More than `max` bytes is an error.
    pub async fn get(&self, key: &str, max: usize) -> Result<Option<Bytes>, Error> {
        Ok(self.get_tagged(key, max).await?.map(|(b, _)| b))
    }

    /// The blob at `key` and its ETag, or `None` when there is none.
    pub async fn get_tagged(&self, key: &str, max: usize) -> Result<Option<(Bytes, String)>, Error> {
        let r = self.send(Kind::Read, reqwest::Method::GET, key, &[], &[], Bytes::new(), Some(REQUEST_MAX)).await?;
        match r.status().as_u16() {
            200 => {
                let etag = etag_of(&r);
                read_bounded(r, max).await.map(|b| Some((b, etag)))
            }
            404 => Ok(None),
            _ => Err(self.fail(r).await),
        }
    }

    /// Writes the block blob at `key`.
    pub async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        let r = self.send(Kind::Write, reqwest::Method::PUT, key, &[], &[("x-ms-blob-type", "BlockBlob".into())], body, Some(REQUEST_MAX)).await?;
        match r.status().is_success() {
            true => Ok(()),
            false => Err(self.fail(r).await),
        }
    }

    /// Writes the blob at `key` only when there is none: `false` when one exists.
    pub async fn put_new(&self, key: &str, body: Bytes) -> Result<bool, Error> {
        Ok(self.put_if(key, body, None).await?.is_some())
    }

    /// Writes the blob at `key` only when its ETag is `etag` (`If-Match`), or, with `None`, only
    /// when there is none: the new ETag, or `None` when the condition failed.
    pub async fn put_if(&self, key: &str, body: Bytes, etag: Option<&str>) -> Result<Option<String>, Error> {
        let condition = match etag {
            Some(e) => ("if-match", e.to_string()),
            None => ("if-none-match", "*".to_string()),
        };
        let r = self.send(Kind::Write, reqwest::Method::PUT, key, &[], &[("x-ms-blob-type", "BlockBlob".into()), condition], body, Some(REQUEST_MAX)).await?;
        match r.status().as_u16() {
            200..=299 => Ok(Some(etag_of(&r))),
            412 | 409 => Ok(None),
            _ => Err(self.fail(r).await),
        }
    }

    /// The blob at `key` (bytes `start..end` of it with `range`) as it arrives, or `None` when
    /// there is none. Only an idle read times out.
    pub async fn open(&self, key: &str, range: Option<(u64, u64)>) -> Result<Option<Download>, Error> {
        let extra: Vec<(&str, String)> = range.map(|(a, b)| ("x-ms-range", format!("bytes={a}-{}", b.saturating_sub(1)))).into_iter().collect();
        let r = self.send(Kind::Read, reqwest::Method::GET, key, &[], &extra, Bytes::new(), None).await?;
        match r.status().as_u16() {
            200 | 206 => Ok(Some(Download(r))),
            404 => Ok(None),
            _ => Err(self.fail(r).await),
        }
    }

    /// Deletes the blob at `key` and its snapshots only at version `etag`: whether it's gone
    /// (`false`: it changed).
    pub async fn delete_if(&self, key: &str, etag: &str) -> Result<bool, Error> {
        let r = self.send(Kind::Write, reqwest::Method::DELETE, key, &[], &[("x-ms-delete-snapshots", "include".into()), ("if-match", etag.to_string())], Bytes::new(), Some(REQUEST_MAX)).await?;
        match r.status().as_u16() {
            200..=299 | 404 => Ok(true),
            412 => Ok(false),
            _ => Err(self.fail(r).await),
        }
    }

    /// Deletes the blob at `key` and its snapshots (none there is fine).
    pub async fn delete(&self, key: &str) -> Result<(), Error> {
        let r = self.send(Kind::Write, reqwest::Method::DELETE, key, &[], &[("x-ms-delete-snapshots", "include".into())], Bytes::new(), Some(REQUEST_MAX)).await?;
        match r.status().as_u16() {
            200..=299 | 404 => Ok(()),
            _ => Err(self.fail(r).await),
        }
    }

    /// Every blob under `prefix`, as `(key, size)`, bounded as [`crate::s3::Client::list`]'s.
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
        self.list_blobs(prefix, false, f).await
    }

    /// [`Self::list_while`], with blobs whose blocks are staged but not yet committed (an
    /// upload in progress) when `uncommitted`.
    pub async fn list_blobs(&self, prefix: &str, uncommitted: bool, f: &mut (dyn FnMut(&str, u64) -> bool + Send)) -> Result<bool, Error> {
        const PAGE_MAX: usize = 8 << 20;
        let mut count = 0usize;
        let mut key_bytes = 0usize;
        let mut marker = String::new();
        for _ in 0..LIST_PAGES_MAX {
            let mut q = vec![("restype", "container"), ("comp", "list"), ("prefix", prefix), ("maxresults", "5000")];
            if uncommitted {
                q.push(("include", "uncommittedblobs"));
            }
            if !marker.is_empty() {
                q.push(("marker", marker.as_str()));
            }
            let r = self.send(Kind::Read, reqwest::Method::GET, "", &q, &[], Bytes::new(), Some(REQUEST_MAX)).await?;
            if !r.status().is_success() {
                return Err(self.fail(r).await);
            }
            let page = read_bounded(r, PAGE_MAX).await?;
            let page = String::from_utf8_lossy(&page);
            for item in elements(&page, "Blob") {
                // A name with characters XML can't hold comes `Encoded`: roll writes none.
                let Some(name) = elements(item, "Name").next().map(xml_unescape) else { continue };
                let size = elements(item, "Content-Length").next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                key_bytes += name.len();
                if count >= LIST_ENTRIES_MAX || key_bytes > LIST_KEY_BYTES_MAX {
                    return Err(err("TooLarge", format!("the listing of [{prefix}] holds more than [{LIST_ENTRIES_MAX}] objects or [{LIST_KEY_BYTES_MAX}] bytes of keys")));
                }
                if name.starts_with(prefix) {
                    count += 1;
                    if !f(&name, size) {
                        return Ok(true);
                    }
                }
            }
            let next = elements(&page, "NextMarker").next().map(xml_unescape).unwrap_or_default();
            if next.is_empty() || next == marker {
                return Ok(false);
            }
            marker = next;
        }
        Err(err("TooLarge", format!("the listing of [{prefix}] is more than [{LIST_PAGES_MAX}] pages")))
    }

    /// Creates the container (setup tools and tests; a repository never creates one): whether
    /// it was made, `false` when it already existed.
    pub async fn create_container(&self) -> Result<bool, Error> {
        let r = self.send(Kind::Write, reqwest::Method::PUT, "", &[("restype", "container")], &[], Bytes::new(), Some(REQUEST_MAX)).await?;
        match r.status().as_u16() {
            200..=299 => Ok(true),
            409 => Ok(false),
            _ => Err(self.fail(r).await),
        }
    }

    /// A block id for block `n` (from 1) of upload `upload`: base64 of the same length for
    /// every block, unique to the upload so concurrent writers of one blob never share one.
    fn block_id(upload: &str, n: u32) -> String {
        base64::engine::general_purpose::STANDARD.encode(format!("{upload}-{n:06}"))
    }

    /// Stages block `n` (from 1) of upload `upload` of `key`: the block's id.
    pub async fn put_block(&self, key: &str, upload: &str, n: u32, body: Bytes) -> Result<String, Error> {
        let id = Self::block_id(upload, n);
        let r = self.send(Kind::Write, reqwest::Method::PUT, key, &[("comp", "block"), ("blockid", &id)], &[], body, Some(REQUEST_MAX)).await?;
        match r.status().is_success() {
            true => Ok(id),
            false => Err(self.fail(r).await),
        }
    }

    /// Commits the staged blocks `ids`, in order, as the blob at `key`.
    pub async fn put_block_list(&self, key: &str, ids: &[String]) -> Result<(), Error> {
        let blocks: String = ids.iter().map(|id| format!("<Latest>{id}</Latest>")).collect();
        let body = Bytes::from(format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>{blocks}</BlockList>"));
        let r = self.send(Kind::Write, reqwest::Method::PUT, key, &[("comp", "blocklist")], &[("content-type", "application/xml".into())], body, Some(REQUEST_MAX)).await?;
        match r.status().is_success() {
            true => Ok(()),
            false => Err(self.fail(r).await),
        }
    }
}

/// An error response: Azure's `Code` and `Message` from at most [`ERROR_BODY_MAX`] bytes, with
/// anything of the SAS token taken out (a server may echo the URL).
async fn failure(r: reqwest::Response, auth: &Auth) -> Error {
    let status = r.status().as_u16();
    let header = r.headers().get("x-ms-error-code").and_then(|v| v.to_str().ok()).map(str::to_string);
    let body = read_bounded(r, ERROR_BODY_MAX).await.unwrap_or_default();
    let text = String::from_utf8_lossy(&body);
    let code = elements(&text, "Code").next().map(xml_unescape).or(header).unwrap_or_else(|| format!("Http{status}"));
    let mut message: String = elements(&text, "Message").next().map(xml_unescape).unwrap_or_default().lines().next().unwrap_or_default().chars().take(512).collect();
    if let Auth::Sas(s) = auth {
        message = message.replace(s.as_str(), "[sas]");
        for part in s.split('&').filter(|p| p.len() > 8) {
            message = message.replace(part, "[sas]");
        }
    }
    Error { status: Some(status), code, message }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_check_as_elasticsearch_does() {
        let e = |private: bool| crate::egress::Egress::of(|_| None, private).unwrap();
        assert_eq!(LocationMode::parse("Primary_Then_Secondary"), Ok(LocationMode::PrimaryThenSecondary));
        assert_eq!(LocationMode::parse("bogus").unwrap_err(), "No enum constant org.elasticsearch.repositories.azure.LocationMode.BOGUS");
        let (p, s) = endpoints("acct", None, None, None, LocationMode::PrimaryOnly, &e(false)).unwrap();
        assert_eq!((p.as_str(), s.unwrap().as_str()), ("https://acct.blob.core.windows.net/", "https://acct-secondary.blob.core.windows.net/"));
        assert!(endpoints("acct", Some("x.net"), Some("https://e.example.com"), None, LocationMode::PrimaryOnly, &e(false)).unwrap_err().starts_with("Both an endpoint suffix as well as a primary endpoint"));
        assert!(endpoints("acct", None, None, Some("https://e.example.com"), LocationMode::PrimaryOnly, &e(false)).unwrap_err().starts_with("A primary endpoint is required"));
        assert!(endpoints("acct", None, Some("https://e.example.com"), None, LocationMode::SecondaryOnly, &e(false)).unwrap_err().starts_with("Unable to configure an AzureClient using a secondary location"));
        assert!(endpoints("acct", None, Some("http://127.0.0.1:10000/devstoreaccount1"), None, LocationMode::PrimaryOnly, &e(false)).is_err());
        assert!(endpoints("acct", None, Some("http://127.0.0.1:10000/devstoreaccount1"), None, LocationMode::PrimaryOnly, &e(true)).is_ok());
        assert!(Auth::new("acct", Some("a2V5"), Some("sv=1&sig=x")).unwrap_err().starts_with("Both a secret as well as a shared access token"));
        let e = Auth::new("acct", Some("not base64 SECRETKEY!"), None).unwrap_err();
        assert!(!e.contains("SECRETKEY"), "{e}");
        assert!(valid_container("elasticsearch-snapshots") && !valid_container("a--b") && !valid_container("Up") && !valid_container("x/y"));
    }

    /// Shared Key's string to sign, as Azure's documentation spells it for a blob request with
    /// a query (canonicalized resource `/<account><path>\n<param>:<value>`).
    #[test]
    fn shared_key_signs_the_documented_string() {
        let c = Client::new("acct", Auth::Key(b"key".to_vec()), reqwest::Url::parse("https://acct.blob.core.windows.net").unwrap(), None, LocationMode::PrimaryOnly, "snaps", &crate::egress::Egress::of(|_| None, false).unwrap()).unwrap();
        let url = c.url(&c.primary, "a/b c", &[("comp", "block"), ("blockid", "QQ==")]);
        assert_eq!(url.as_str(), "https://acct.blob.core.windows.net/snaps/a/b%20c?comp=block&blockid=QQ%3D%3D");
        let headers = vec![("x-ms-version".to_string(), VERSION.to_string()), ("x-ms-date".to_string(), "Mon, 01 Jan 2024 00:00:00 GMT".to_string())];
        let want = format!("PUT\n\n\n3\n\n\n\n\n\n\n\n\nx-ms-date:Mon, 01 Jan 2024 00:00:00 GMT\nx-ms-version:{VERSION}\n/acct/snaps/a/b%20c\nblockid:QQ==\ncomp:block");
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(b"key").unwrap();
        m.update(want.as_bytes());
        let sig = base64::engine::general_purpose::STANDARD.encode(m.finalize().into_bytes());
        assert_eq!(c.sign(b"key", "PUT", &url, &[("comp", "block"), ("blockid", "QQ==")], &headers, 3), format!("SharedKey acct:{sig}"));
    }
}
