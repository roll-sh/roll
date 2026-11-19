//! Who may reach a cluster's data plane: its IP allowlist (Elastic Cloud's traffic filters, AWS
//! OpenSearch Service's IP-based policies), and how a listener decides a request's client
//! address (docs/search/architecture.md, "Client addresses and IP allowlists").
//!
//! - [`IpAllowlist`]: CIDRs, IPv4 and IPv6; empty admits everyone. An IPv4-mapped IPv6 address
//!   (`::ffff:203.0.113.7`, what a dual-stack socket reports for an IPv4 peer) is the IPv4
//!   address it carries, and a mapped CIDR is stored as the IPv4 CIDR it means, so IPv4 clients
//!   match IPv4 entries however they arrive.
//! - [`Origin`]: where a request comes from. A client's address; roll's own control plane (the
//!   console, the per-index routes, links' credentials), which never comes over the data plane
//!   and isn't filtered; or unknown, which an allowlist refuses.
//! - [`ClientAddresses`]: the listener's rule. The client is the connection's peer (the PROXY
//!   header's source when the listener expects one); a header the operator names is believed
//!   only when that peer is one of the operator's trusted proxies, and never otherwise.

use hyper::HeaderMap;
use hyper::header::HeaderName;
use ipnet::{IpNet, Ipv4Net};
use std::net::{IpAddr, SocketAddr};

/// The most entries an allowlist may have.
pub const MAX_ENTRIES: usize = 64;

/// The body of the answer a refused request gets: Elastic Cloud's proxy's 403 for traffic
/// filtering. It names nothing about the cluster (no name, flavor or version).
pub const REFUSED_BODY: &str = r#"{"ok":false,"message":"Forbidden due to traffic filtering"}"#;

/// `ip`, with an IPv4-mapped IPv6 address as the IPv4 address it carries.
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// A CIDR (`203.0.113.0/24`, `2001:db8::/32`) or a bare address (one host), kept canonical as
/// roll apps' allowlists are: `10.1.2.3/8` is `10.0.0.0/8`.
pub fn parse_cidr(s: &str) -> Result<IpNet, String> {
    let s = s.trim();
    let net = match s.parse::<IpNet>() {
        Ok(n) => n,
        Err(_) => match s.parse::<IpAddr>() {
            Ok(ip) => IpNet::from(ip),
            Err(_) => return Err(format!("{s:?} isn't a CIDR (203.0.113.0/24, 2001:db8::/32) or an IP address")),
        },
    };
    Ok(mapped_as_v4(net.trunc()))
}

/// `::ffff:a.b.c.d/n` (n ≥ 96) as `a.b.c.d/(n - 96)`.
fn mapped_as_v4(net: IpNet) -> IpNet {
    if let IpNet::V6(n) = net
        && n.prefix_len() >= 96
        && let Some(v4) = n.addr().to_ipv4_mapped()
    {
        return IpNet::V4(Ipv4Net::new(v4, n.prefix_len() - 96).expect("a prefix of at most 32"));
    }
    net
}

/// Where a data-plane request comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A client, at this address.
    Client(IpAddr),
    /// roll's own control plane, in process (or forwarded, signed, by a node of the fleet).
    Internal,
    /// No address anyone can vouch for: refused by any allowlist.
    Unknown,
}

/// A cluster's IP allowlist (empty: open).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IpAllowlist(Vec<IpNet>);

impl IpAllowlist {
    pub fn new(nets: impl IntoIterator<Item = IpNet>) -> IpAllowlist {
        IpAllowlist(nets.into_iter().map(mapped_as_v4).collect())
    }

    pub fn is_open(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether an entry contains `ip`.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        self.0.iter().any(|n| n.contains(&ip))
    }

    /// Whether a request from `origin` may reach the cluster.
    pub fn admits(&self, origin: Origin) -> bool {
        if self.0.is_empty() {
            return true;
        }
        match origin {
            Origin::Internal => true,
            Origin::Client(ip) => self.contains(ip),
            Origin::Unknown => false,
        }
    }
}

/// How a listener decides the client address of a request (rolld: `--search-client-ip-header`,
/// `--search-trusted-proxies`).
#[derive(Clone, Debug, Default)]
pub struct ClientAddresses {
    /// The header a trusted proxy puts the client's address in (`CF-Connecting-IP`).
    header: Option<HeaderName>,
    /// The peers whose header is believed.
    trusted: Vec<IpNet>,
}

impl ClientAddresses {
    /// `header` and `trusted` (comma-separated CIDRs or addresses) go together: one without the
    /// other is a mistake, refused.
    pub fn new(header: Option<&str>, trusted: Option<&str>) -> anyhow::Result<ClientAddresses> {
        let header = header.map(str::trim).filter(|h| !h.is_empty());
        let trusted: Vec<IpNet> = trusted
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| parse_cidr(s).map_err(|e| anyhow::anyhow!("--search-trusted-proxies: {e}")))
            .collect::<anyhow::Result<_>>()?;
        match (header, trusted.is_empty()) {
            (None, true) => Ok(ClientAddresses::default()),
            (Some(_), true) => anyhow::bail!("--search-client-ip-header needs --search-trusted-proxies: the header is believed only from those peers"),
            (None, false) => anyhow::bail!("--search-trusted-proxies needs --search-client-ip-header: the header those proxies set"),
            (Some(h), false) => {
                let header = HeaderName::try_from(h).map_err(|_| anyhow::anyhow!("--search-client-ip-header: {h:?} isn't a header name"))?;
                anyhow::ensure!(!is_client_set(&header), "--search-client-ip-header: {h} is a header any client sets (name the one your proxy writes, e.g. CF-Connecting-IP)");
                Ok(ClientAddresses { header: Some(header), trusted })
            }
        }
    }

    /// The client address of a request that came from `peer` (the connection's peer, or the
    /// PROXY header's source): the header's address when `peer` is a trusted proxy and the
    /// header holds one (its last one: what the proxy itself appended), else `peer`.
    pub fn client(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        let peer = canonical(peer);
        let Some(h) = &self.header else { return peer };
        if !self.trusted.iter().any(|n| n.contains(&peer)) {
            return peer;
        }
        headers
            .get_all(h)
            .iter()
            .next_back()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|v| parse_addr(v.trim()))
            .map_or(peer, canonical)
    }
}

/// `X-Forwarded-For` and `Forwarded` hold whatever the client wrote before a proxy appended:
/// naming them is refused, since a proxy that passes them on instead of appending would hand a
/// client's own word through. Name the header the proxy writes itself.
fn is_client_set(h: &HeaderName) -> bool {
    matches!(h.as_str(), "x-forwarded-for" | "forwarded")
}

fn parse_addr(s: &str) -> Option<IpAddr> {
    s.parse::<IpAddr>().ok().or_else(|| s.parse::<SocketAddr>().ok().map(|a| a.ip()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn cidrs_parse_and_refuse_what_they_should() {
        assert_eq!(parse_cidr("203.0.113.0/24").unwrap().to_string(), "203.0.113.0/24");
        assert_eq!(parse_cidr(" 2001:db8::/32 ").unwrap().to_string(), "2001:db8::/32");
        assert_eq!(parse_cidr("198.51.100.7").unwrap().to_string(), "198.51.100.7/32");
        assert_eq!(parse_cidr("2001:db8::1").unwrap().to_string(), "2001:db8::1/128");
        assert_eq!(parse_cidr("::ffff:10.0.0.0/104").unwrap().to_string(), "10.0.0.0/8", "a mapped CIDR is the IPv4 one");
        assert_eq!(parse_cidr("10.1.2.3/8").unwrap().to_string(), "10.0.0.0/8", "canonical");
        for bad in ["", "nope", "10.0.0.0/33", "10.0.0/8", "2001:db8::/129"] {
            assert!(parse_cidr(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ipv4_mapped_addresses_match_ipv4_entries() {
        let list = IpAllowlist::new([parse_cidr("203.0.113.0/24").unwrap(), parse_cidr("2001:db8::/32").unwrap()]);
        assert!(list.admits(Origin::Client(ip("203.0.113.9"))));
        assert!(list.admits(Origin::Client(ip("::ffff:203.0.113.9"))), "a dual-stack socket's IPv4 peer");
        assert!(list.admits(Origin::Client(ip("2001:db8::5"))));
        assert!(!list.admits(Origin::Client(ip("198.51.100.1"))));
        assert!(!list.admits(Origin::Client(ip("::ffff:198.51.100.1"))));
        assert!(!list.admits(Origin::Unknown), "nobody vouches for it");
        assert!(list.admits(Origin::Internal), "roll's control plane");
        let mapped = IpAllowlist::new(["::ffff:198.51.100.0/120".parse().unwrap()]);
        assert!(mapped.admits(Origin::Client(ip("198.51.100.4"))));
        assert!(IpAllowlist::default().admits(Origin::Unknown), "empty is open");
    }

    #[test]
    fn a_header_is_believed_only_from_a_trusted_proxy() {
        let cfg = ClientAddresses::new(Some("CF-Connecting-IP"), Some("10.0.0.0/8, 192.0.2.7")).unwrap();
        let mut h = HeaderMap::new();
        h.insert("cf-connecting-ip", "203.0.113.9".parse().unwrap());
        assert_eq!(cfg.client(ip("10.1.2.3"), &h), ip("203.0.113.9"));
        assert_eq!(cfg.client(ip("::ffff:192.0.2.7"), &h), ip("203.0.113.9"), "a mapped trusted peer");
        assert_eq!(cfg.client(ip("198.51.100.1"), &h), ip("198.51.100.1"), "an untrusted peer's header is ignored");
        h.insert("cf-connecting-ip", "garbage".parse().unwrap());
        assert_eq!(cfg.client(ip("10.1.2.3"), &h), ip("10.1.2.3"), "an unreadable header leaves the proxy's own address");
        assert_eq!(ClientAddresses::default().client(ip("::ffff:10.1.2.3"), &h), ip("10.1.2.3"));
        assert!(ClientAddresses::new(Some("CF-Connecting-IP"), None).is_err());
        assert!(ClientAddresses::new(None, Some("10.0.0.0/8")).is_err());
        assert!(ClientAddresses::new(Some("X-Forwarded-For"), Some("10.0.0.0/8")).is_err(), "a client's own header");
    }
}
