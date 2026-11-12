//! The gateway's HTTP client for upstreams: rustls, no redirects, timeouts, and private
//! addresses refused (a literal host, or a name resolving to one) unless the upstream allows
//! them (`UpstreamSpec::allow_private`): AGENTS.md's `PublicOnly` rule, as
//! `code-control/src/review/apps.rs` does it ([`private_ip`], [`PublicOnly`] and
//! [`literal_private_host`] are ported from there).
//!
//! Two clients, built once: one resolves through [`PublicOnly`] and ignores proxies from the
//! environment (a proxy would resolve the name itself, past the check), the other allows any
//! address. Neither follows redirects, so an upstream can't bounce a request (and its key)
//! somewhere else.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// Connecting to an upstream.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest gap between two reads of an upstream's body (a stream that stops moving).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// From sending a request to its response's headers: long, because a non-streamed answer
/// with long thinking only answers when it's done.
pub const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(900);

/// Loopback, private, link-local, CGNAT, multicast, unspecified and documentation ranges (and
/// IPv4 inside IPv6). Ported from `code-control/src/review/apps.rs`.
pub fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            v.is_loopback()
                || v.is_private()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_multicast()
                || v.is_broadcast()
                || v.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || o[0] >= 240
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return private_ip(IpAddr::V4(v4));
            }
            let s = v.segments();
            // IPv4 carried inside IPv6: NAT64 (64:ff9b::/96, also the local-use 64:ff9b:1::/48)
            // and 6to4 (2002::/16) reach whatever IPv4 address they embed.
            let embedded = |hi: u16, lo: u16| std::net::Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
            if (s[0] == 0x64 && s[1] == 0xff9b && (s[2..6] == [0, 0, 0, 0] || s[2] == 1)) && private_ip(IpAddr::V4(embedded(s[6], s[7]))) {
                return true;
            }
            if s[0] == 0x2002 && private_ip(IpAddr::V4(embedded(s[1], s[2]))) {
                return true;
            }
            v.is_loopback() || v.is_unspecified() || v.is_multicast() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 || s[0] == 0x2001 && s[1] == 0xdb8
        }
    }
}

/// Resolves names, refusing private addresses. Ported from `code-control/src/review/apps.rs`.
pub struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = addrs.into_iter().filter(|a| !private_ip(a.ip())).collect();
            if public.is_empty() {
                return Err(format!("{host} resolves to a private address").into());
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Is `url`'s host a private address written out (an IP literal, or `localhost`)? IP literals
/// never reach the resolver, so they're checked here. Ported from
/// `code-control/src/review/apps.rs`.
pub fn literal_private_host(url: &url::Url) -> bool {
    let literal = match url.host() {
        Some(url::Host::Ipv4(v)) => Some(IpAddr::V4(v)),
        Some(url::Host::Ipv6(v)) => Some(IpAddr::V6(v)),
        Some(url::Host::Domain(d)) => d.parse::<IpAddr>().ok(),
        None => None,
    };
    literal.is_some_and(private_ip) || url.host_str().is_some_and(|h| h.eq_ignore_ascii_case("localhost") || h.to_ascii_lowercase().ends_with(".localhost"))
}

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
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn private_ranges() {
        let v4 = |a, b, c, d| IpAddr::V4(Ipv4Addr::new(a, b, c, d));
        for ip in [v4(127, 0, 0, 1), v4(10, 1, 2, 3), v4(172, 16, 0, 1), v4(192, 168, 1, 1), v4(169, 254, 169, 254), v4(100, 64, 0, 1), v4(0, 0, 0, 0)] {
            assert!(private_ip(ip), "{ip}");
        }
        for s in ["::1", "fc00::1", "fe80::1", "64:ff9b::a9fe:a9fe", "2002:7f00:1::"] {
            assert!(private_ip(s.parse().unwrap()), "{s}");
        }
        assert!(private_ip(IpAddr::V6(Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped())));
        for ip in [v4(1, 1, 1, 1), v4(8, 8, 8, 8), IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, 0x1111))] {
            assert!(!private_ip(ip), "{ip}");
        }
        let u = |s: &str| url::Url::parse(s).unwrap();
        assert!(literal_private_host(&u(&format!("http://{}:8080/x", Ipv4Addr::LOCALHOST))));
        assert!(literal_private_host(&u("https://[::1]/x")));
        assert!(literal_private_host(&u("http://LOCALHOST/x")));
        assert!(!literal_private_host(&u("https://api.openai.com/v1")));
    }

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
