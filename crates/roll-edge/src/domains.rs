//! Certificates for custom domains on a node alone (docs/apps/README.md, "Networking"): one per
//! hostname over ACME HTTP-01 ([`crate::acme::Solver::Http01`]), for exactly the hostnames a
//! [`CustomDomains`] says are active. rolld adapts roll apps to that trait, so neither crate
//! knows the other.
//!
//! Each certificate is stored sealed in object storage (`system/certs/domains/<host>.sealed`),
//! installed in the [`CertStore`] (SNI picks it), renewed [`RENEW_BEFORE`] it expires, and
//! uninstalled and deleted when the domain goes. Orders are bounded per pass, and a hostname whose
//! order failed waits, twice as long after each failure up to a cap, before it's tried again.

use crate::acme::{Issuer, short};
use crate::certs::{RENEW_BEFORE, SINGLETON, Singleton};
use crate::seal::Sealer;
use crate::tls::{CertPem, CertStore, Parsed};
use futures::future::BoxFuture;
use pg_storage::Objects;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// The custom domains to hold certificates for, and where their state goes.
pub trait CustomDomains: Send + Sync + 'static {
    /// The hostnames that are active now (verified and routed).
    fn active(&self) -> BoxFuture<'_, anyhow::Result<Vec<String>>>;
    /// A hostname's certificate was issued (or renewed), or its order failed.
    fn report<'a>(&'a self, host: &'a str, state: &'a CertState) -> BoxFuture<'a, ()>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CertState {
    /// Unix seconds.
    Issued { not_after: i64 },
    Failed(String),
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Orders started in one pass; the rest wait for the next.
    pub orders_per_sync: usize,
    /// The wait after a hostname's first failed order, doubled after each further one…
    pub backoff: Duration,
    /// …up to this.
    pub backoff_max: Duration,
    /// Active hostnames considered in one pass.
    pub max_domains: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { orders_per_sync: 5, backoff: Duration::from_secs(300), backoff_max: Duration::from_secs(24 * 3600), max_domains: 10_000 }
    }
}

/// Where a custom domain's certificate is stored.
pub const PREFIX: &str = "system/certs/domains/";

pub fn object_key(host: &str) -> String {
    format!("{PREFIX}{host}.sealed")
}

/// Whether the edge may hold a certificate for `host`: lowercase DNS labels, at least two, and
/// not under one of `refuse` (roll's own zones, whose names its wildcards serve).
pub fn acceptable(host: &str, refuse: &[String]) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    host.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| (1..=63).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !l.starts_with('-') && !l.ends_with('-'))
        && !labels.last().is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()))
        && !refuse.iter().any(|z| host == z || host.ends_with(&format!(".{z}")))
}

fn now_s() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

struct Failing {
    failures: u32,
    retry_at: Instant,
}

pub struct DomainCerts {
    objects: Objects,
    sealer: Sealer,
    store: Arc<CertStore>,
    issuer: Arc<dyn Issuer>,
    singleton: Arc<dyn Singleton>,
    domains: Arc<dyn CustomDomains>,
    refuse: Vec<String>,
    limits: Limits,
    /// The installed certificate's expiry, by hostname.
    held: Mutex<HashMap<String, i64>>,
    failing: Mutex<HashMap<String, Failing>>,
}

impl DomainCerts {
    /// `refuse`: roll's own zones (`roll.sh`, `roll.site`), never ordered for whatever
    /// `domains` says.
    #[allow(clippy::too_many_arguments)]
    pub fn new(objects: Objects, sealer: Sealer, store: Arc<CertStore>, issuer: Arc<dyn Issuer>, singleton: Arc<dyn Singleton>, domains: Arc<dyn CustomDomains>, refuse: Vec<String>, limits: Limits) -> Self {
        let refuse = refuse.into_iter().map(|z| z.trim_matches('.').to_ascii_lowercase()).filter(|z| !z.is_empty()).collect();
        Self { objects, sealer, store, issuer, singleton, domains, refuse, limits, held: Mutex::default(), failing: Mutex::default() }
    }

    async fn active(&self) -> anyhow::Result<BTreeSet<String>> {
        let hosts = self.domains.active().await?;
        Ok(hosts.into_iter().map(|h| h.trim_end_matches('.').to_ascii_lowercase()).filter(|h| acceptable(h, &self.refuse)).take(self.limits.max_domains).collect())
    }

    async fn load(&self, host: &str) -> anyhow::Result<Option<Parsed>> {
        let key = object_key(host);
        let Some(sealed) = self.objects.get(&key).await? else { return Ok(None) };
        let pem: CertPem = serde_json::from_slice(&self.sealer.open(&key, &sealed)?)?;
        Ok(Some(pem.parse()?))
    }

    fn install(&self, host: &str, parsed: &Parsed) {
        self.store.install(parsed);
        if self.held.lock().unwrap().insert(host.to_string(), parsed.not_after) != Some(parsed.not_after) {
            info!(host, serial = %parsed.serial, not_after = parsed.not_after, "edge: custom domain certificate installed");
        }
    }

    fn due(&self, host: &str) -> bool {
        self.held.lock().unwrap().get(host).is_none_or(|not_after| not_after - now_s() < RENEW_BEFORE.as_secs() as i64)
    }

    fn waiting(&self, host: &str) -> bool {
        self.failing.lock().unwrap().get(host).is_some_and(|f| Instant::now() < f.retry_at)
    }

    /// One pass: drop what's no longer active, install what's stored, and (holding the
    /// singleton) order what's missing or expiring, at most [`Limits::orders_per_sync`].
    pub async fn sync_once(&self) -> anyhow::Result<()> {
        let active = self.active().await?;
        let holds = self.singleton.holds(SINGLETON).await;
        let gone: Vec<String> = self.held.lock().unwrap().keys().filter(|h| !active.contains(*h)).cloned().collect();
        for host in gone {
            self.store.remove(&host);
            self.held.lock().unwrap().remove(&host);
            info!(host, "edge: custom domain certificate dropped (the domain is gone)");
        }
        self.failing.lock().unwrap().retain(|h, _| active.contains(h));
        if holds {
            for (key, _) in self.objects.list(PREFIX).await? {
                let host = key.strip_prefix(PREFIX).and_then(|k| k.strip_suffix(".sealed")).unwrap_or_default();
                if !active.contains(host) {
                    self.objects.delete(&key).await?;
                }
            }
        }
        let mut orders = 0;
        for host in &active {
            if !self.due(host) {
                continue;
            }
            // Issued by this node before a restart, or by the node that held the singleton.
            match self.load(host).await {
                Ok(Some(p)) if p.covers(std::slice::from_ref(host)) => self.install(host, &p),
                Ok(_) => {}
                Err(e) => warn!(host, error = %e, "edge: a stored custom domain certificate doesn't load"),
            }
            if !self.due(host) || !holds || self.waiting(host) || orders >= self.limits.orders_per_sync {
                continue;
            }
            // Orders take a while: only for a hostname that's still active now.
            if !self.active().await?.contains(host) {
                continue;
            }
            orders += 1;
            let state = match self.order(host).await {
                Ok(not_after) => {
                    self.failing.lock().unwrap().remove(host);
                    CertState::Issued { not_after }
                }
                Err(e) => {
                    let mut failing = self.failing.lock().unwrap();
                    let f = failing.entry(host.clone()).or_insert(Failing { failures: 0, retry_at: Instant::now() });
                    f.failures = f.failures.saturating_add(1);
                    let wait = self.limits.backoff.saturating_mul(1 << (f.failures - 1).min(20)).min(self.limits.backoff_max);
                    f.retry_at = Instant::now() + wait;
                    warn!(host, error = %e, failures = f.failures, retry_in_s = wait.as_secs(), "edge: custom domain certificate order failed");
                    CertState::Failed(short(&format!("{e:#}"), 300))
                }
            };
            self.domains.report(host, &state).await;
        }
        Ok(())
    }

    async fn order(&self, host: &str) -> anyhow::Result<i64> {
        info!(host, "edge: ordering a custom domain certificate (HTTP-01)");
        let pem = self.issuer.issue(&[host.to_string()]).await?;
        let parsed = pem.parse()?;
        anyhow::ensure!(parsed.covers(&[host.to_string()]), "the CA's certificate doesn't cover {host}");
        let key = object_key(host);
        self.objects.put(&key, self.sealer.seal(&key, &serde_json::to_vec(&pem)?).into()).await?;
        self.install(host, &parsed);
        Ok(parsed.not_after)
    }

    /// Sync every `every` forever (a failed pass is retried on the next one).
    pub async fn run(self: Arc<Self>, every: Duration) {
        loop {
            if let Err(e) = self.sync_once().await {
                warn!(error = %e, "edge: custom domain certificates sync failed");
            }
            tokio::time::sleep(every).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::certs::LocalSingleton;
    use pg_storage::StorageConfig;

    /// Self-signed certificates, failing for names in `fail`.
    #[derive(Default)]
    struct FakeCa {
        orders: Mutex<Vec<String>>,
        fail: Mutex<Vec<String>>,
    }

    impl Issuer for FakeCa {
        fn issue<'a>(&'a self, names: &'a [String]) -> BoxFuture<'a, anyhow::Result<CertPem>> {
            Box::pin(async move {
                self.orders.lock().unwrap().push(names.join(","));
                anyhow::ensure!(!self.fail.lock().unwrap().contains(&names[0]), "urn:ietf:params:acme:error:connection");
                crate::tls::self_signed(names)
            })
        }
    }

    #[derive(Default)]
    struct Domains {
        active: Mutex<Vec<String>>,
        reports: Mutex<Vec<(String, CertState)>>,
    }

    impl CustomDomains for Domains {
        fn active(&self) -> BoxFuture<'_, anyhow::Result<Vec<String>>> {
            Box::pin(async move { Ok(self.active.lock().unwrap().clone()) })
        }
        fn report<'a>(&'a self, host: &'a str, state: &'a CertState) -> BoxFuture<'a, ()> {
            Box::pin(async move { self.reports.lock().unwrap().push((host.to_string(), state.clone())) })
        }
    }

    /// Only active hostnames are ordered for, at most `orders_per_sync` a pass; a failure backs
    /// off; a removed domain's certificate is uninstalled and deleted.
    #[tokio::test]
    async fn orders_are_bounded_backed_off_and_follow_the_active_domains() {
        let dir = tempfile::tempdir().unwrap();
        let objects = Objects::new(&StorageConfig::Local(dir.path().to_path_buf())).unwrap();
        let sealer = Sealer::new("0123456789abcdef0123456789abcdef").unwrap();
        let (ca, domains, store) = (Arc::new(FakeCa::default()), Arc::new(Domains::default()), CertStore::new());
        *domains.active.lock().unwrap() = ["a.example.com", "b.example.com", "c.example.com", "x.roll.test", "bad_name.example.com"].map(String::from).to_vec();
        ca.fail.lock().unwrap().push("c.example.com".into());
        let limits = Limits { orders_per_sync: 2, ..Limits::default() };
        let certs = DomainCerts::new(objects.clone(), sealer, store.clone(), ca.clone(), Arc::new(LocalSingleton), domains.clone(), vec!["roll.test".into()], limits);

        certs.sync_once().await.unwrap();
        assert_eq!(*ca.orders.lock().unwrap(), ["a.example.com", "b.example.com"], "two a pass, never roll's own or a malformed name");
        certs.sync_once().await.unwrap();
        certs.sync_once().await.unwrap();
        assert_eq!(ca.orders.lock().unwrap().len(), 3, "the failed one waits: {:?}", ca.orders.lock().unwrap());
        assert!(store.lookup(Some("a.example.com")).is_some() && store.lookup(Some("c.example.com")).is_none());
        let reports = domains.reports.lock().unwrap().clone();
        assert!(matches!(reports.last(), Some((h, CertState::Failed(e))) if h == "c.example.com" && e.contains("connection")), "{reports:?}");

        domains.active.lock().unwrap().retain(|h| h != "a.example.com");
        certs.sync_once().await.unwrap();
        assert!(store.lookup(Some("a.example.com")).is_none(), "uninstalled");
        assert!(!objects.exists(&object_key("a.example.com")).await.unwrap() && objects.exists(&object_key("b.example.com")).await.unwrap());
    }
}
