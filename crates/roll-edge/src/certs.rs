//! The fleet's certificates: stored sealed in object storage (`system/certs/<set>.sealed`), polled
//! by every node and installed in its [`CertStore`] when they change, and issued or renewed (30
//! days before they expire) by whichever node holds the `acme` singleton.

use crate::acme::Issuer;
use crate::hosts::CertSet;
use crate::seal::Sealer;
use crate::tls::{CertPem, CertStore, Parsed};
use futures::future::BoxFuture;
use pg_storage::Objects;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// One node at a time for fleet-wide jobs. The cluster layer's `singleton(name)` implements it
/// for the fleet; [`LocalSingleton`] is a single node, which always holds it.
pub trait Singleton: Send + Sync + 'static {
    /// Whether this node holds `name` now.
    fn holds<'a>(&'a self, name: &'a str) -> BoxFuture<'a, bool>;
}

/// A node on its own.
pub struct LocalSingleton;

impl Singleton for LocalSingleton {
    fn holds<'a>(&'a self, _name: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(std::future::ready(true))
    }
}

/// The singleton's name.
pub const SINGLETON: &str = "acme";

/// Renew this long before a certificate expires.
pub const RENEW_BEFORE: Duration = Duration::from_secs(30 * 86_400);

pub fn object_key(set: &str) -> String {
    format!("system/certs/{set}.sealed")
}

fn now_s() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

pub struct CertManager {
    objects: Objects,
    sealer: Sealer,
    store: Arc<CertStore>,
    sets: Vec<CertSet>,
    issuer: Option<Arc<dyn Issuer>>,
    singleton: Arc<dyn Singleton>,
    /// The serial installed per set.
    installed: Mutex<HashMap<String, String>>,
    /// After a failed order, not before this (the CA's rate limits count failures).
    retry_at: Mutex<HashMap<String, std::time::Instant>>,
}

/// How long to wait after a failed order.
pub const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(3600);

impl CertManager {
    /// `issuer`: none on a node that only ever loads what others issued.
    pub fn new(objects: Objects, sealer: Sealer, store: Arc<CertStore>, sets: Vec<CertSet>, issuer: Option<Arc<dyn Issuer>>, singleton: Arc<dyn Singleton>) -> Self {
        Self { objects, sealer, store, sets, issuer, singleton, installed: Mutex::default(), retry_at: Mutex::default() }
    }

    /// The stored certificate of `set`, if there is one that opens and parses.
    async fn load(&self, set: &str) -> anyhow::Result<Option<(CertPem, Parsed)>> {
        let key = object_key(set);
        let Some(sealed) = self.objects.get(&key).await? else { return Ok(None) };
        let pem: CertPem = serde_json::from_slice(&self.sealer.open(&key, &sealed)?)?;
        let parsed = pem.parse()?;
        Ok(Some((pem, parsed)))
    }

    fn install(&self, set: &str, parsed: &Parsed) {
        let mut installed = self.installed.lock().unwrap();
        if installed.get(set) != Some(&parsed.serial) {
            self.store.install(parsed);
            installed.insert(set.to_string(), parsed.serial.clone());
            info!(set, serial = %parsed.serial, not_after = parsed.not_after, "edge: certificate installed");
        }
    }

    /// One pass: install what's stored, and (holding the singleton, with an issuer) issue
    /// what's missing, doesn't cover its set or expires within [`RENEW_BEFORE`].
    pub async fn sync_once(&self) -> anyhow::Result<()> {
        let mut first_error = None;
        for set in &self.sets {
            if let Err(e) = self.sync_set(set).await {
                warn!(set = %set.name, error = %e, "edge: certificate sync failed");
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn sync_set(&self, set: &CertSet) -> anyhow::Result<()> {
        let stored = self.load(&set.name).await?;
        if let Some((_, p)) = &stored {
            self.install(&set.name, p);
        }
        let due = match &stored {
            Some((_, p)) => !p.covers(&set.names) || p.not_after - now_s() < RENEW_BEFORE.as_secs() as i64,
            None => true,
        };
        let Some(issuer) = &self.issuer else { return Ok(()) };
        let waiting = self.retry_at.lock().unwrap().get(&set.name).is_some_and(|t| std::time::Instant::now() < *t);
        if !due || waiting || !self.singleton.holds(SINGLETON).await {
            return Ok(());
        }
        info!(set = %set.name, names = ?set.names, "edge: ordering a certificate");
        let pem = match issuer.issue(&set.names).await {
            Ok(p) => p,
            Err(e) => {
                self.retry_at.lock().unwrap().insert(set.name.clone(), std::time::Instant::now() + RETRY_AFTER_FAILURE);
                return Err(e);
            }
        };
        let parsed = pem.parse()?;
        let key = object_key(&set.name);
        self.objects.put(&key, self.sealer.seal(&key, &serde_json::to_vec(&pem)?).into()).await?;
        self.install(&set.name, &parsed);
        Ok(())
    }

    /// Sync every `every` forever (a failed pass is retried on the next one).
    pub async fn run(self: Arc<Self>, every: Duration) {
        loop {
            let _ = self.sync_once().await;
            tokio::time::sleep(every).await;
        }
    }
}
