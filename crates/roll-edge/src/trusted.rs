//! Cloudflare's addresses: a request from one of them, for a hostname Cloudflare proxies, carries
//! the real client in `CF-Connecting-IP`. From anywhere else that header is a lie.
//!
//! The list is Cloudflare's published ranges (https://www.cloudflare.com/ips/). It changes
//! rarely; `ROLL_EDGE_CLOUDFLARE_IPS` (comma-separated CIDRs) replaces it without a release.

use ipnet::IpNet;
use std::net::IpAddr;

/// https://www.cloudflare.com/ips-v4 and https://www.cloudflare.com/ips-v6
pub const CLOUDFLARE_RANGES: &[&str] = &[
    "173.245.48.0/20",
    "103.21.244.0/22",
    "103.22.200.0/22",
    "103.31.4.0/22",
    "141.101.64.0/18",
    "108.162.192.0/18",
    "190.93.240.0/20",
    "188.114.96.0/20",
    "197.234.240.0/22",
    "198.41.128.0/17",
    "162.158.0.0/15",
    "104.16.0.0/13",
    "104.24.0.0/14",
    "172.64.0.0/13",
    "131.0.72.0/22",
    "2400:cb00::/32",
    "2606:4700::/32",
    "2803:f800::/32",
    "2405:b500::/32",
    "2405:8100::/32",
    "2a06:98c0::/29",
    "2c0f:f248::/32",
];

#[derive(Clone, Debug)]
pub struct TrustedProxies {
    nets: Vec<IpNet>,
}

impl TrustedProxies {
    /// Cloudflare's ranges.
    pub fn cloudflare() -> Self {
        Self { nets: CLOUDFLARE_RANGES.iter().map(|c| c.parse().expect("valid CIDR")).collect() }
    }

    /// Comma-separated CIDRs (or bare addresses).
    pub fn parse(list: &str) -> anyhow::Result<Self> {
        let nets = list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<IpNet>().or_else(|_| s.parse::<IpAddr>().map(IpNet::from)).map_err(|_| anyhow::anyhow!("{s} isn't a CIDR")))
            .collect::<Result<_, _>>()?;
        Ok(Self { nets })
    }

    /// No one (tests, a node not behind Cloudflare).
    pub fn none() -> Self {
        Self { nets: vec![] }
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        self.nets.iter().any(|n| n.contains(&ip))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        let cf = TrustedProxies::cloudflare();
        assert!(cf.contains("104.16.1.1".parse().unwrap()));
        assert!(cf.contains("::ffff:104.16.1.1".parse().unwrap()));
        assert!(cf.contains("2606:4700::1".parse().unwrap()));
        assert!(!cf.contains("203.0.113.9".parse().unwrap()));
        let custom = TrustedProxies::parse("10.0.0.0/8, 192.0.2.7").unwrap();
        assert!(custom.contains("10.1.2.3".parse().unwrap()) && custom.contains("192.0.2.7".parse().unwrap()));
        assert!(TrustedProxies::parse("nope").is_err());
    }
}
