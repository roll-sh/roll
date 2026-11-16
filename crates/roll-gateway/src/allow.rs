//! The gateway's allow-list: what it may reach, decided inside the network, not by roll.
//!
//! An entry is `HOST:PORT`, `HOST:*`, `*.SUFFIX:PORT`, `CIDR:PORT` or `CIDR:*` (an IPv6 address
//! or CIDR in brackets: `[fd00::/8]:5432`). A name is allowed by a name entry, or when every
//! address it resolves to (on the gateway) is in a CIDR entry for the port; an IP literal only by
//! a CIDR entry. An empty list allows nothing.

use ipnet::IpNet;
use std::net::{IpAddr, SocketAddr};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Host {
    /// A name, lowercase.
    Name(String),
    /// `*.corp.internal`: any name ending in `.corp.internal`.
    Suffix(String),
    Net(IpNet),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Port {
    One(u16),
    Any,
}

impl Port {
    fn covers(self, p: u16) -> bool {
        matches!(self, Port::Any) || self == Port::One(p)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    host: Host,
    port: Port,
    text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllowList(Vec<Entry>);

/// A host name as DNS has them: labels of letters, digits, `-` and `_`, 253 characters at most.
fn name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 253 && s.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
}

fn parse_entry(raw: &str) -> Result<Entry, String> {
    let bad = || format!("{raw}: an entry is HOST:PORT, *.SUFFIX:PORT or CIDR:PORT (the port may be *)");
    let (host, port) = match raw.strip_prefix('[') {
        Some(rest) => {
            let (h, p) = rest.split_once("]:").ok_or_else(bad)?;
            (h, p)
        }
        None => raw.rsplit_once(':').ok_or_else(bad)?,
    };
    let port = match port {
        "*" => Port::Any,
        p => Port::One(p.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(bad)?),
    };
    let host = if let Ok(net) = host.parse::<IpNet>() {
        Host::Net(net.trunc())
    } else if let Ok(ip) = host.parse::<IpAddr>() {
        Host::Net(IpNet::from(ip))
    } else if let Some(suffix) = host.strip_prefix("*.") {
        if !name(suffix) {
            return Err(bad());
        }
        Host::Suffix(format!(".{}", suffix.to_ascii_lowercase()))
    } else if name(host) {
        Host::Name(host.to_ascii_lowercase())
    } else {
        return Err(bad());
    };
    Ok(Entry { host, port, text: raw.to_string() })
}

impl AllowList {
    /// Entries from flags or a file's lines (blank lines and `#` comments skipped).
    pub fn parse<'a>(lines: impl IntoIterator<Item = &'a str>) -> Result<AllowList, String> {
        let mut v = Vec::new();
        for l in lines {
            let l = l.split('#').next().unwrap_or_default().trim();
            if l.is_empty() {
                continue;
            }
            v.push(parse_entry(l)?);
        }
        if v.len() > crate::proto::MAX_ALLOW {
            return Err(format!("at most {} entries", crate::proto::MAX_ALLOW));
        }
        Ok(AllowList(v))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The entries as given (the hello reports them).
    pub fn entries(&self) -> Vec<String> {
        self.0.iter().map(|e| e.text.clone()).collect()
    }

    fn named(&self, host: &str, port: u16) -> bool {
        let host = host.to_ascii_lowercase();
        self.0.iter().any(|e| {
            e.port.covers(port)
                && match &e.host {
                    Host::Name(n) => *n == host,
                    Host::Suffix(s) => host.ends_with(s.as_str()) && host.len() > s.len(),
                    Host::Net(_) => false,
                }
        })
    }

    fn in_net(&self, ip: IpAddr, port: u16) -> bool {
        // An IPv4-mapped IPv6 address is the IPv4 address it maps.
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        self.0.iter().any(|e| e.port.covers(port) && matches!(&e.host, Host::Net(n) if n.contains(&ip)))
    }

    /// Where to dial for `host:port`, or why not. `resolve` looks a name up (on the gateway, with
    /// the network's own DNS); it's only asked for a name the list could allow.
    pub async fn check<F, Fut>(&self, host: &str, port: u16, resolve: F) -> Result<Vec<SocketAddr>, String>
    where
        F: FnOnce(String, u16) -> Fut,
        Fut: std::future::Future<Output = std::io::Result<Vec<SocketAddr>>>,
    {
        let refused = || format!("{host}:{port} isn't in the gateway's allow-list");
        let literal = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
        if let Ok(ip) = literal.parse::<IpAddr>() {
            return if self.in_net(ip, port) { Ok(vec![SocketAddr::new(ip, port)]) } else { Err(refused()) };
        }
        if !name(host) {
            return Err(format!("{host} isn't a host name"));
        }
        let by_name = self.named(host, port);
        if !by_name && !self.0.iter().any(|e| e.port.covers(port) && matches!(e.host, Host::Net(_))) {
            return Err(refused());
        }
        let addrs = resolve(host.to_string(), port).await.map_err(|_| format!("{host} doesn't resolve on the gateway"))?;
        if addrs.is_empty() {
            return Err(format!("{host} doesn't resolve on the gateway"));
        }
        if !by_name && !addrs.iter().all(|a| self.in_net(a.ip(), port)) {
            return Err(refused());
        }
        Ok(addrs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn to(addrs: &'static [&'static str]) -> std::io::Result<Vec<SocketAddr>> {
        Ok(addrs.iter().map(|a| a.parse().unwrap()).collect())
    }

    #[tokio::test]
    async fn the_gateway_reaches_only_what_it_lists() {
        let l = AllowList::parse(["10.0.0.0/8:5432", "db.corp.internal:3306", "*.ldap.corp:636", "[fd00::/8]:*", "# a comment", ""]).unwrap();
        assert_eq!(l.entries().len(), 4);
        let never = |_: String, _: u16| async { panic!("resolved a name the list can't allow") };
        // IP literals: only inside a CIDR entry for the port.
        assert_eq!(l.check("10.1.2.3", 5432, never).await.unwrap(), vec!["10.1.2.3:5432".parse().unwrap()]);
        assert!(l.check("10.1.2.3", 22, never).await.is_err());
        assert!(l.check("169.254.169.254", 80, never).await.is_err());
        assert!(l.check("[fd00::1]", 22, never).await.is_ok());
        assert!(l.check("::ffff:10.0.0.1", 5432, never).await.is_ok(), "a mapped address is its IPv4 one");
        // Names: by a name entry, or resolving only into a CIDR entry.
        assert!(l.check("DB.corp.internal", 3306, |_, _| to(&["192.168.9.9:3306"])).await.is_ok());
        assert!(l.check("dc1.ldap.corp", 636, |_, _| to(&["172.16.0.1:636"])).await.is_ok());
        assert!(l.check("ldap.corp", 636, |_, _| to(&["203.0.113.5:636"])).await.is_err(), "*.suffix needs a label before it");
        assert!(l.check("pg.internal", 5432, |_, _| to(&["10.9.9.9:5432"])).await.is_ok());
        assert!(l.check("pg.internal", 5432, |_, _| to(&["10.9.9.9:5432", "8.8.8.8:5432"])).await.is_err(), "every address must be inside");
        assert!(l.check("db.corp.internal", 22, |_, _| to(&["192.168.9.9:22"])).await.is_err());
        assert!(l.check("a b", 5432, never).await.is_err());
        // Nothing listed: nothing reached.
        assert!(AllowList::default().check("10.0.0.1", 5432, never).await.is_err());
        for bad in ["10.0.0.0/8", "host:0", "host:99999", "*.:5432", "a/b:1", "[fd00::1:5432"] {
            assert!(AllowList::parse([bad]).is_err(), "{bad}");
        }
    }
}
