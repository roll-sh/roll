//! The web crawler's sites, kept in the catalogue every node shares (docs/search/crawler.md,
//! "Domain verification"): each cluster's verified domains and origins, and each owner's crawls
//! of sites they haven't verified (the daily limit, counted across the fleet). Clusters reach
//! them through [`CatalogSites`] (`ClusterConfig::sites`); people through
//! `/v1/search/clusters/{id}/site-verifications`, decided on that cluster. A verification is the
//! cluster's, its token the owner's for the site (the same in each of their clusters), so a
//! record published once proves the site for another of their clusters in one check, while a
//! grant on one cluster touches only that cluster's verifications.
//!
//! A domain is verified by a DNS TXT record and covers every origin on it; an origin by the file
//! it serves itself, and covers that origin alone (`verify`). A verification is re-checked every
//! 30 days ([`Control::start_site_rechecks`]); a failing one is retried, then lapses, and its
//! crawls fall back to the unverified limits. Checks and `crawl-test` make roll send requests, so
//! each owner gets [`PROBES_PER_MINUTE`] of them on a node.

use crate::control::{Control, ControlError, Result};
use crate::store::{DEFER_FOR, DEFER_UNTIL, PENDING_FOR, SearchCluster, SiteVerification, Started, Store};
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
/// (as the crawler counts them: `search_es` `crawler::sync`). The one identity a cluster has for
/// the crawler's sites: its verification tokens are derived from it (`Store::site_token`), and
/// the daily count and pending caps are counted by it.
pub fn owner_of(c: &SearchCluster) -> String {
    c.owner.clone().unwrap_or_else(|| "(no owner)".to_string())
}

/// The catalogue as clusters see it.
pub struct CatalogSites {
    pub store: Arc<Store>,
}

#[async_trait::async_trait]
impl verify::Sites for CatalogSites {
    async fn verified(&self, cluster: &str, origins: &[String]) -> Vec<String> {
        // A catalogue that can't be read verifies nothing: the stricter limits apply.
        let held = self.store.verified_sites(cluster).await.unwrap_or_default();
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
    /// Cluster `c`'s verifications.
    pub async fn site_verifications(&self, c: &SearchCluster) -> Result<Value> {
        let all = self.store.verifications(&c.id).await?;
        Ok(json!({ "verifications": all.iter().map(shown).collect::<Vec<_>>() }))
    }

    /// Starts verifying `text` for cluster `c`, a domain or (`origin`) a URL's origin: what to
    /// publish (the owner's token for the site, derived from the two: a record published once works
    /// at once in their other clusters, and again after every row that named it is gone). 409 past the pending caps (`store::MAX_PENDING_PER_CLUSTER` in the
    /// cluster, `store::MAX_PENDING` across the owner's). A host the crawler never fetches
    /// (blocked, or the fleet's own) can't be verified.
    pub async fn start_site_verification(&self, c: &SearchCluster, text: &str, origin: bool) -> Result<Value> {
        let (kind, key) = verify::target(text, &egress(self)).map_err(ControlError::Invalid)?;
        match (kind, origin) {
            (verify::Kind::Origin, false) => return Err(ControlError::Invalid(format!("[{text}] is a URL: give it as origin, verified by the file that origin serves"))),
            (verify::Kind::Domain, true) => return Err(ControlError::Invalid(format!("[{text}] isn't a URL: give an origin as https://host[:port], or a domain as domain"))),
            _ => {}
        }
        let blocked = crawler::blocked_hosts_of(|k| setting(self, k));
        let probe = match kind {
            verify::Kind::Origin => search_es::xpack::entsearch::sync::crawler::url::Link::parse(&format!("{key}/"), None),
            verify::Kind::Domain => search_es::xpack::entsearch::sync::crawler::url::Link::parse(&format!("https://{key}/"), None),
        };
        if probe.is_some_and(|l| crawler::is_blocked(&l, &blocked)) {
            return Err(ControlError::Invalid(format!("[{key}] is a host the crawler never fetches: it can't be verified")));
        }
        match self.store.start_verification(&c.id, &owner_of(c), &key, kind.as_str(), crate::meter::now_ms()).await? {
            Started::Row(v) => Ok(shown(&v)),
            Started::Full(cap, place) => Err(ControlError::Conflict(format!("{cap} verifications are pending {place} already: check or delete some (pending ones are dropped after {} days)", PENDING_FOR / 86_400_000))),
            Started::Gone => Err(ControlError::NotFound(format!("cluster [{}] is deleted", c.name))),
        }
    }

    /// Checks cluster `c`'s verification `site` now: verified when the token is published, else
    /// why not. A check that couldn't be made (the site busy) is a 429 and changes nothing.
    pub async fn check_site_verification(&self, c: &SearchCluster, site: &str) -> Result<Value> {
        crawlers_on(self)?;
        let v = self.store.verification(&c.id, site).await?.ok_or_else(|| ControlError::NotFound(format!("no verification of [{site}]: start one first")))?;
        take_probe(&owner_of(c))?;
        if let verify::Checked::NotChecked(why) = self.check_site(&v, false).await? {
            return Err(ControlError::Busy(why));
        }
        let v = self.store.verification(&c.id, site).await?.ok_or_else(|| ControlError::NotFound(format!("no verification of [{site}]")))?;
        Ok(shown(&v))
    }

    pub async fn delete_site_verification(&self, c: &SearchCluster, site: &str) -> Result<Value> {
        match self.store.delete_verification(&c.id, site).await? {
            true => Ok(json!({ "deleted": site })),
            false => Err(ControlError::NotFound(format!("no verification of [{site}]"))),
        }
    }

    /// Checks `v` and records a pass or a failure; a check that couldn't be made is recorded
    /// by neither (the caller decides). `background`: a re-check, which waits longer for a turn.
    async fn check_site(&self, v: &SiteVerification, background: bool) -> Result<verify::Checked> {
        let kind = verify::Kind::parse(&v.kind).ok_or_else(|| ControlError::Internal(anyhow::anyhow!("verification [{}] has kind [{}]", v.site, v.kind)))?;
        let floor = crawler::unverified_floor(|k| setting(self, k));
        let blocked = crawler::blocked_hosts_of(|k| setting(self, k));
        let checked = verify::check(&egress(self), &blocked, kind, &v.site, &v.token, floor, background).await;
        let now = crate::meter::now_ms();
        match &checked {
            verify::Checked::Proven => self.store.record_check(&v.cluster_id, &v.site, Ok(()), now).await?,
            verify::Checked::Failed(why) => self.store.record_check(&v.cluster_id, &v.site, Err(why), now).await?,
            verify::Checked::NotChecked(_) => {}
        }
        Ok(checked)
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
    /// each is claimed in the catalogue just before it is checked, so on a fleet one node checks
    /// it; one that couldn't be made (a busy site) is tried again in [`DEFER_FOR`], not counted
    /// as a failure until the last check is [`DEFER_UNTIL`] old. Nothing is checked while
    /// crawlers are off on the node.
    pub(crate) fn start_site_rechecks(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RECHECK_EVERY).await;
                let Some(c) = me.upgrade() else { return };
                if crawlers_on(&c).is_err() {
                    continue;
                }
                let due = match c.store.due_rechecks(crate::meter::now_ms(), RECHECK_BATCH).await {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(error = %e, "listing site verifications to re-check failed");
                        continue;
                    }
                };
                for v in due {
                    let now = crate::meter::now_ms();
                    match c.store.claim_recheck(&v.cluster_id, &v.site, now).await {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(e) => {
                            tracing::warn!(site = v.site, error = %e, "claiming a site verification's re-check failed");
                            continue;
                        }
                    }
                    let outcome = match c.check_site(&v, true).await {
                        // Deferred, but not forever: once the last check is a day past its
                        // interval, a check that can't be made counts as failed.
                        Ok(verify::Checked::NotChecked(why)) => {
                            let now = crate::meter::now_ms();
                            match now - v.checked_at.unwrap_or(v.created_at) > DEFER_UNTIL {
                                true => c.store.record_check(&v.cluster_id, &v.site, Err(&why), now).await.map_err(ControlError::from),
                                false => c.store.defer_check(&v.cluster_id, &v.site, now + DEFER_FOR).await.map_err(ControlError::from),
                            }
                        }
                        Ok(_) => Ok(()),
                        Err(e) => Err(e),
                    };
                    if let Err(e) = outcome {
                        tracing::warn!(site = v.site, error = %e, "re-checking a site verification failed");
                    }
                }
            }
        });
    }
}
