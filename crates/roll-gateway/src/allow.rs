//! The gateway's allow-list: what it may reach, decided inside the network, not by roll.
//!
//! An entry is a host, a `*.SUFFIX` or a CIDR, then a port: `HOST:PORT`, `HOST:LOW-HIGH` (a range,
//! both ends included) or `HOST:*` (an IPv6 address or CIDR in brackets: `[fd00::/8]:5432`). A name is allowed by a name entry, or when every
//! address it resolves to (on the gateway) is in a CIDR entry for the port; an IP literal only by
//! a CIDR entry. An empty list allows nothing.
//!
//! An SRV lookup (`mongodb+srv://`) is answered only when every target the record names passes
//! that same check, and is looked up at all only when the list could allow a target of it.

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
    /// `LOW-HIGH`, both included, `LOW <= HIGH`.
    Range(u16, u16),
    Any,
}

impl Port {
    fn covers(self, p: u16) -> bool {
        match self {
            Port::One(one) => one == p,
            Port::Range(low, high) => (low..=high).contains(&p),
            Port::Any => true,
        }
    }
}

/// A port, 1–65535, written in digits only (no sign, no spaces).
fn port(s: &str) -> Option<u16> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse::<u16>().ok()).flatten().filter(|p| *p != 0)
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
    let bad = || format!("{raw}: an entry is HOST:PORT, *.SUFFIX:PORT or CIDR:PORT (the port may be LOW-HIGH, 1-65535, or *)");
    let (host, port) = match raw.strip_prefix('[') {
        Some(rest) => {
            let (h, p) = rest.split_once("]:").ok_or_else(bad)?;
            (h, p)
        }
        None => raw.rsplit_once(':').ok_or_else(bad)?,
    };
    let port = match port.split_once('-') {
        _ if port == "*" => Port::Any,
        Some((low, high)) => match (self::port(low), self::port(high)) {
            (Some(low), Some(high)) if low <= high => Port::Range(low, high),
            _ => return Err(bad()),
        },
        None => Port::One(self::port(port).ok_or_else(bad)?),
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

    /// Whether the list could allow a target of an SRV record on `domain` (the record's own name
    /// without its `_service._proto` labels): a CIDR entry may, once the targets resolve; a name
    /// or suffix entry may when it shares the domain's parent, where the targets of a MongoDB
    /// record sit.
    fn could_hold(&self, domain: &str) -> bool {
        let domain = domain.to_ascii_lowercase();
        let parent = domain.split_once('.').map_or(domain.as_str(), |(_, p)| p);
        let under = format!(".{parent}");
        self.0.iter().any(|e| match &e.host {
            Host::Net(_) => true,
            Host::Name(n) => n.ends_with(&under) || *n == domain,
            Host::Suffix(s) => s.ends_with(&under) || under.ends_with(s.as_str()),
        })
    }

    /// The targets of `srv`'s SRV record (`_mongodb._tcp.cluster0.corp.internal`), each `host:port`,
    /// if every one is allowed; at most [`crate::proto::MAX_TARGETS`] are looked at. `lookup` asks
    /// the network's DNS for the record; `resolve` resolves a target as [`AllowList::check`]
    /// does. The error says why without naming a target (roll sees it; the gateway logs the
    /// target itself, through `refused`).
    pub async fn check_srv<L, LFut, R, RFut>(&self, srv: &str, lookup: L, resolve: R, refused: impl Fn(&str)) -> Result<Vec<String>, String>
    where
        L: FnOnce(String) -> LFut,
        LFut: std::future::Future<Output = std::io::Result<Vec<(String, u16)>>>,
        R: Fn(String, u16) -> RFut,
        RFut: std::future::Future<Output = std::io::Result<Vec<SocketAddr>>>,
    {
        let srv = srv.trim_end_matches('.');
        if !name(srv) {
            return Err(format!("{srv} isn't a host name"));
        }
        let domain = srv.split('.').skip_while(|l| l.starts_with('_')).collect::<Vec<_>>().join(".");
        if domain.is_empty() || domain.len() == srv.len() || !self.could_hold(&domain) {
            return Err(format!("{srv} isn't in the gateway's allow-list"));
        }
        let mut found = lookup(srv.to_string()).await.map_err(|_| format!("{srv} has no SRV record on the gateway"))?;
        found.truncate(crate::proto::MAX_TARGETS);
        if found.is_empty() {
            return Err(format!("{srv} has no SRV record on the gateway"));
        }
        let mut out = Vec::with_capacity(found.len());
        for (host, port) in found {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            if let Err(why) = self.check(&host, port, &resolve).await {
                refused(&format!("{host}:{port}: {why}"));
                return Err(format!("a target of {srv} isn't in the gateway's allow-list"));
            }
            out.push(format!("{host}:{port}"));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

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
        for bad in ["10.0.0.0/8", "host:0", "host:99999", "host:+5432", "*.:5432", "a/b:1", "[fd00::1:5432"] {
            assert!(AllowList::parse([bad]).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn a_port_range_holds_both_ends_and_nothing_past_them() {
        let l = AllowList::parse(["db.corp.internal:5432-5439", "*.ldap.corp:636-637", "10.0.0.0/8:27017-27019", "[fd00::/8]:9000-9000"]).unwrap();
        let never = |_: String, _: u16| async { panic!("resolved a name the list can't allow") };
        let named = |_: String, p: u16| async move { Ok(vec![SocketAddr::from(([192, 168, 9, 9], p))]) };
        for (host, inside, outside) in [("db.corp.internal", [5432, 5435, 5439], [5431, 5440]), ("dc1.ldap.corp", [636, 636, 637], [635, 638])] {
            for p in inside {
                assert!(l.check(host, p, named).await.is_ok(), "{host}:{p}");
            }
            for p in outside {
                assert!(l.check(host, p, never).await.is_err(), "{host}:{p}");
            }
        }
        for (p, ok) in [(27016, false), (27017, true), (27018, true), (27019, true), (27020, false)] {
            assert_eq!(l.check("10.1.2.3", p, never).await.is_ok(), ok, "10.1.2.3:{p}");
        }
        assert!(l.check("[fd00::1]", 9000, never).await.is_ok() && l.check("[fd00::1]", 9001, never).await.is_err());
        // A name resolving into a CIDR range's port is allowed; one past it isn't looked up.
        assert!(l.check("mongo.internal", 27019, |_, p| async move { Ok(vec![SocketAddr::from(([10, 0, 0, 5], p))]) }).await.is_ok());
        assert!(l.check("mongo.internal", 27020, never).await.is_err());
        // The entry is reported as given.
        assert_eq!(l.entries()[0], "db.corp.internal:5432-5439");
        // Strict: both ends 1-65535, LOW <= HIGH, digits only, one range.
        for bad in ["host:5439-5432", "host:0-10", "host:1-65536", "host:-5432", "host:5432-", "host:5432--5439", "host:1-2-3", "host:a-b", "host:*-10", "host: 1-2", "10.0.0.0/8:+1-2", "[fd00::/8]:9000-1"] {
            assert!(AllowList::parse([bad]).is_err(), "{bad}");
        }
        assert!(AllowList::parse(["host:1-65535"]).unwrap().check("host", 65535, named).await.is_ok());
    }

    #[tokio::test]
    async fn an_srv_record_is_answered_only_when_every_target_is_allowed() {
        let srv = |targets: &'static [(&'static str, u16)]| move |_: String| async move { Ok(targets.iter().map(|(h, p)| (h.to_string(), *p)).collect::<Vec<_>>()) };
        let resolve = |_: String, p: u16| async move { Ok(vec![SocketAddr::from(([10, 0, 0, 7], p))]) };
        let quiet = |_: &str| {};
        let l = AllowList::parse(["*.abcde.corp.internal:27017"]).unwrap();
        let got = l.check_srv("_mongodb._tcp.cluster0.abcde.corp.internal", srv(&[("Shard-0.abcde.corp.internal.", 27017), ("shard-1.abcde.corp.internal", 27017)]), resolve, quiet).await.unwrap();
        assert_eq!(got, ["shard-0.abcde.corp.internal:27017", "shard-1.abcde.corp.internal:27017"]);
        // One target outside the list refuses the whole answer, without naming it to roll.
        let logged = Mutex::new(vec![]);
        let err = l.check_srv("_mongodb._tcp.cluster0.abcde.corp.internal", srv(&[("shard-0.abcde.corp.internal", 27017), ("db.elsewhere.example", 27017)]), resolve, |s: &str| logged.lock().unwrap().push(s.to_string())).await.unwrap_err();
        assert!(!err.contains("elsewhere") && logged.lock().unwrap()[0].contains("db.elsewhere.example:27017"), "{err}");
        assert!(l.check_srv("_mongodb._tcp.cluster0.abcde.corp.internal", srv(&[("shard-0.abcde.corp.internal", 22)]), resolve, quiet).await.is_err(), "the port counts");
        // A name the list can't hold isn't looked up at all, nor is one without a domain.
        let never = |_: String| async { panic!("looked up a name the list can't allow") };
        assert!(l.check_srv("_mongodb._tcp.cluster0.other.example", never, resolve, quiet).await.is_err());
        assert!(l.check_srv("_mongodb._tcp", never, resolve, quiet).await.is_err());
        // A CIDR entry holds any name, once its targets resolve inside it; at most 16 are looked at.
        let nets = AllowList::parse(["10.0.0.0/8:27017"]).unwrap();
        let many: &'static [(&'static str, u16)] = Box::leak(vec![("db.corp.internal", 27017u16); 40].into_boxed_slice());
        assert_eq!(nets.check_srv("_mongodb._tcp.c.corp.internal", srv(many), resolve, quiet).await.unwrap().len(), crate::proto::MAX_TARGETS);
    }
}
