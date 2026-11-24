//! Site verification (docs/search/crawler.md, "Domain verification"), scoped as Google Search
//! Console scopes its properties:
//!
//! - a **domain** (a registrable domain, [`super::hosts::site_of`]) is proven by a DNS TXT record
//!   `roll-site-verification=<token>` on it, and covers every origin on it;
//! - an **origin** (`scheme://host[:port]`, exactly) is proven by the file [`FILE`] that origin
//!   serves itself, redirects only within it, and covers that origin alone. Controlling one host
//!   or port of a domain proves nothing about its others.
//!
//! A crawl whose every origin its owner verified gets the verified limits. Verifications, like
//! each owner's crawls of unverified sites, are kept by the host outside any cluster's state
//! ([`Sites`]: `search-control`'s catalogue, shared by the fleet), so no new cluster or connector
//! starts them over.

use super::fetch::Fetcher;
use super::hosts::site_of;
use super::url::Link;
use crate::egress::Egress;
use std::time::Duration;

/// Where the token file is, on the origin.
pub const FILE: &str = "/.well-known/roll-site-verification.txt";
/// What a TXT record holding the token starts with.
pub const TXT_PREFIX: &str = "roll-site-verification=";
/// Redirects the file may take, each within its origin.
const REDIRECTS: usize = 5;
/// The most of the file read.
const FILE_BYTES: usize = 4 << 10;

/// What the host keeps for crawls: owners' verifications, and owners' crawls of sites they
/// haven't verified, counted across the fleet.
#[async_trait::async_trait]
pub trait Sites: Send + Sync {
    /// Of `origins` (a crawl's, `scheme://host[:port]`), those `owner` has verified, by the
    /// origin's file or its domain's DNS record (and whose last check still holds).
    async fn verified(&self, owner: &str, origins: &[String]) -> Vec<String>;
    /// Records a crawl of the unverified `sites` by `owner` at `now_ms`, or (recording nothing)
    /// the first that had `per_day` crawls by that owner in 24 hours and when the next may
    /// start. `Err`: the record couldn't be read or written.
    async fn record_crawl(&self, owner: &str, sites: &[String], per_day: u64, now_ms: i64) -> Result<Option<(String, i64)>, String>;
}

impl std::fmt::Debug for dyn Sites {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sites")
    }
}

/// What a verification covers, and so how it is proven.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A registrable domain and every origin on it, by DNS.
    Domain,
    /// One origin, by its file.
    Origin,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Domain => "domain",
            Kind::Origin => "origin",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "domain" => Some(Kind::Domain),
            "origin" => Some(Kind::Origin),
            _ => None,
        }
    }
}

/// What `text` asks to verify: a URL is its origin exactly (`https` unless private hosts are
/// allowed, for a local test site); anything else is a domain, verified as its registrable
/// domain.
pub fn target(text: &str, egress: &Egress) -> Result<(Kind, String), String> {
    let text = text.trim();
    if text.contains("://") {
        let link = Link::parse(text, None).ok_or_else(|| format!("[{text}] isn't an http or https URL"))?;
        if link.url().scheme() != "https" && !egress.allow_private() {
            return Err(format!("[{text}]: an origin is verified over https"));
        }
        return Ok((Kind::Origin, link.origin()));
    }
    let link = Link::parse(&format!("https://{text}/"), None).filter(|l| l.url().path() == "/" && l.url().port().is_none()).ok_or_else(|| format!("[{text}] isn't a domain (give a URL to verify one origin)"))?;
    let host = link.url().host_str().unwrap_or_default();
    if host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') {
        return Err(format!("[{text}] is an address, which has no DNS records: verify its origin by URL instead"));
    }
    Ok((Kind::Domain, site_of(&link)))
}

/// Whether verification `key` of `kind` covers crawl origin `origin`.
pub fn covers(kind: Kind, key: &str, origin: &str) -> bool {
    match kind {
        Kind::Origin => key == origin,
        Kind::Domain => Link::parse(&format!("{origin}/"), None).is_some_and(|l| site_of(&l) == key),
    }
}

/// Checks that `token` is published for verification `key` of `kind`: a TXT record on the
/// domain, or the origin's file (read through the crawler's own fetch path and egress check,
/// each request in the site's turn `floor` apart, redirects followed only within the origin).
pub async fn check(egress: &Egress, kind: Kind, key: &str, token: &str, floor: Duration) -> Result<(), String> {
    if kind == Kind::Domain {
        return match txt_has(key, token).await {
            true => Ok(()),
            false => Err(format!("no TXT record {TXT_PREFIX}<token> on [{key}] (DNS changes can take a while to be seen)")),
        };
    }
    let file = Link::parse(&format!("{key}{FILE}"), None).ok_or_else(|| format!("[{key}] isn't an origin"))?;
    let fetcher = Fetcher::new(egress.clone(), None)?;
    let mut at = file.clone();
    for _ in 0..=REDIRECTS {
        if at.origin() != key {
            return Err(format!("the file redirected to [{}], another origin: a token another origin serves proves nothing about [{key}]", at.as_str()));
        }
        super::polite_turn(&at, floor).await?;
        let resp = fetcher.get(&at, false, FILE_BYTES).await.map_err(|f| format!("reading [{}] failed: {}", at.as_str(), f.class()))?;
        match resp.status {
            200..=299 => {
                let body = String::from_utf8_lossy(&resp.body);
                return match body.lines().any(|l| l.trim() == token) {
                    true => Ok(()),
                    false => Err(format!("[{}] doesn't hold the token", at.as_str())),
                };
            }
            300..=399 => match resp.location.as_deref().and_then(|l| Link::parse(l, Some(at.url()))) {
                Some(next) => at = next,
                None => return Err(format!("[{}] redirected nowhere", at.as_str())),
            },
            s => return Err(format!("[{}] answered {s}", at.as_str())),
        }
    }
    Err(format!("[{}] redirected more than {REDIRECTS} times", file.as_str()))
}

/// Whether a TXT record on `domain` is `roll-site-verification=<token>`.
async fn txt_has(domain: &str, token: &str) -> bool {
    use hickory_resolver::proto::rr::{RData, RecordType};
    // Built once it can be: a failed build (no resolver configuration yet) is tried again.
    static R: std::sync::OnceLock<hickory_resolver::TokioResolver> = std::sync::OnceLock::new();
    let r = match R.get() {
        Some(r) => r,
        None => match hickory_resolver::TokioResolver::builder_tokio().ok().and_then(|b| b.build().ok()) {
            Some(built) => R.get_or_init(|| built),
            None => return false,
        },
    };
    let Ok(Ok(found)) = tokio::time::timeout(Duration::from_secs(5), r.lookup(format!("{domain}."), RecordType::TXT)).await else { return false };
    let want = format!("{TXT_PREFIX}{token}");
    found.answers().iter().any(|rec| match &rec.data {
        RData::TXT(t) => t.txt_data.iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect::<String>().trim() == want,
        _ => false,
    })
}
