//! ACME's challenge checks (roll-secrets' acme module, docs/secrets.md "ACME"): `http-01` through a
//! guarded client (`roll_net::PublicOnly`, the literal-host check, no redirects, no proxy, a
//! timeout and a bounded read), `dns-01` through public DNS (Cloudflare's resolvers, not the
//! fleet's own, so a private zone can't answer for a public name), with a timeout and bounded
//! answers. A deployment that allows private addresses (`ROLL_CODE_WEBHOOK_ALLOW_PRIVATE`, local
//! development) reaches them and resolves through the system's resolver.

use hickory_resolver::proto::rr::{RData, RecordType};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);
/// TXT records read at one name, and bytes kept of one.
const TXT_MAX: usize = 32;
const TXT_BYTES: usize = 1024;

pub(super) struct Checks {
    client: reqwest::Client,
    allow_private: bool,
}

impl Checks {
    pub(super) fn new(allow_private: bool) -> Checks {
        let mut b = reqwest::Client::builder().timeout(TIMEOUT).redirect(reqwest::redirect::Policy::none()).user_agent("roll-secrets-acme");
        if !allow_private {
            b = b.dns_resolver(roll_net::PublicOnly).no_proxy();
        }
        Checks { client: b.build().expect("an HTTP client"), allow_private }
    }

    /// A resolver per lookup: nothing cached, so a record a client just added is seen.
    fn resolver(&self) -> Result<hickory_resolver::TokioResolver, String> {
        use hickory_resolver::config::{CLOUDFLARE, ResolverConfig};
        let mut b = if self.allow_private {
            hickory_resolver::TokioResolver::builder_tokio().map_err(|e| e.to_string())?
        } else {
            hickory_resolver::Resolver::builder_with_config(ResolverConfig::udp_and_tcp(&CLOUDFLARE), Default::default())
        };
        b.options_mut().timeout = Duration::from_secs(5);
        b.options_mut().attempts = 2;
        b.build().map_err(|e| e.to_string())
    }
}

fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Handle::current().block_on(f)
}

impl roll_secrets::acme::Checks for Checks {
    fn txt(&self, name: &str) -> Result<Vec<String>, String> {
        let resolver = self.resolver()?;
        let found = block_on(tokio::time::timeout(TIMEOUT, resolver.lookup(format!("{name}."), RecordType::TXT))).map_err(|_| "timed out".to_string())?.map_err(|e| e.to_string())?;
        Ok(found
            .answers()
            .iter()
            .filter_map(|r| match &r.data {
                RData::TXT(t) => Some(t.txt_data.iter().flat_map(|s| s.iter().copied()).take(TXT_BYTES).collect::<Vec<u8>>()),
                _ => None,
            })
            .take(TXT_MAX)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .collect())
    }

    fn get(&self, url: &str, limit: usize) -> Result<Vec<u8>, String> {
        let u = url::Url::parse(url).map_err(|e| e.to_string())?;
        if u.scheme() != "http" || u.host_str().is_none() {
            return Err("an http-01 challenge is fetched over http://".into());
        }
        if !self.allow_private && roll_net::literal_private_host(&u) {
            return Err(format!("{} is a private address", u.host_str().unwrap_or_default()));
        }
        block_on(async {
            let mut resp = self.client.get(u).send().await.map_err(|e| e.without_url().to_string())?;
            if !resp.status().is_success() {
                return Err(format!("answered {}", resp.status()));
            }
            let mut out = Vec::new();
            while let Some(c) = resp.chunk().await.map_err(|e| e.without_url().to_string())? {
                if out.len() + c.len() > limit {
                    return Err(format!("an answer over {limit} bytes"));
                }
                out.extend_from_slice(&c);
            }
            Ok(out)
        })
    }
}
