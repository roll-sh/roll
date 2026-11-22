//! roll secrets as the intercepting egress step's [`roll_intercept::Interception`]: one consumer's
//! interception CA, live as roll secrets says on every connection, and roll secrets' own filter
//! for each request. roll previews' egress uses it; roll apps' closed environments can too.

use std::sync::Arc;

/// One CA's interception: live as roll secrets says (asked every connection), its leaves, and
/// roll secrets' filter against the host connected to.
pub struct CaInterception {
    secrets: Arc<roll_secrets::Secrets>,
    ca: roll_secrets::intercept::InterceptCa,
}

impl roll_intercept::Interception for CaInterception {
    fn live(&self) -> anyhow::Result<bool> {
        self.secrets.intercept_ca_live(&self.ca.id).map_err(|e| anyhow::anyhow!("{e}"))
    }
    fn covers(&self, host: &str) -> bool {
        self.ca.covers(host)
    }
    fn leaf(&self, host: &str) -> anyhow::Result<roll_intercept::Leaf> {
        let l = self.ca.leaf(host).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(roll_intercept::Leaf { cert_pem: l.cert_pem, key_pem: l.key.expose().to_string().into() })
    }
    fn filter(&self, req: roll_intercept::Outgoing<'_>) -> Result<roll_intercept::Filtered, String> {
        let mut out = roll_secrets::proxy::Outgoing { host: req.host, path: req.path, headers: req.headers, query: req.query };
        let scrub = roll_secrets::proxy::filter(&mut out, &*self.secrets, &roll_secrets::proxy::Limits::default()).map_err(|b| b.to_string())?;
        let swapped = scrub.swapped();
        let scrub = (swapped > 0).then(|| Box::new(SecretsScrub { scrub, secrets: self.secrets.clone(), echoed: None }) as Box<dyn roll_intercept::ResponseScrub>);
        Ok(roll_intercept::Filtered { swapped, scrub })
    }
}

/// roll secrets' scrub of one response ([`roll_secrets::proxy::Scrub`]: the values stay in it),
/// and on an echo [`roll_secrets::Secrets::echoed`]: the placeholder ended, an alert audited, the
/// environment's notice queued.
struct SecretsScrub {
    scrub: roll_secrets::proxy::Scrub,
    secrets: Arc<roll_secrets::Secrets>,
    echoed: Option<roll_secrets::proxy::Echoed>,
}

impl SecretsScrub {
    fn saw(&mut self, e: roll_secrets::proxy::Echoed) -> String {
        let why = e.to_string();
        self.echoed = Some(e);
        why
    }
}

impl roll_intercept::ResponseScrub for SecretsScrub {
    fn headers(&mut self, headers: &axum::http::HeaderMap) -> Result<(), String> {
        self.scrub.headers(headers).map_err(|e| self.saw(e))
    }
    fn chunk(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.scrub.chunk(bytes).map_err(|e| self.saw(e))
    }
    fn echoed(&self) {
        if let Some(e) = &self.echoed
            && let Err(err) = self.secrets.echoed(&e.placeholder, &e.host)
        {
            tracing::warn!(host = %e.host, key = %e.key, error = %err, "intercept: an echoed key's placeholder wasn't ended");
        }
    }
}

impl CaInterception {
    pub fn new(secrets: Arc<roll_secrets::Secrets>, ca: roll_secrets::intercept::InterceptCa) -> CaInterception {
        CaInterception { secrets, ca }
    }
}
