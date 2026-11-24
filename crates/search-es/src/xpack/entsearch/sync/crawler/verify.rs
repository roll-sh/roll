//! Domain verification (docs/search/crawler.md, "Domain verification"): an owner proves control of
//! a site (its registrable domain, [`super::hosts::site_of`]) with a token roll gave it, as a DNS
//! TXT record `roll-site-verification=<token>` on the site, or as the file
//! [`FILE`] on the site holding the token. A crawl whose every site its owner verified gets the
//! verified limits. Verifications, like each owner's crawls of unverified sites, are kept by the
//! host outside any cluster's state ([`Sites`]: `search-control`'s catalogue, shared by the
//! fleet), so no new cluster or connector starts them over.

use super::fetch::Fetcher;
use super::hosts::site_of;
use super::url::Link;
use crate::egress::Egress;

/// Where the token file is, on the site.
pub const FILE: &str = "/.well-known/roll-site-verification.txt";
/// What a TXT record holding the token starts with.
pub const TXT_PREFIX: &str = "roll-site-verification=";
/// Redirects the file may take, each within the site.
const REDIRECTS: usize = 5;
/// The most of the file read.
const FILE_BYTES: usize = 4 << 10;

/// What the host keeps for crawls: owners' verified sites, and owners' crawls of sites they
/// haven't verified, counted across the fleet.
#[async_trait::async_trait]
pub trait Sites: Send + Sync {
    /// Of `sites`, those `owner` has verified (and whose last check still holds).
    async fn verified(&self, owner: &str, sites: &[String]) -> Vec<String>;
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

/// How a site was proven.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proof {
    Dns,
    File,
}

impl Proof {
    pub fn as_str(self) -> &'static str {
        match self {
            Proof::Dns => "dns",
            Proof::File => "file",
        }
    }
}

/// The site a domain or URL names, and the origin its file is read from (`https://<host>`,
/// or the URL's own origin under `allow_private_hosts` for a local test site).
pub fn target(domain_or_url: &str, egress: &Egress) -> Result<(String, Link), String> {
    let text = domain_or_url.trim();
    let url = if text.contains("://") { text.to_string() } else { format!("https://{text}/") };
    let link = Link::parse(&url, None).ok_or_else(|| format!("[{text}] isn't a domain"))?;
    if link.url().scheme() != "https" && !egress.allow_private() {
        return Err(format!("[{text}]: a site is verified over https"));
    }
    let site = site_of(&link);
    let file = Link::parse(&format!("{}{FILE}", link.origin()), None).ok_or_else(|| format!("[{text}] isn't a domain"))?;
    Ok((site, file))
}

/// Checks that `token` is published for `site`: as a TXT record on it, or in its file (read
/// through the crawler's own fetch path and egress check, following redirects only within the
/// site, never from another one).
pub async fn check(egress: &Egress, site: &str, file: &Link, token: &str) -> Result<Proof, String> {
    let dns = txt_has(site, token).await;
    if dns {
        return Ok(Proof::Dns);
    }
    let fetcher = Fetcher::new(egress.clone(), None)?;
    let mut at = file.clone();
    for _ in 0..=REDIRECTS {
        if site_of(&at) != site {
            return Err(format!("the file redirected off the site, to [{}]: a token from another site proves nothing", at.as_str()));
        }
        if at.url().scheme() != "https" && !egress.allow_private() {
            return Err(format!("the file redirected to [{}], which isn't https", at.as_str()));
        }
        let resp = fetcher.get(&at, false, FILE_BYTES).await.map_err(|f| format!("reading [{}] failed: {}", at.as_str(), f.class()))?;
        match resp.status {
            200..=299 => {
                let body = String::from_utf8_lossy(&resp.body);
                return match body.lines().any(|l| l.trim() == token) {
                    true => Ok(Proof::File),
                    false => Err(format!("[{}] doesn't hold the token, and no TXT record {TXT_PREFIX}… on [{site}] does", at.as_str())),
                };
            }
            300..=399 => match resp.location.as_deref().and_then(|l| Link::parse(l, Some(at.url()))) {
                Some(next) => at = next,
                None => return Err(format!("[{}] redirected nowhere", at.as_str())),
            },
            s => return Err(format!("no TXT record {TXT_PREFIX}… on [{site}], and [{}] answered {s}", at.as_str())),
        }
    }
    Err(format!("[{}] redirected more than {REDIRECTS} times", file.as_str()))
}

/// Whether a TXT record on `site` is `roll-site-verification=<token>`.
async fn txt_has(site: &str, token: &str) -> bool {
    use hickory_resolver::proto::rr::{RData, RecordType};
    static R: std::sync::OnceLock<Option<hickory_resolver::TokioResolver>> = std::sync::OnceLock::new();
    // A site that's an address (a local test site) has no TXT records.
    if site.parse::<std::net::IpAddr>().is_ok() || site.contains(':') {
        return false;
    }
    let Some(r) = R.get_or_init(|| hickory_resolver::TokioResolver::builder_tokio().ok().and_then(|b| b.build().ok())) else { return false };
    let Ok(Ok(found)) = tokio::time::timeout(std::time::Duration::from_secs(5), r.lookup(format!("{site}."), RecordType::TXT)).await else { return false };
    let want = format!("{TXT_PREFIX}{token}");
    found.answers().iter().any(|rec| match &rec.data {
        RData::TXT(t) => t.txt_data.iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect::<String>().trim() == want,
        _ => false,
    })
}
