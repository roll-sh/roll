//! Certificates from an ACME CA (Let's Encrypt) over DNS-01: every name in an order gets an
//! `_acme-challenge` TXT record through a [`DnsProvider`] (Cloudflare's API in the cloud), which
//! is removed again once the order is done, whether it worked or not.
//!
//! The ACME account is created once and kept sealed in object storage, one per directory URL.

use crate::seal::Sealer;
use crate::tls::CertPem;
use anyhow::Context as _;
use futures::future::BoxFuture;
use instant_acme::{Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus, RetryPolicy};
use pg_storage::Objects;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tracing::{info, warn};

/// Publishes and removes DNS TXT records.
pub trait DnsProvider: Send + Sync + 'static {
    /// Create a TXT record `name` (no trailing dot) with `value`; returns the id to delete it by.
    fn create_txt<'a>(&'a self, name: &'a str, value: &'a str) -> BoxFuture<'a, anyhow::Result<String>>;
    fn delete_txt<'a>(&'a self, name: &'a str, id: &'a str) -> BoxFuture<'a, anyhow::Result<()>>;
}

/// Cloudflare's DNS API with a token that may edit DNS in the zones (`ROLL_CLOUDFLARE_API_TOKEN`).
/// The zone of a name is found by asking for its parent domains, longest first.
pub struct Cloudflare {
    api: String,
    token: String,
    http: reqwest::Client,
    /// Zone name → id.
    zones: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for Cloudflare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cloudflare").field("api", &self.api).finish_non_exhaustive()
    }
}

#[derive(serde::Deserialize)]
struct CfResponse<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<serde_json::Value>,
    result: Option<T>,
}

#[derive(serde::Deserialize)]
struct CfId {
    id: String,
}

impl Cloudflare {
    pub const API: &'static str = "https://api.cloudflare.com/client/v4";

    /// `api`: [`Cloudflare::API`], or a stand-in (tests).
    pub fn new(token: &str, api: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(!token.trim().is_empty(), "ROLL_CLOUDFLARE_API_TOKEN is empty");
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none()).build()?;
        Ok(Self { api: api.trim_end_matches('/').to_string(), token: token.trim().to_string(), http, zones: Mutex::default() })
    }

    async fn call<T: serde::de::DeserializeOwned>(&self, req: reqwest::RequestBuilder, what: &str) -> anyhow::Result<T> {
        let resp = req.bearer_auth(&self.token).send().await.with_context(|| format!("Cloudflare: {what}"))?;
        let status = resp.status();
        let body: CfResponse<T> = resp.json().await.with_context(|| format!("Cloudflare: {what}: HTTP {status}"))?;
        match body.result {
            Some(r) if body.success => Ok(r),
            _ => anyhow::bail!("Cloudflare: {what}: HTTP {status}: {}", serde_json::Value::from(body.errors)),
        }
    }

    async fn zone_for(&self, name: &str) -> anyhow::Result<String> {
        let labels: Vec<&str> = name.split('.').collect();
        let candidates: Vec<String> = (1..labels.len().saturating_sub(1)).map(|i| labels[i..].join(".")).collect();
        if let Some(id) = candidates.iter().find_map(|c| self.zones.lock().unwrap().get(c).cloned()) {
            return Ok(id);
        }
        for zone in &candidates {
            // Zone names are DNS names: nothing in them needs escaping.
            let url = format!("{}/zones?name={zone}", self.api);
            let found: Vec<CfId> = self.call(self.http.get(url), &format!("find zone {zone}")).await?;
            if let Some(z) = found.into_iter().next() {
                self.zones.lock().unwrap().insert(zone.clone(), z.id.clone());
                return Ok(z.id);
            }
        }
        anyhow::bail!("Cloudflare: no zone for {name} (does the token see it?)")
    }
}

impl DnsProvider for Cloudflare {
    fn create_txt<'a>(&'a self, name: &'a str, value: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            let zone = self.zone_for(name).await?;
            let url = format!("{}/zones/{zone}/dns_records", self.api);
            let body = serde_json::json!({ "type": "TXT", "name": name, "content": value, "ttl": 60, "comment": "roll edge ACME challenge" });
            let rec: CfId = self.call(self.http.post(url).json(&body), &format!("create TXT {name}")).await?;
            Ok(format!("{zone}/{}", rec.id))
        })
    }

    fn delete_txt<'a>(&'a self, name: &'a str, id: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let (zone, rec) = id.split_once('/').context("bad record id")?;
            let url = format!("{}/zones/{zone}/dns_records/{rec}", self.api);
            let _: CfId = self.call(self.http.delete(url), &format!("delete TXT {name}")).await?;
            Ok(())
        })
    }
}

/// Where and how to order certificates.
#[derive(Clone, Debug)]
pub struct AcmeConfig {
    /// The directory URL (`ROLL_ACME_DIRECTORY`): Let's Encrypt's production one in the cloud.
    pub directory: String,
    /// The account's contact (`ROLL_ACME_EMAIL`).
    pub email: Option<String>,
    /// A PEM root the CA's own HTTPS chains to, instead of the system's (Pebble's test CA).
    pub ca_file: Option<PathBuf>,
    /// How long to wait after publishing the TXT records before asking the CA to check them.
    pub propagation: Duration,
    /// How long an order may take, from ready to the certificate.
    pub timeout: Duration,
}

impl AcmeConfig {
    pub const LETS_ENCRYPT: &'static str = "https://acme-v02.api.letsencrypt.org/directory";

    pub fn new(directory: &str) -> Self {
        Self { directory: directory.to_string(), email: None, ca_file: None, propagation: Duration::from_secs(20), timeout: Duration::from_secs(180) }
    }
}

/// Issues certificates.
pub trait Issuer: Send + Sync + 'static {
    fn issue<'a>(&'a self, names: &'a [String]) -> BoxFuture<'a, anyhow::Result<CertPem>>;
}

/// An ACME client with a DNS-01 solver.
pub struct Acme {
    cfg: AcmeConfig,
    dns: std::sync::Arc<dyn DnsProvider>,
    objects: Objects,
    sealer: Sealer,
    account: tokio::sync::Mutex<Option<Account>>,
}

impl Acme {
    pub fn new(cfg: AcmeConfig, dns: std::sync::Arc<dyn DnsProvider>, objects: Objects, sealer: Sealer) -> Self {
        Self { cfg, dns, objects, sealer, account: Default::default() }
    }

    fn account_key(&self) -> String {
        let h = Sha256::digest(self.cfg.directory.as_bytes());
        format!("system/certs/acme-account-{}.sealed", hex::encode(&h[..8]))
    }

    fn builder(&self) -> anyhow::Result<instant_acme::AccountBuilder> {
        crate::install_crypto_provider();
        Ok(match &self.cfg.ca_file {
            Some(f) => Account::builder_with_root(f)?,
            None => Account::builder()?,
        })
    }

    async fn account(&self) -> anyhow::Result<Account> {
        let mut slot = self.account.lock().await;
        if let Some(a) = slot.as_ref() {
            return Ok(a.clone());
        }
        let key = self.account_key();
        let account = match self.objects.get(&key).await? {
            Some(sealed) => {
                let creds: AccountCredentials = serde_json::from_slice(&self.sealer.open(&key, &sealed)?)?;
                self.builder()?.from_credentials(creds).await?
            }
            None => {
                let contact = self.cfg.email.iter().map(|e| format!("mailto:{e}")).collect::<Vec<_>>();
                let contact: Vec<&str> = contact.iter().map(String::as_str).collect();
                let new = NewAccount { contact: &contact, terms_of_service_agreed: true, only_return_existing: false };
                let (account, creds) = self.builder()?.create(&new, self.cfg.directory.clone(), None).await?;
                self.objects.put(&key, self.sealer.seal(&key, &serde_json::to_vec(&creds)?).into()).await?;
                info!(directory = %self.cfg.directory, "acme: account created");
                account
            }
        };
        *slot = Some(account.clone());
        Ok(account)
    }

    async fn order(&self, names: &[String], records: &mut Vec<(String, String)>) -> anyhow::Result<CertPem> {
        let account = self.account().await?;
        let ids: Vec<Identifier> = names.iter().map(|n| Identifier::Dns(n.clone())).collect();
        let mut order = account.new_order(&NewOrder::new(&ids)).await?;
        // Publish every record first, wait once, then tell the CA.
        let mut authzs = order.authorizations();
        while let Some(a) = authzs.next().await {
            let mut a = a?;
            match a.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                s => anyhow::bail!("acme: authorization is {s:?}"),
            }
            let ch = a.challenge(ChallengeType::Dns01).context("acme: the CA offers no dns-01 challenge")?;
            let Identifier::Dns(domain) = ch.identifier().identifier else { anyhow::bail!("acme: not a DNS identifier") };
            let name = format!("_acme-challenge.{domain}");
            let id = self.dns.create_txt(&name, &ch.key_authorization().dns_value()).await?;
            records.push((name, id));
        }
        tokio::time::sleep(self.cfg.propagation).await;
        let mut authzs = order.authorizations();
        while let Some(a) = authzs.next().await {
            let mut a = a?;
            if a.status != AuthorizationStatus::Pending {
                continue;
            }
            a.challenge(ChallengeType::Dns01).context("acme: dns-01 challenge gone")?.set_ready().await?;
        }
        let retry = RetryPolicy::new().initial_delay(Duration::from_millis(500)).timeout(self.cfg.timeout);
        let status = order.poll_ready(&retry).await?;
        anyhow::ensure!(status == OrderStatus::Ready, "acme: order is {status:?}, not ready");
        let key = order.finalize().await?;
        let chain = order.poll_certificate(&retry).await?;
        Ok(CertPem { chain, key })
    }
}

impl Issuer for Acme {
    fn issue<'a>(&'a self, names: &'a [String]) -> BoxFuture<'a, anyhow::Result<CertPem>> {
        Box::pin(async move {
            let mut records = vec![];
            let result = self.order(names, &mut records).await;
            for (name, id) in records {
                if let Err(e) = self.dns.delete_txt(&name, &id).await {
                    warn!(%name, error = %e, "acme: challenge record left behind");
                }
            }
            result
        })
    }
}
