//! The web crawler's sites, kept in the catalogue every node shares (docs/search/crawler.md,
//! "Domain verification"): each owner's verified domains and origins, and the owner's crawls of
//! sites it hasn't verified (the daily limit, counted across the fleet). Clusters reach them
//! through [`CatalogSites`] (`ClusterConfig::sites`); people through `/v1/search/clusters/{id}/
//! site-verifications`, which act for the cluster's owner, so every cluster and connector of
//! one owner shares its verifications and none starts them over.
//!
//! A domain is verified by a DNS TXT record and covers every origin on it; an origin by the file
//! it serves itself, and covers that origin alone (`verify`). A verification is re-checked every
//! 30 days ([`Control::start_site_rechecks`]); a failing one is retried, then lapses, and its
//! crawls fall back to the unverified limits. Checks and `crawl-test` make roll send requests, so
//! each owner gets [`PROBES_PER_MINUTE`] of them on a node.

use crate::control::{Control, ControlError, Result};
use crate::store::{MAX_PENDING, PENDING_FOR, SearchCluster, SiteVerification, Store};
use search_es::egress::Egress;
use search_es::xpack::entsearch::sync::crawler::{self, verify};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often nodes look for verifications due a re-check, and how many one look takes.
const RECHECK_EVERY: Duration = Duration::from_secs(3600);
const RECHECK_BATCH: i64 = 50;
/// `crawl-test`s and verification checks one owner may start in a minute, on one node.
pub const PROBES_PER_MINUTE: usize = 10;

/// Whose verifications a cluster's are: its owner, or the operator's for clusters without one
/// (as the crawler counts them: `search_es` `crawler::sync`).
pub fn owner_of(c: &SearchCluster) -> String {
    c.owner.clone().unwrap_or_else(|| "(no owner)".to_string())
}

/// The catalogue as clusters see it.
pub struct CatalogSites {
    pub store: Arc<Store>,
}

#[async_trait::async_trait]
impl verify::Sites for CatalogSites {
    async fn verified(&self, owner: &str, origins: &[String]) -> Vec<String> {
        // A catalogue that can't be read verifies nothing: the stricter limits apply.
        let held = self.store.verified_sites(owner).await.unwrap_or_default();
        origins.iter().filter(|o| held.iter().any(|(key, kind)| verify::Kind::parse(kind).is_some_and(|k| verify::covers(k, key, o)))).cloned().collect()
    }

    async fn record_crawl(&self, owner: &str, sites: &[String], per_day: u64, now_ms: i64) -> std::result::Result<Option<(String, i64)>, String> {
        self.store.record_site_crawl(owner, sites, per_day, now_ms).await.map_err(|e| e.to_string())
    }
}

fn setting<'a>(c: &'a Control, key: &str) -> Option<&'a str> {
    c.cfg.node_settings.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// The crawler's egress rules, as a crawl on this node has them (its `allow_private_hosts`);
/// settings that don't parse let nothing out.
fn egress(c: &Control) -> Egress {
    let allow_private = setting(c, search_es::xpack::entsearch::sync::ALLOW_PRIVATE).is_some_and(|v| v.eq_ignore_ascii_case("true"));
    Egress::of(|k| setting(c, k), allow_private).unwrap_or_else(|_| Egress::deny_all())
}

/// A verification as it is shown: what to publish, and where it stands.
fn shown(v: &SiteVerification) -> Value {
    let publish = match v.kind.as_str() {
        "domain" => json!({ "dns": { "name": v.site, "type": "TXT", "value": format!("{}{}", verify::TXT_PREFIX, v.token) } }),
        _ => json!({ "file": { "url": format!("{}{}", v.site, verify::FILE), "content": v.token } }),
    };
    let mut out = json!({
        "site": v.site,
        "kind": v.kind,
        "status": v.status,
        "error": v.error,
        "verified_at": v.verified_at,
        "checked_at": v.checked_at,
        "next_check_at": v.due_at,
    });
    if let (Some(o), Some(p)) = (out.as_object_mut(), publish.as_object()) {
        o.extend(p.clone());
    }
    out
}

/// Each owner's probes in the last minute, on this node.
fn probes() -> &'static parking_lot::Mutex<HashMap<String, Vec<Instant>>> {
    static P: std::sync::LazyLock<parking_lot::Mutex<HashMap<String, Vec<Instant>>>> = std::sync::LazyLock::new(Default::default);
    &P
}

/// Takes one of `owner`'s [`PROBES_PER_MINUTE`], or says when the next is free (429).
fn take_probe(owner: &str) -> Result<()> {
    let now = Instant::now();
    let mut all = probes().lock();
    if all.len() > 10_000 {
        all.retain(|_, v| v.iter().any(|t| now.duration_since(*t) < Duration::from_secs(60)));
    }
    let mine = all.entry(owner.to_string()).or_default();
    mine.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
    if mine.len() >= PROBES_PER_MINUTE {
        let free = 60u64.saturating_sub(mine.first().map_or(0, |t| now.duration_since(*t).as_secs()));
        return Err(ControlError::Busy(format!("at most {PROBES_PER_MINUTE} crawl tests and verification checks a minute: try again in {free}s")));
    }
    mine.push(now);
    Ok(())
}

/// Refuses while web crawlers are off on this node.
fn crawlers_on(c: &Control) -> Result<()> {
    match crawler::enabled(|k| setting(c, k)) {
        true => Ok(()),
        false => Err(ControlError::Invalid(format!("web crawlers are turned off on this node ({})", crawler::ENABLED))),
    }
}

impl Control {
    /// The verifications of cluster `c`'s owner.
    pub async fn site_verifications(&self, c: &SearchCluster) -> Result<Value> {
        let all = self.store.verifications(&owner_of(c)).await?;
        Ok(json!({ "verifications": all.iter().map(shown).collect::<Vec<_>>() }))
    }

    /// Starts verifying `text` for cluster `c`'s owner, a domain or (`origin`) a URL's origin:
    /// what to publish (the same token again for one already started). 409 past [`MAX_PENDING`]
    /// pending ones.
    pub async fn start_site_verification(&self, c: &SearchCluster, text: &str, origin: bool) -> Result<Value> {
        let (kind, key) = verify::target(text, &egress(self)).map_err(ControlError::Invalid)?;
        match (kind, origin) {
            (verify::Kind::Origin, false) => return Err(ControlError::Invalid(format!("[{text}] is a URL: give it as origin, verified by the file that origin serves"))),
            (verify::Kind::Domain, true) => return Err(ControlError::Invalid(format!("[{text}] isn't a URL: give an origin as https://host[:port], or a domain as domain"))),
            _ => {}
        }
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|e| ControlError::Internal(anyhow::anyhow!("os randomness: {e}")))?;
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        match self.store.start_verification(&owner_of(c), &key, kind.as_str(), &token, crate::meter::now_ms()).await? {
            Some(v) => Ok(shown(&v)),
            None => Err(ControlError::Conflict(format!("{MAX_PENDING} verifications are pending already: check or delete some (pending ones are dropped after {} days)", PENDING_FOR / 86_400_000))),
        }
    }

    /// Checks owner's verification `site` now: verified when the token is published, else why
    /// not.
    pub async fn check_site_verification(&self, c: &SearchCluster, site: &str) -> Result<Value> {
        crawlers_on(self)?;
        let owner = owner_of(c);
        let v = self.store.verification(&owner, site).await?.ok_or_else(|| ControlError::NotFound(format!("no verification of [{site}]: start one first")))?;
        take_probe(&owner)?;
        self.check_site(&v).await?;
        let v = self.store.verification(&owner, site).await?.ok_or_else(|| ControlError::NotFound(format!("no verification of [{site}]")))?;
        Ok(shown(&v))
    }

    pub async fn delete_site_verification(&self, c: &SearchCluster, site: &str) -> Result<Value> {
        match self.store.delete_verification(&owner_of(c), site).await? {
            true => Ok(json!({ "deleted": site })),
            false => Err(ControlError::NotFound(format!("no verification of [{site}]"))),
        }
    }

    async fn check_site(&self, v: &SiteVerification) -> Result<()> {
        let kind = verify::Kind::parse(&v.kind).ok_or_else(|| ControlError::Internal(anyhow::anyhow!("verification [{}] has kind [{}]", v.site, v.kind)))?;
        let floor = crawler::unverified_floor(|k| setting(self, k));
        let outcome = verify::check(&egress(self), kind, &v.site, &v.token, floor).await;
        self.store.record_check(&v.owner, &v.site, outcome.as_ref().map(|_| ()).map_err(String::as_str), crate::meter::now_ms()).await?;
        Ok(())
    }

    /// `crawl-test` for cluster `c`: `url` through the crawler's own path, with connector
    /// `connector`'s configuration or a crawl of the URL's site. Runs on the cluster's owner
    /// (the route's), loading it if it sleeps; writes nothing.
    pub async fn crawl_test(&self, c: &SearchCluster, url: &str, connector: Option<&str>) -> Result<Value> {
        crawlers_on(self)?;
        take_probe(&owner_of(c))?;
        let loaded = self.node.open(crate::control::spec_for(c)).await?;
        let v = crawler::probe::test(&loaded.cluster, connector, url).await.map_err(ControlError::Invalid)?;
        serde_json::from_str(&v.to_json()).map_err(|e| ControlError::Internal(e.into()))
    }

    /// Re-checks verifications that are due (`store::after_check`), every [`RECHECK_EVERY`]:
    /// each is claimed in the catalogue first, so on a fleet one node checks it. Nothing is checked
    /// while crawlers are off on the node.
    pub(crate) fn start_site_rechecks(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RECHECK_EVERY).await;
                let Some(c) = me.upgrade() else { return };
                if crawlers_on(&c).is_err() {
                    continue;
                }
                let due = match c.store.claim_rechecks(crate::meter::now_ms(), RECHECK_BATCH).await {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(error = %e, "listing site verifications to re-check failed");
                        continue;
                    }
                };
                for v in due {
                    if let Err(e) = c.check_site(&v).await {
                        tracing::warn!(site = v.site, error = %e, "re-checking a site verification failed");
                    }
                }
            }
        });
    }
}
