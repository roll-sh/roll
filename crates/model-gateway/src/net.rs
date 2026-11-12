//! The gateway's HTTP client for upstreams: rustls, no redirects, timeouts, and private
//! addresses refused (a literal host, or a name resolving to one) unless the upstream allows
//! them (`UpstreamSpec::allow_private`): AGENTS.md's `PublicOnly` rule
//! (`roll-net`).
//!
//! Two clients, built once: one resolves through [`PublicOnly`] and ignores proxies from the
//! environment (a proxy would resolve the name itself, past the check), the other allows any
//! address. Neither follows redirects, so an upstream can't bounce a request (and its key)
//! somewhere else.

use std::time::Duration;

/// Connecting to an upstream.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest gap between two reads of an upstream's body (a stream that stops moving).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// From sending a request to its response's headers: long, because a non-streamed answer
/// with long thinking only answers when it's done.
pub const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(900);

pub use roll_net::{PublicOnly, literal_private_host, private_ip};

/// The two clients.
#[derive(Clone)]
pub struct Net {
    public: reqwest::Client,
    private: reqwest::Client,
}

impl Net {
    pub fn new() -> anyhow::Result<Net> {
        let base = || {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(IDLE_TIMEOUT)
                .pool_idle_timeout(Duration::from_secs(90))
                .no_proxy()
        };
        Ok(Net { public: base().dns_resolver(PublicOnly).build()?, private: base().build()? })
    }

    /// The client for a request to `url`, after checking it (a literal private host is refused
    /// unless `allow_private`; a name is checked as it resolves).
    pub fn client(&self, url: &url::Url, allow_private: bool) -> Result<&reqwest::Client, String> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("{} isn't http(s)", url.scheme()));
        }
        if allow_private {
            return Ok(&self.private);
        }
        if literal_private_host(url) {
            return Err(format!("{} is a private address (allow private addresses for this upstream to use it)", url.host_str().unwrap_or_default()));
        }
        Ok(&self.public)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn private_literals_need_allowing() {
        let net = Net::new().unwrap();
        let local = url::Url::parse(&format!("http://{}:9/v1", Ipv4Addr::LOCALHOST)).unwrap();
        assert!(net.client(&local, false).unwrap_err().contains("private address"));
        assert!(net.client(&local, true).is_ok());
        assert!(net.client(&url::Url::parse("https://api.anthropic.com").unwrap(), false).is_ok());
        assert!(net.client(&url::Url::parse("file:///etc/passwd").unwrap(), true).is_err());
    }
}
