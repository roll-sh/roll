//! One HTTP request of a crawl. Every URL is the tenant's choice, so the client resolves names
//! to public addresses only ([`roll_net::PublicOnly`]), refuses private literals
//! ([`roll_net::literal_private_host`]), uses no proxy and follows no redirect (the crawler
//! follows them by hand, checking each hop like a link). A response is read up to a cap that
//! counts decompressed bytes, so a compression bomb stops there; headers are capped too.

use super::url::{Link, origin_of};
use std::io::Write;
use std::time::Duration;

/// What the crawler sends as `User-Agent`.
pub fn user_agent() -> String {
    format!("roll-crawler/{} (+https://roll.sh/docs/crawler)", env!("CARGO_PKG_VERSION"))
}

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// The most of a response's headers read.
pub const HEADER_BYTES_MAX: usize = 64 << 10;

/// The credential and the one origin it goes to.
pub struct Auth {
    pub origin: String,
    /// The whole `Authorization` value.
    pub header: String,
}

/// A decompressor that stops at a cap of decompressed bytes.
pub struct Inflate {
    inner: Inner,
    max: usize,
}

enum Inner {
    Gzip(flate2::write::GzDecoder<Vec<u8>>),
    Deflate(flate2::write::ZlibDecoder<Vec<u8>>),
    Raw(flate2::write::DeflateDecoder<Vec<u8>>),
    Plain(Vec<u8>),
}

impl Inflate {
    pub fn gzip(max: usize) -> Inflate {
        Inflate { inner: Inner::Gzip(flate2::write::GzDecoder::new(Vec::new())), max }
    }

    fn plain(max: usize) -> Inflate {
        Inflate { inner: Inner::Plain(Vec::new()), max }
    }

    fn deflate(max: usize) -> Inflate {
        Inflate { inner: Inner::Deflate(flate2::write::ZlibDecoder::new(Vec::new())), max }
    }

    fn len(&self) -> usize {
        match &self.inner {
            Inner::Gzip(d) => d.get_ref().len(),
            Inner::Deflate(d) => d.get_ref().len(),
            Inner::Raw(d) => d.get_ref().len(),
            Inner::Plain(v) => v.len(),
        }
    }

    /// Feeds `chunk` in small steps, failing as soon as the output passes the cap.
    pub fn write(&mut self, chunk: &[u8]) -> Result<(), String> {
        let too_large = || "the response is larger than max_response_size".to_string();
        if let Inner::Plain(v) = &mut self.inner {
            if v.len() + chunk.len() > self.max {
                return Err(too_large());
            }
            v.extend_from_slice(chunk);
            return Ok(());
        }
        for part in chunk.chunks(1 << 10) {
            let r = match &mut self.inner {
                Inner::Gzip(d) => d.write_all(part),
                Inner::Deflate(d) => d.write_all(part),
                Inner::Raw(d) => d.write_all(part),
                Inner::Plain(_) => Ok(()),
            };
            if let Err(e) = r {
                // Not zlib after all: servers send raw deflate as `deflate` too.
                if let Inner::Deflate(d) = &self.inner
                    && d.get_ref().is_empty()
                {
                    self.inner = Inner::Raw(flate2::write::DeflateDecoder::new(Vec::new()));
                    return self.write(chunk);
                }
                return Err(format!("the response doesn't decompress: {e}"));
            }
            if self.len() > self.max {
                return Err(too_large());
            }
        }
        Ok(())
    }

    pub fn finish(self) -> Result<Vec<u8>, String> {
        let max = self.max;
        let out = match self.inner {
            Inner::Gzip(d) => d.finish(),
            Inner::Deflate(d) => d.finish(),
            Inner::Raw(d) => d.finish(),
            Inner::Plain(v) => Ok(v),
        }
        .map_err(|e| format!("the response doesn't decompress: {e}"))?;
        if out.len() > max {
            return Err("the response is larger than max_response_size".into());
        }
        Ok(out)
    }
}

/// Why a request failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Timeout,
    Network,
    TooLarge,
    /// A refused address, an encoding the crawler doesn't read, headers over the cap.
    Refused,
}

impl Failure {
    pub fn class(self) -> &'static str {
        match self {
            Failure::Timeout => "timeout",
            Failure::Network => "network",
            Failure::TooLarge => "too_large",
            Failure::Refused => "refused",
        }
    }
}

/// A response, its body read.
pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub location: Option<String>,
    pub retry_after: Option<String>,
    pub body: Vec<u8>,
    /// Bytes received (headers and the body as sent): what the rate counts.
    pub wire_bytes: u64,
}

pub struct Fetcher {
    http: reqwest::Client,
    allow_private: bool,
    auth: Option<Auth>,
    ua: String,
}

impl Fetcher {
    pub fn new(allow_private: bool, auth: Option<Auth>) -> Result<Fetcher, String> {
        let mut b = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(SOCKET_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .http1_max_headers(128)
            .no_proxy();
        if !allow_private {
            b = b.dns_resolver(std::sync::Arc::new(roll_net::PublicOnly));
        }
        let http = b.build().map_err(|e| format!("the crawler's HTTP client: {e}"))?;
        Ok(Fetcher { http, allow_private, auth, ua: user_agent() })
    }

    /// Whether `link` may get the credential: its origin is exactly the credential's, the chain
    /// that led here never left that origin (`clean`), and the request is `https` (or, under
    /// `allow_private_hosts`, a loopback address, for local development).
    fn credential_for(&self, link: &Link, clean: bool) -> Option<&str> {
        let a = self.auth.as_ref()?;
        let url = link.url();
        let host = url.host_str().unwrap_or_default().trim_start_matches('[').trim_end_matches(']');
        let loopback = host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()) || host.eq_ignore_ascii_case("localhost");
        let secure = url.scheme() == "https" || (self.allow_private && loopback);
        (clean && secure && origin_of(url) == a.origin).then_some(a.header.as_str())
    }

    /// Whether `link`'s address is one the crawler may fetch from.
    pub fn refused(&self, link: &Link) -> bool {
        !self.allow_private && roll_net::literal_private_host(link.url())
    }

    /// One request, no redirect followed, within [`REQUEST_TIMEOUT`]. `clean`: every hop
    /// before this one was at the credential's origin.
    pub async fn get(&self, link: &Link, clean: bool, max: usize) -> Result<Response, Failure> {
        if self.refused(link) {
            return Err(Failure::Refused);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, self.request(link, clean, max)).await {
            Ok(r) => r,
            Err(_) => Err(Failure::Timeout),
        }
    }

    async fn request(&self, link: &Link, clean: bool, max: usize) -> Result<Response, Failure> {
        let mut req = self
            .http
            .get(link.url().clone())
            .header("User-Agent", &self.ua)
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("Accept-Encoding", "gzip, deflate");
        if let Some(h) = self.credential_for(link, clean) {
            req = req.header("Authorization", h);
        }
        let mut resp = req.send().await.map_err(|e| if e.is_timeout() { Failure::Timeout } else { Failure::Network })?;
        let headers = resp.headers();
        let header_bytes: usize = headers.iter().map(|(k, v)| k.as_str().len() + v.len() + 4).sum();
        if header_bytes > HEADER_BYTES_MAX {
            return Err(Failure::Refused);
        }
        let text = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
        let status = resp.status().as_u16();
        let content_type = text("content-type").unwrap_or_default().to_ascii_lowercase();
        let (location, retry_after) = (text("location"), text("retry-after"));
        let mut inflate = match text("content-encoding").map(|e| e.trim().to_ascii_lowercase()).as_deref() {
            None | Some("" | "identity") => Inflate::plain(max),
            Some("gzip" | "x-gzip") => Inflate::gzip(max),
            Some("deflate") => Inflate::deflate(max),
            Some(_) => return Err(Failure::Refused),
        };
        // Redirects and errors aren't read past a small part.
        let max = if (200..300).contains(&status) { max } else { 64 << 10 };
        if (200..300).contains(&status) && resp.content_length().is_some_and(|n| n > max as u64) && matches!(inflate.inner, Inner::Plain(_)) {
            return Err(Failure::TooLarge);
        }
        let mut wire = header_bytes as u64;
        let mut read = 0usize;
        loop {
            let chunk = resp.chunk().await.map_err(|e| if e.is_timeout() { Failure::Timeout } else { Failure::Network })?;
            let Some(chunk) = chunk else { break };
            wire += chunk.len() as u64;
            read += chunk.len();
            if !(200..300).contains(&status) && read > max {
                break;
            }
            inflate.write(&chunk).map_err(|e| if e.contains("larger") { Failure::TooLarge } else { Failure::Refused })?;
        }
        let body = if (200..300).contains(&status) { inflate.finish().map_err(|e| if e.contains("larger") { Failure::TooLarge } else { Failure::Refused })? } else { Vec::new() };
        Ok(Response { status, content_type, location, retry_after, body, wire_bytes: wire })
    }
}

/// `Retry-After` (seconds, or an HTTP date) as a wait.
pub fn retry_after(v: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let v = v.trim();
    if let Ok(s) = v.parse::<u64>() {
        return Some(Duration::from_secs(s));
    }
    let at = chrono::DateTime::parse_from_rfc2822(v).ok()?;
    Some((at.with_timezone(&chrono::Utc) - now).to_std().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_credential_stays_on_its_origin() {
        let auth = || Some(Auth { origin: "https://docs.example.com".into(), header: "Bearer t".into() });
        let f = Fetcher::new(false, auth()).unwrap();
        let l = |s: &str| Link::parse(s, None).unwrap();
        assert_eq!(f.credential_for(&l("https://docs.example.com/a"), true), Some("Bearer t"));
        assert_eq!(f.credential_for(&l("https://docs.example.com/a"), false), None, "after another origin");
        assert_eq!(f.credential_for(&l("http://docs.example.com/a"), true), None, "never over http");
        assert_eq!(f.credential_for(&l("https://docs.example.com:8443/a"), true), None);
        assert_eq!(f.credential_for(&l("https://evil.docs.example.com/a"), true), None);
        let dev = Fetcher::new(true, Some(Auth { origin: "http://127.0.0.1:9".into(), header: "x".into() })).unwrap();
        assert_eq!(dev.credential_for(&l("http://127.0.0.1:9/a"), true), Some("x"), "loopback under allow_private_hosts");
        assert!(f.refused(&l("http://169.254.169.254/latest")) && !dev.refused(&l("http://127.0.0.1:9/")));
    }
}
