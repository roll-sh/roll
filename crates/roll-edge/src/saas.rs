//! Cloudflare for SaaS custom hostnames (docs/apps/README.md, "Networking"): on the fleet a
//! custom domain is a custom hostname on the site zone, whose certificate Cloudflare issues over
//! HTTP DCV and renews at its edge once the customer's CNAME points at the fallback origin
//! (`up.<site>`). Traffic reaches the edge from Cloudflare with the custom hostname as SNI and
//! `Host`; the edge answers it with the fallback origin's certificate
//! ([`crate::tls::CertStore::set_unknown_name`]).
//!
//! rolld adapts this to roll apps' certificate seam; nothing here knows roll apps. Only call it
//! with hostnames roll apps has accepted, and only [`crate::domains::acceptable`] ones are sent.

use crate::acme::{Cloudflare, short};
use crate::domains::acceptable;
use std::sync::Arc;

/// A custom hostname's certificate, as Cloudflare reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostnameCert {
    /// Waiting for DCV or issuance; Cloudflare's reason when it gives one.
    Pending(Option<String>),
    Issued,
    Failed(String),
}

#[derive(serde::Deserialize)]
struct CustomHostname {
    id: String,
    hostname: String,
    #[serde(default)]
    ssl: Option<Ssl>,
    #[serde(default)]
    verification_errors: Vec<String>,
}

#[derive(serde::Deserialize)]
struct Ssl {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    validation_errors: Vec<ValidationError>,
}

#[derive(serde::Deserialize)]
struct ValidationError {
    #[serde(default)]
    message: String,
}

#[derive(serde::Deserialize)]
struct Deleted {
    #[allow(dead_code)]
    id: String,
}

impl CustomHostname {
    fn cert(&self) -> HostnameCert {
        let ssl = self.ssl.as_ref();
        let status = ssl.and_then(|s| s.status.as_deref()).unwrap_or("initializing");
        let reason = ssl
            .and_then(|s| s.validation_errors.iter().map(|e| e.message.as_str()).find(|m| !m.is_empty()))
            .or_else(|| self.verification_errors.iter().map(String::as_str).find(|m| !m.is_empty()))
            .map(|m| short(m, 300));
        match status {
            "active" => HostnameCert::Issued,
            "expired" | "deleted" | "inactive" | "deactivating" | "pending_deletion" => HostnameCert::Failed(reason.unwrap_or_else(|| format!("the certificate is {status}"))),
            s if s.ends_with("timed_out") => HostnameCert::Failed(reason.unwrap_or_else(|| format!("Cloudflare gave up: {s}"))),
            _ => HostnameCert::Pending(reason),
        }
    }
}

/// Custom hostnames on one zone.
pub struct CustomHostnames {
    cf: Arc<Cloudflare>,
    /// The site zone's name (`roll.site`).
    zone: String,
    /// roll's own zones: never sent.
    refuse: Vec<String>,
}

impl std::fmt::Debug for CustomHostnames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomHostnames").field("zone", &self.zone).finish_non_exhaustive()
    }
}

impl CustomHostnames {
    /// `zone`: the zone the fallback origin is on; `refuse`: roll's own zones.
    pub fn new(cf: Arc<Cloudflare>, zone: &str, refuse: Vec<String>) -> Self {
        Self { cf, zone: zone.trim_matches('.').to_ascii_lowercase(), refuse }
    }

    async fn zone_id(&self) -> anyhow::Result<String> {
        self.cf.zone_id(&self.zone).await?.ok_or_else(|| anyhow::anyhow!("Cloudflare: no zone {} (does the token see it?)", self.zone))
    }

    fn check(&self, host: &str) -> anyhow::Result<()> {
        anyhow::ensure!(acceptable(host, &self.refuse), "{host:?} isn't a hostname roll sends to Cloudflare");
        Ok(())
    }

    async fn find(&self, zone: &str, host: &str) -> anyhow::Result<Option<CustomHostname>> {
        // `acceptable` hostnames are `[a-z0-9.-]`: nothing to escape.
        let url = self.cf.url(&format!("/zones/{zone}/custom_hostnames?hostname={host}"));
        let found: Vec<CustomHostname> = self.cf.call(self.cf.http().get(url), &format!("find custom hostname {host}")).await?;
        Ok(found.into_iter().find(|h| h.hostname.eq_ignore_ascii_case(host)))
    }

    /// Make sure `host` is a custom hostname (HTTP DCV, a DV certificate), creating it if it
    /// isn't, and say where its certificate is.
    pub async fn ensure(&self, host: &str) -> anyhow::Result<HostnameCert> {
        self.check(host)?;
        let zone = self.zone_id().await?;
        if let Some(h) = self.find(&zone, host).await? {
            return Ok(h.cert());
        }
        let body = serde_json::json!({ "hostname": host, "ssl": { "method": "http", "type": "dv", "settings": { "min_tls_version": "1.2" } } });
        let url = self.cf.url(&format!("/zones/{zone}/custom_hostnames"));
        let created: CustomHostname = self.cf.call(self.cf.http().post(url).json(&body), &format!("create custom hostname {host}")).await?;
        Ok(created.cert())
    }

    /// Delete `host`'s custom hostname, if there is one.
    pub async fn delete(&self, host: &str) -> anyhow::Result<()> {
        self.check(host)?;
        let zone = self.zone_id().await?;
        let Some(h) = self.find(&zone, host).await? else { return Ok(()) };
        anyhow::ensure!(!h.id.is_empty() && h.id.len() <= 64 && h.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "Cloudflare: a custom hostname id that isn't one");
        let url = self.cf.url(&format!("/zones/{zone}/custom_hostnames/{}", h.id));
        let _: Deleted = self.cf.call(self.cf.http().delete(url), &format!("delete custom hostname {host}")).await?;
        Ok(())
    }
}
