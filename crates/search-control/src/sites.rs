//! The web crawler's sites, kept in the catalogue every node shares (docs/search/crawler.md,
//! "Domain verification"): each owner's verified sites, and the owner's crawls of sites it hasn't
//! verified (the daily limit, counted across the fleet). Clusters reach them through
//! [`CatalogSites`] (`ClusterConfig::sites`); people through `/v1/search/clusters/{id}/
//! site-verifications`, which act for the cluster's owner, so every cluster and connector of
//! one owner shares its verifications and none starts them over.
//!
//! A verification is re-checked every [`RECHECK_AFTER`] ([`Control::start_site_rechecks`]); one
//! that fails then lapses, and the site's crawls fall back to the unverified limits.

use crate::control::{Control, ControlError, Result};
use crate::store::{SearchCluster, SiteVerification, Store};
use search_es::egress::Egress;
use search_es::xpack::entsearch::sync::crawler::verify;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// How long a verification holds before it is checked again.
pub const RECHECK_AFTER: Duration = Duration::from_secs(30 * 86_400);
/// How often nodes look for verifications due a re-check, and how many one look takes.
const RECHECK_EVERY: Duration = Duration::from_secs(3600);
const RECHECK_BATCH: i64 = 50;

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
    async fn verified(&self, owner: &str, sites: &[String]) -> Vec<String> {
        // A catalogue that can't be read verifies nothing: the stricter limits apply.
        self.store.verified_sites(owner, sites).await.unwrap_or_default()
    }

    async fn record_crawl(&self, owner: &str, sites: &[String], per_day: u64, now_ms: i64) -> std::result::Result<Option<(String, i64)>, String> {
        self.store.record_site_crawl(owner, sites, per_day, now_ms).await.map_err(|e| e.to_string())
    }
}

/// The crawler's egress rules, as a crawl on this node has them (its `allow_private_hosts`).
fn egress(c: &Control) -> Egress {
    let allow_private = c.cfg.node_settings.iter().rev().find(|(k, _)| k == search_es::xpack::entsearch::sync::ALLOW_PRIVATE).is_some_and(|(_, v)| v.eq_ignore_ascii_case("true"));
    Egress::of(|k| c.cfg.node_settings.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.as_str()), allow_private).unwrap_or_else(|_| Egress::of(|_| None, false).expect("empty rules parse"))
}

/// A verification as it is shown: what to publish, and where it stands.
fn shown(v: &SiteVerification) -> Value {
    json!({
        "site": v.site,
        "status": v.status,
        "proof": v.proof,
        "error": v.error,
        "verified_at": v.verified_at,
        "checked_at": v.checked_at,
        "dns": { "name": v.site, "type": "TXT", "value": format!("{}{}", verify::TXT_PREFIX, v.token) },
        "file": { "url": format!("{}{}", v.origin, verify::FILE), "content": v.token },
    })
}

impl Control {
    /// The verifications of cluster `c`'s owner.
    pub async fn site_verifications(&self, c: &SearchCluster) -> Result<Value> {
        let all = self.store.verifications(&owner_of(c)).await?;
        Ok(json!({ "verifications": all.iter().map(shown).collect::<Vec<_>>() }))
    }

    /// Starts verifying `domain` (a domain or URL) for cluster `c`'s owner: its site and the
    /// token to publish (the same token again for a site already started).
    pub async fn start_site_verification(&self, c: &SearchCluster, domain: &str) -> Result<Value> {
        let (site, file) = verify::target(domain, &egress(self)).map_err(ControlError::Invalid)?;
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|e| ControlError::Internal(anyhow::anyhow!("os randomness: {e}")))?;
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let origin = file.origin();
        let v = self.store.start_verification(&owner_of(c), &site, &origin, &token, crate::meter::now_ms()).await?;
        Ok(shown(&v))
    }

    /// Checks owner's verification of `site` now: verified when the token is published, else
    /// why not.
    pub async fn check_site_verification(&self, c: &SearchCluster, site: &str) -> Result<Value> {
        let owner = owner_of(c);
        let v = self.store.verification(&owner, site).await?.ok_or_else(|| ControlError::NotFound(format!("no verification of [{site}]: start one first")))?;
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
        let file = search_es::xpack::entsearch::sync::crawler::url::Link::parse(&format!("{}{}", v.origin, verify::FILE), None).ok_or_else(|| ControlError::Invalid(format!("[{}] isn't a URL", v.origin)))?;
        let now = crate::meter::now_ms();
        match verify::check(&egress(self), &v.site, &file, &v.token).await {
            Ok(proof) => self.store.record_check(&v.owner, &v.site, Some(proof.as_str()), None, now).await?,
            Err(why) => self.store.record_check(&v.owner, &v.site, None, Some(&why), now).await?,
        }
        Ok(())
    }

    /// `crawl-test` for cluster `c`: `url` through the crawler's own path, with connector
    /// `connector`'s configuration or a crawl of the URL's site. Runs on the cluster's owner
    /// (the route's), loading it if it sleeps; writes nothing.
    pub async fn crawl_test(&self, c: &SearchCluster, url: &str, connector: Option<&str>) -> Result<Value> {
        let loaded = self.node.open(crate::control::spec_for(c)).await?;
        let v = search_es::xpack::entsearch::sync::crawler::probe::test(&loaded.cluster, connector, url).await.map_err(ControlError::Invalid)?;
        serde_json::from_str(&v.to_json()).map_err(|e| ControlError::Internal(e.into()))
    }

    /// Re-checks verifications older than [`RECHECK_AFTER`], every [`RECHECK_EVERY`]: each is
    /// claimed in the catalogue first, so on a fleet one node checks it.
    pub(crate) fn start_site_rechecks(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RECHECK_EVERY).await;
                let Some(c) = me.upgrade() else { return };
                let now = crate::meter::now_ms();
                let due = match c.store.claim_rechecks(now - RECHECK_AFTER.as_millis() as i64, now, RECHECK_BATCH).await {
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
