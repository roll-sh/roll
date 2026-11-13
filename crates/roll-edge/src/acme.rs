//! Certificates from an ACME CA (Let's Encrypt), proven by a [`Solver`]:
//!
//! - DNS-01 (the edge's own wildcards): every name in an order gets an `_acme-challenge` TXT
//!   record through a [`DnsProvider`] (Cloudflare's API in the cloud).
//! - HTTP-01 (custom domains on a node alone): the key authorization goes into [`Http01`], which
//!   the plain-HTTP listener answers `/.well-known/acme-challenge/<token>` from.
//!
//! Either way the proof is taken down again once the order is done, whether it worked or not.
//! The ACME account ([`AcmeAccount`]) is created once and kept sealed in object storage, one per
//! directory URL, and shared by every solver.

use crate::seal::Sealer;
use crate::tls::CertPem;
use anyhow::Context as _;
use futures::future::BoxFuture;
use instant_acme::{Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus, RetryPolicy};
use pg_storage::Objects;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, warn};

/// Publishes and removes DNS TXT records.
pub trait DnsProvider: Send + Sync + 'static {
    /// Create a TXT record `name` (no trailing dot) with `value`; returns the id to delete it by.
    fn create_txt<'a>(&'a self, name: &'a str, value: &'a str) -> BoxFuture<'a, anyhow::Result<String>>;
    fn delete_txt<'a>(&'a self, name: &'a str, id: &'a str) -> BoxFuture<'a, anyhow::Result<()>>;
}

/// Cloudflare's API with a token that may edit DNS in the zones (`ROLL_CLOUDFLARE_API_TOKEN`),
/// and custom hostnames on the site zone ([`crate::saas`]). The zone of a name is found by asking
/// for its parent domains, longest first.
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

    /// The most of an answer read: Cloudflare's are a few KiB.
    pub const MAX_RESPONSE: usize = 1 << 20;

    /// `api`: [`Cloudflare::API`], or a stand-in (tests).
    pub fn new(token: &str, api: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(!token.trim().is_empty(), "ROLL_CLOUDFLARE_API_TOKEN is empty");
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none()).build()?;
        Ok(Self { api: api.trim_end_matches('/').to_string(), token: token.trim().to_string(), http, zones: Mutex::default() })
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.api)
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Send `req` with the token and read its `result`, reading at most [`Self::MAX_RESPONSE`]
    /// bytes of the answer. An error carries at most the first three of Cloudflare's, shortened.
    pub(crate) async fn call<T: serde::de::DeserializeOwned>(&self, req: reqwest::RequestBuilder, what: &str) -> anyhow::Result<T> {
        let resp = req.bearer_auth(&self.token).send().await.with_context(|| format!("Cloudflare: {what}"))?;
        let status = resp.status();
        let bytes = read_bounded(resp, Self::MAX_RESPONSE).await.with_context(|| format!("Cloudflare: {what}: HTTP {status}"))?;
        let body: CfResponse<T> = serde_json::from_slice(&bytes).with_context(|| format!("Cloudflare: {what}: HTTP {status}"))?;
        match body.result {
            Some(r) if body.success => Ok(r),
            _ => {
                let errors: Vec<String> = body.errors.iter().take(3).map(|e| short(&e.to_string(), 200)).collect();
                anyhow::bail!("Cloudflare: {what}: HTTP {status}: [{}]", errors.join(", "))
            }
        }
    }

    /// The id of the zone named exactly `zone`, if the token sees one.
    pub async fn zone_id(&self, zone: &str) -> anyhow::Result<Option<String>> {
        if let Some(id) = self.zones.lock().unwrap().get(zone).cloned() {
            return Ok(Some(id));
        }
        // Zone names are DNS names: nothing in them needs escaping.
        let url = format!("{}/zones?name={zone}", self.api);
        let found: Vec<CfId> = self.call(self.http.get(url), &format!("find zone {zone}")).await?;
        let Some(z) = found.into_iter().next() else { return Ok(None) };
        self.zones.lock().unwrap().insert(zone.to_string(), z.id.clone());
        Ok(Some(z.id))
    }

    async fn zone_for(&self, name: &str) -> anyhow::Result<String> {
        let labels: Vec<&str> = name.split('.').collect();
        for i in 1..labels.len().saturating_sub(1) {
            if let Some(id) = self.zone_id(&labels[i..].join(".")).await? {
                return Ok(id);
            }
        }
        anyhow::bail!("Cloudflare: no zone for {name} (does the token see it?)")
    }
}

/// A response's body, refusing one longer than `max` whatever its `Content-Length` says.
pub(crate) async fn read_bounded(mut resp: reqwest::Response, max: usize) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(resp.content_length().is_none_or(|n| n <= max as u64), "the answer is over {max} bytes");
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        anyhow::ensure!(out.len() + chunk.len() <= max, "the answer is over {max} bytes");
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// `s`, cut to at most `max` characters.
pub(crate) fn short(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
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

/// The HTTP-01 key authorizations of the orders in flight, by token: what the plain-HTTP
/// listener ([`crate::http::serve_plain`]) answers `GET /.well-known/acme-challenge/<token>` with.
#[derive(Default)]
pub struct Http01 {
    tokens: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for Http01 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http01").field("tokens", &self.tokens.lock().unwrap().len()).finish()
    }
}

impl Http01 {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// The key authorization for `token`, if an order is waiting on it.
    pub fn answer(&self, token: &str) -> Option<String> {
        self.tokens.lock().unwrap().get(token).cloned()
    }

    fn put(&self, token: &str, key_authorization: &str) {
        self.tokens.lock().unwrap().insert(token.to_string(), key_authorization.to_string());
    }

    fn remove(&self, token: &str) {
        self.tokens.lock().unwrap().remove(token);
    }
}

/// How an order's names are proven.
#[derive(Clone)]
pub enum Solver {
    /// A TXT record per name: wildcards work; the CA is asked once DNS has had time to spread.
    Dns01(Arc<dyn DnsProvider>),
    /// A token over plain HTTP on port 80 of the name: no wildcards, and the node that answers
    /// must be the one holding the token (a node alone).
    Http01(Arc<Http01>),
}

impl Solver {
    fn challenge_type(&self) -> ChallengeType {
        match self {
            Solver::Dns01(_) => ChallengeType::Dns01,
            Solver::Http01(_) => ChallengeType::Http01,
        }
    }
}

/// A proof to take down once its order is done.
enum Published {
    Txt { name: String, id: String },
    Token(String),
}

/// The ACME account for one directory, kept sealed in object storage and shared by every
/// [`Acme`] that orders from that directory.
pub struct AcmeAccount {
    cfg: AcmeConfig,
    objects: Objects,
    sealer: Sealer,
    account: tokio::sync::Mutex<Option<Account>>,
}

impl AcmeAccount {
    pub fn new(cfg: AcmeConfig, objects: Objects, sealer: Sealer) -> Arc<Self> {
        Arc::new(Self { cfg, objects, sealer, account: Default::default() })
    }

    fn key(&self) -> String {
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

    async fn get(&self) -> anyhow::Result<Account> {
        let mut slot = self.account.lock().await;
        if let Some(a) = slot.as_ref() {
            return Ok(a.clone());
        }
        let key = self.key();
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
}

/// An ACME client with one [`Solver`].
pub struct Acme {
    account: Arc<AcmeAccount>,
    solver: Solver,
}

impl Acme {
    pub fn new(account: Arc<AcmeAccount>, solver: Solver) -> Self {
        Self { account, solver }
    }

    async fn publish(&self, ch: &instant_acme::ChallengeHandle<'_>, published: &mut Vec<Published>) -> anyhow::Result<()> {
        let Identifier::Dns(domain) = ch.identifier().identifier else { anyhow::bail!("acme: not a DNS identifier") };
        match &self.solver {
            Solver::Dns01(dns) => {
                let name = format!("_acme-challenge.{domain}");
                let id = dns.create_txt(&name, &ch.key_authorization().dns_value()).await?;
                published.push(Published::Txt { name, id });
            }
            Solver::Http01(tokens) => {
                tokens.put(&ch.token, ch.key_authorization().as_str());
                published.push(Published::Token(ch.token.clone()));
            }
        }
        Ok(())
    }

    async fn order(&self, names: &[String], published: &mut Vec<Published>) -> anyhow::Result<CertPem> {
        let cfg = &self.account.cfg;
        let account = self.account.get().await?;
        let kind = self.solver.challenge_type();
        let ids: Vec<Identifier> = names.iter().map(|n| Identifier::Dns(n.clone())).collect();
        let mut order = account.new_order(&NewOrder::new(&ids)).await?;
        // Publish every proof first, wait once (DNS), then tell the CA.
        let mut authzs = order.authorizations();
        while let Some(a) = authzs.next().await {
            let mut a = a?;
            match a.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                s => anyhow::bail!("acme: authorization is {s:?}"),
            }
            let ch = a.challenge(kind.clone()).with_context(|| format!("acme: the CA offers no {kind:?} challenge"))?;
            self.publish(&ch, published).await?;
        }
        if matches!(self.solver, Solver::Dns01(_)) {
            tokio::time::sleep(cfg.propagation).await;
        }
        let mut authzs = order.authorizations();
        while let Some(a) = authzs.next().await {
            let mut a = a?;
            if a.status != AuthorizationStatus::Pending {
                continue;
            }
            a.challenge(kind.clone()).with_context(|| format!("acme: {kind:?} challenge gone"))?.set_ready().await?;
        }
        let retry = RetryPolicy::new().initial_delay(Duration::from_millis(500)).timeout(cfg.timeout);
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
            let mut published = vec![];
            let result = self.order(names, &mut published).await;
            for p in published {
                match (p, &self.solver) {
                    (Published::Txt { name, id }, Solver::Dns01(dns)) => {
                        if let Err(e) = dns.delete_txt(&name, &id).await {
                            warn!(%name, error = %e, "acme: challenge record left behind");
                        }
                    }
                    (Published::Token(t), Solver::Http01(tokens)) => tokens.remove(&t),
                    _ => {}
                }
            }
            result
        })
    }
}
