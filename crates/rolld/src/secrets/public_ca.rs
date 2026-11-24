//! roll secrets' publicly trusted certificates (docs/secrets.md, "Publicly trusted
//! certificates"): the ACME client is the edge's (`roll_edge::acme::order`, dns-01 through
//! `roll_edge::acme::Cloudflare`), talking to the CA through a guarded client: public addresses
//! only (`roll_net::PublicOnly`, the literal-host check), no redirects, no proxy, a timeout and a
//! bounded read. A deployment that allows private addresses (`ROLL_CODE_WEBHOOK_ALLOW_PRIVATE`,
//! local development) reaches them, and `ROLL_SECRETS_ACME_CA_FILE` adds a root for a test CA's
//! own HTTPS (Pebble's).

use axum::http::{Request, Response};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use instant_acme::{Account, AccountCredentials, BodyWrapper, BytesResponse, ExternalAccountKey, HttpClient, NewAccount};
use roll_edge::acme::{Cloudflare, Solver, Timing};
use roll_secrets::Value;
use roll_secrets::public_certs::{DnsProvider, Order, Ordered, PUBLIC_CAS, PublicCa};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// The most of one answer from the CA read: directories, orders and certificate chains are a few
/// KiB.
const ANSWER_MAX: usize = 1 << 20;

pub(super) struct RollPublicCa {
    /// Directories besides [`PUBLIC_CAS`] (`ROLL_SECRETS_ACME_ALLOW`).
    allow: Vec<String>,
    allow_private: bool,
    ca_file: Option<PathBuf>,
    /// Cloudflare's API (`ROLL_CLOUDFLARE_API`: a stand-in in tests).
    cloudflare_api: String,
    timing: Timing,
}

impl RollPublicCa {
    pub(super) fn new(allow_private: bool, cloudflare_api: &str) -> anyhow::Result<RollPublicCa> {
        let allow = std::env::var("ROLL_SECRETS_ACME_ALLOW").unwrap_or_default().split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect::<Vec<_>>();
        for d in &allow {
            let u = url::Url::parse(d).map_err(|_| anyhow::anyhow!("ROLL_SECRETS_ACME_ALLOW: {d:?} isn't a directory URL"))?;
            anyhow::ensure!(u.scheme() == "https", "ROLL_SECRETS_ACME_ALLOW: {d} isn't HTTPS");
        }
        let ca_file = std::env::var("ROLL_SECRETS_ACME_CA_FILE").ok().filter(|v| !v.trim().is_empty()).map(PathBuf::from);
        let propagation = match std::env::var("ROLL_SECRETS_ACME_PROPAGATION_S") {
            Ok(s) => s.trim().parse().map_err(|_| anyhow::anyhow!("ROLL_SECRETS_ACME_PROPAGATION_S is a number of seconds, not {s:?}"))?,
            Err(_) => 20,
        };
        Ok(RollPublicCa { allow, allow_private, ca_file, cloudflare_api: cloudflare_api.to_string(), timing: Timing { propagation: Duration::from_secs(propagation), timeout: Duration::from_secs(180) } })
    }

    fn client(&self) -> Result<Guarded, String> {
        let mut b = reqwest::Client::builder().timeout(Duration::from_secs(30)).connect_timeout(Duration::from_secs(10)).redirect(reqwest::redirect::Policy::none()).user_agent("roll-secrets-acme");
        if let Some(f) = &self.ca_file {
            let pem = std::fs::read(f).map_err(|e| format!("ROLL_SECRETS_ACME_CA_FILE: {e}"))?;
            b = b.add_root_certificate(reqwest::Certificate::from_pem(&pem).map_err(|_| "ROLL_SECRETS_ACME_CA_FILE isn't a PEM certificate".to_string())?);
        }
        if !self.allow_private {
            b = b.dns_resolver(roll_net::PublicOnly).no_proxy();
        }
        Ok(Guarded { http: b.build().map_err(|e| e.to_string())?, allow_private: self.allow_private })
    }

    fn solver(&self, dns: DnsProvider, credential: &Value) -> Result<Arc<Cloudflare>, String> {
        match dns {
            DnsProvider::Cloudflare => Cloudflare::new(credential.expose(), &self.cloudflare_api).map(Arc::new).map_err(|e| e.to_string()),
        }
    }

    // The store calls from its blocking threads: wait for the CA there.
    fn block<T>(f: impl Future<Output = T>) -> T {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
    }

    async fn order_async(&self, o: Order<'_>) -> Result<Ordered, String> {
        roll_edge::install_crypto_provider();
        let http = Box::new(self.client()?);
        let builder = Account::builder_with_http(http);
        let (account, created) = match o.account {
            Some(creds) => {
                let creds: AccountCredentials = serde_json::from_str(creds.expose()).map_err(|_| "the project's ACME account isn't readable".to_string())?;
                (builder.from_credentials(creds).await.map_err(problem)?, None)
            }
            None => {
                let eab = match o.eab {
                    Some((kid, key)) => {
                        use base64::Engine as _;
                        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(key.expose().trim().trim_end_matches('=')).map_err(|_| "the EAB HMAC key isn't base64url".to_string())?;
                        Some(ExternalAccountKey::new(kid.to_string(), &raw))
                    }
                    None => None,
                };
                let new = NewAccount { contact: &[], terms_of_service_agreed: true, only_return_existing: false };
                let (account, creds) = builder.create(&new, o.directory.to_string(), eab.as_ref()).await.map_err(problem)?;
                (account, Some(Value::new(serde_json::to_string(&creds).map_err(|e| e.to_string())?)))
            }
        };
        let solver = Solver::Dns01(self.solver(o.dns, o.credential)?);
        let cert = roll_edge::acme::order(&account, &solver, o.names, self.timing).await.map_err(|e| format!("{e:#}"))?;
        Ok(Ordered { chain: Value::new(cert.chain), private_key: Value::new(cert.key), account: created })
    }
}

/// The CA's problem, by its type and detail (never a request, which would carry a nonce or a key).
fn problem(e: instant_acme::Error) -> String {
    match e {
        instant_acme::Error::Api(p) => format!("the CA refused: {} {}", p.r#type.unwrap_or_default().trim_start_matches("urn:ietf:params:acme:error:"), p.detail.unwrap_or_default()),
        e => format!("ACME: {e}"),
    }
}

impl PublicCa for RollPublicCa {
    fn allows(&self, directory: &str) -> bool {
        PUBLIC_CAS.contains(&directory) || self.allow.iter().any(|d| d == directory)
    }

    fn check(&self, dns: DnsProvider, credential: &Value, names: &[String]) -> Result<(), String> {
        let cf = self.solver(dns, credential)?;
        Self::block(async {
            for n in names {
                cf.zone_for(n.trim_start_matches("*.")).await.map_err(|e| format!("{e:#}"))?;
            }
            Ok(())
        })
    }

    fn order(&self, o: Order<'_>) -> Result<Ordered, String> {
        Self::block(self.order_async(o))
    }
}

/// instant-acme's HTTP through reqwest, guarded: public addresses only (unless allowed), no
/// redirects, answers bounded.
struct Guarded {
    http: reqwest::Client,
    allow_private: bool,
}

impl HttpClient for Guarded {
    fn request(&self, req: Request<BodyWrapper<Bytes>>) -> Pin<Box<dyn Future<Output = Result<BytesResponse, instant_acme::Error>> + Send>> {
        let http = self.http.clone();
        let allow_private = self.allow_private;
        Box::pin(async move {
            let other = |e: String| instant_acme::Error::Other(e.into());
            let (parts, body) = req.into_parts();
            let url = url::Url::parse(&parts.uri.to_string()).map_err(|e| other(e.to_string()))?;
            if url.scheme() != "https" || (!allow_private && roll_net::literal_private_host(&url)) {
                return Err(other(format!("{} isn't a public HTTPS address", url.host_str().unwrap_or_default())));
            }
            let body = body.collect().await.map(|b| b.to_bytes()).unwrap_or_default();
            let mut resp = http.request(parts.method, url).headers(parts.headers).body(body).send().await.map_err(|e| other(e.without_url().to_string()))?;
            let mut out = Response::builder().status(resp.status());
            for (k, v) in resp.headers() {
                out = out.header(k, v);
            }
            if resp.content_length().is_some_and(|n| n > ANSWER_MAX as u64) {
                return Err(other(format!("the CA's answer is over {ANSWER_MAX} bytes")));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(|e| other(e.without_url().to_string()))? {
                if bytes.len() + chunk.len() > ANSWER_MAX {
                    return Err(other(format!("the CA's answer is over {ANSWER_MAX} bytes")));
                }
                bytes.extend_from_slice(&chunk);
            }
            let resp = out.body(Full::new(Bytes::from(bytes))).map_err(|e| other(e.to_string()))?;
            Ok(BytesResponse::from(resp))
        })
    }
}
