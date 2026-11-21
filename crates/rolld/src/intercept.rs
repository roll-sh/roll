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
        Ok(roll_intercept::Leaf { cert_pem: l.cert_pem, key_pem: l.key.expose().to_string() })
    }
    fn filter(&self, req: roll_intercept::Outgoing<'_>) -> Result<usize, String> {
        let mut out = roll_secrets::proxy::Outgoing { host: req.host, path: req.path, headers: req.headers, query: req.query };
        // The response's scrub (the proxy contract's step 6) isn't wired into roll-intercept yet.
        roll_secrets::proxy::filter(&mut out, &*self.secrets, &roll_secrets::proxy::Limits::default()).map(|s| s.swapped()).map_err(|b| b.to_string())
    }
}

impl CaInterception {
    pub fn new(secrets: Arc<roll_secrets::Secrets>, ca: roll_secrets::intercept::InterceptCa) -> CaInterception {
        CaInterception { secrets, ca }
    }
}
