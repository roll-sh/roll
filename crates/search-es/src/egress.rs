//! The one check for every outbound connection to an address a tenant chose: reindex from
//! remote, a migration's calls, the `postgresql` and `s3` connectors, inference services,
//! watches' HTTP inputs and actions, and snapshot repositories on S3, GCS or Azure endpoints.
//!
//! On top of roll's rule for such URLs (AGENTS.md: [`roll_net::private_ip`] refused unless the
//! feature's own `allow_private` setting is on, local development only), a node never connects
//! to:
//! - the networks and addresses of [`DENY_ADDRESSES`]: rolld puts the operator's
//!   `--search-egress-deny` networks here, and its own public addresses as it resolved them at
//!   start;
//! - whatever the names of [`DENY_NAMES`] resolve to now: rolld puts the names it serves on
//!   here. They're resolved again every [`DENY_NAMES_TTL`], so the check follows the addresses as
//!   they change. Until every name has answered once (addresses, or "no such name"), every
//!   connection fails closed ([`NOT_READY`]): nothing goes out while roll doesn't know its own
//!   addresses.
//!
//! The check is made on the addresses a connection uses, never on the name alone: a client is
//! built with [`Egress::resolver`] (reqwest), or connects to an address [`Egress::resolve`] gave
//! (raw TCP), so no second lookup can lead somewhere else. Addresses written out never reach a
//! resolver, so [`Egress::check_literal`] (or [`Egress::check_literal_now`], with the node's
//! names resolved afresh) checks them before connecting. IPv4 carried in IPv6 (mapped, NAT64,
//! 6to4, compatible) is checked as the IPv4 address it carries.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

/// Addresses no outbound call connects to, whatever name leads there (CIDRs or addresses,
/// comma-separated).
pub const DENY_ADDRESSES: &str = "roll.egress.deny_addresses";
/// Names whose addresses no outbound call connects to (comma-separated).
pub const DENY_NAMES: &str = "roll.egress.deny_names";
/// What [`DENY_ADDRESSES`] and [`DENY_NAMES`] were called when they covered reindex alone; read
/// too, so a node configured by name keeps its rules.
const OLD_DENY_ADDRESSES: &str = "roll.reindex.remote.deny_addresses";
const OLD_DENY_NAMES: &str = "roll.reindex.remote.deny_names";
/// How long the addresses of [`DENY_NAMES`] are reused before they're resolved again.
pub const DENY_NAMES_TTL: Duration = Duration::from_secs(30);
/// After a name of [`DENY_NAMES`] failed to resolve, how soon it's asked again.
const DENY_NAMES_RETRY: Duration = Duration::from_secs(5);
/// How long a name a tenant gave may take to resolve.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Why a connection is refused while some of the node's own names haven't answered yet (the
/// names aren't said: they're roll's, not the caller's).
pub const NOT_READY: &str = "roll hasn't resolved its own addresses yet; try again shortly";

/// Why a connection was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// A private address (written out, or every address the name resolves to), and the
    /// feature's `allow_private` is off. Callers say which setting lifts it.
    Private(String),
    /// An address the node denies ([`DENY_ADDRESSES`], [`DENY_NAMES`]).
    Denied(String),
    /// The node's own names haven't all answered yet ([`NOT_READY`]).
    NotReady,
    /// The name doesn't resolve (or didn't in time).
    Unresolved(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Private(t) => write!(f, "[{t}] is a private address"),
            Refused::Denied(t) => write!(f, "[{t}] is an address roll may not reach"),
            Refused::NotReady => f.write_str(NOT_READY),
            Refused::Unresolved(t) => write!(f, "[{t}] doesn't resolve"),
        }
    }
}

impl std::error::Error for Refused {}

/// An address or network (`10.0.0.0/8`, `2001:db8::/32`, `203.0.113.7`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Net {
    addr: IpAddr,
    prefix: u8,
}

impl Net {
    pub fn parse(s: &str) -> Option<Net> {
        let (a, p) = s.trim().split_once('/').map_or((s.trim(), None), |(a, p)| (a, Some(p)));
        let addr: IpAddr = a.trim_start_matches('[').trim_end_matches(']').parse().ok()?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            Some(p) => p.parse::<u8>().ok().filter(|p| *p <= max)?,
            None => max,
        };
        // An IPv4-mapped network is that IPv4 network (its prefix less the 96 bits before it);
        // any other IPv6 network stays one (`2002::/16` is all of 6to4).
        if let IpAddr::V6(v) = addr
            && let Some(v4) = v.to_ipv4_mapped()
            && prefix >= 96
        {
            return Some(Net { addr: IpAddr::V4(v4), prefix: prefix - 96 });
        }
        Some(Net { addr, prefix })
    }

    /// Whether `ip` is in the network as written, or the IPv4 address it carries is.
    pub fn contains(&self, ip: IpAddr) -> bool {
        self.holds(ip) || { let v4 = unwrap_v4(ip); v4 != ip && self.holds(v4) }
    }

    fn holds(&self, ip: IpAddr) -> bool {
        let mask = |bits: u32, len: u8| if len == 0 { 0 } else { u128::MAX << (bits - len as u32) };
        match (self.addr, ip) {
            (IpAddr::V4(n), IpAddr::V4(i)) => {
                let m = mask(32, self.prefix) as u32;
                u32::from(n) & m == u32::from(i) & m
            }
            (IpAddr::V6(n), IpAddr::V6(i)) => {
                let m = mask(128, self.prefix);
                u128::from(n) & m == u128::from(i) & m
            }
            _ => false,
        }
    }
}

/// The IPv4 address an IPv6 one carries, if it carries one, as `roll_net::private_ip` reads it:
/// mapped (`::ffff:a.b.c.d`), NAT64 (`64:ff9b::/96` and the local-use `64:ff9b:1::/48`), 6to4
/// (`2002::/16`) and IPv4-compatible (`::a.b.c.d`); any other address as it is.
fn unwrap_v4(ip: IpAddr) -> IpAddr {
    let IpAddr::V6(v) = ip else { return ip };
    if let Some(v4) = v.to_ipv4_mapped() {
        return IpAddr::V4(v4);
    }
    let s = v.segments();
    let embedded = |hi: u16, lo: u16| IpAddr::V4(std::net::Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8));
    if s[0] == 0x64 && s[1] == 0xff9b && (s[2..6] == [0, 0, 0, 0] || s[2] == 1) {
        return embedded(s[6], s[7]);
    }
    if s[0] == 0x2002 {
        return embedded(s[1], s[2]);
    }
    if s[..6] == [0, 0, 0, 0, 0, 0] && (s[6] != 0 || s[7] > 1) {
        return embedded(s[6], s[7]);
    }
    ip
}

/// A host as lists match it: lower case, no trailing dot.
pub(crate) fn normal_host(h: &str) -> String {
    h.to_ascii_lowercase().trim_end_matches('.').to_string()
}

/// A list setting as node settings hold it: `a,b` or `[a, b]`.
pub(crate) fn list(v: &str) -> Vec<String> {
    v.trim().trim_start_matches('[').trim_end_matches(']').split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_string).collect()
}

/// What [`DENY_NAMES`] resolved to lately, by the joined names.
struct DenyEntry {
    at: std::time::Instant,
    addrs: Arc<Vec<IpAddr>>,
    /// The names that have answered at least once (with addresses, or "no such name").
    answered: std::collections::HashSet<String>,
}

type DenyCache = parking_lot::Mutex<std::collections::HashMap<String, DenyEntry>>;

fn deny_cache() -> &'static DenyCache {
    static C: std::sync::OnceLock<DenyCache> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// What `names` resolved to last (any age), without resolving.
fn cached_denied(names: &[String]) -> Arc<Vec<IpAddr>> {
    deny_cache().lock().get(&names.join(",")).map(|e| e.addrs.clone()).unwrap_or_default()
}

/// The public addresses `names` resolve to now (at most [`DENY_NAMES_TTL`] old). Private ones are
/// refused anyway unless private hosts are allowed, which is local development. Until every name
/// has answered once (an address, or "no such name"), it fails closed, and the names are asked
/// again soon.
async fn denied_now(names: &[String]) -> Result<Arc<Vec<IpAddr>>, Refused> {
    if names.is_empty() {
        return Ok(Arc::default());
    }
    let key = names.join(",");
    let complete = |e: &DenyEntry| names.iter().all(|n| e.answered.contains(n));
    if let Some(e) = deny_cache().lock().get(&key)
        && e.at.elapsed() < DENY_NAMES_TTL
    {
        return if complete(e) { Ok(e.addrs.clone()) } else { Err(Refused::NotReady) };
    }
    let mut out = Vec::new();
    let mut answered = std::collections::HashSet::new();
    let mut failed = false;
    for n in names {
        if let Ok(ip) = n.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
            out.extend(Some(unwrap_v4(ip)).filter(|ip| !roll_net::private_ip(*ip)));
            answered.insert(n.clone());
            continue;
        }
        match tokio::time::timeout(Duration::from_secs(2), tokio::net::lookup_host((n.as_str(), 0))).await {
            Ok(Ok(addrs)) => {
                out.extend(addrs.map(|a| unwrap_v4(a.ip())).filter(|ip| !roll_net::private_ip(*ip)));
                answered.insert(n.clone());
            }
            // No such name (nothing to deny for it) is an answer; a resolver that failed
            // (SERVFAIL, a timeout, no network) isn't.
            Ok(Err(_)) => {
                if no_such_name(n).await {
                    answered.insert(n.clone());
                }
                failed = true;
            }
            // No answer at all.
            Err(_) => failed = true,
        }
    }
    let now = std::time::Instant::now();
    let mut at = now;
    let mut cache = deny_cache().lock();
    if let Some(prev) = cache.get(&key) {
        if failed {
            // A name that didn't resolve keeps the addresses it had (never fewer than before).
            out.extend(prev.addrs.iter().copied());
        }
        answered.extend(prev.answered.iter().cloned());
    }
    let entry = DenyEntry { at, addrs: Arc::default(), answered };
    if failed || !complete(&entry) {
        // Asked again soon.
        at = now.checked_sub(DENY_NAMES_TTL.saturating_sub(DENY_NAMES_RETRY)).unwrap_or(now);
    }
    out.sort();
    out.dedup();
    let entry = DenyEntry { at, addrs: Arc::new(out), ..entry };
    let r = if complete(&entry) { Ok(entry.addrs.clone()) } else { Err(Refused::NotReady) };
    cache.insert(key, entry);
    r
}

/// Whether the resolver answered that `name` has no addresses: NXDOMAIN, or a name without A
/// or AAAA records (NODATA). `lookup_host`'s error doesn't tell that from a resolver that failed,
/// so the name is asked again through hickory; anything but those two answers is a failure.
async fn no_such_name(name: &str) -> bool {
    use hickory_resolver::net::{DnsError, NetError, NoRecords};
    use hickory_resolver::proto::op::ResponseCode;
    static R: std::sync::OnceLock<Option<hickory_resolver::TokioResolver>> = std::sync::OnceLock::new();
    let Some(r) = R.get_or_init(|| hickory_resolver::TokioResolver::builder_tokio().ok().and_then(|b| b.build().ok())) else {
        return false;
    };
    matches!(
        tokio::time::timeout(Duration::from_secs(2), r.lookup_ip(name)).await,
        Ok(Err(NetError::Dns(DnsError::NoRecordsFound(NoRecords { response_code: ResponseCode::NXDomain | ResponseCode::NoError, .. }))))
    )
}

/// The node's egress rules for one feature: its own `allow_private`, the node's deny lists.
#[derive(Clone, Debug)]
pub struct Egress {
    allow_private: bool,
    deny: Arc<Vec<Net>>,
    deny_names: Arc<Vec<String>>,
}

impl Egress {
    /// The rules from node settings (`setting(key)`), with the feature's `allow_private`.
    pub fn of<'a>(setting: impl Fn(&str) -> Option<&'a str>, allow_private: bool) -> Result<Egress, String> {
        let both = |new: &str, old: &str| [setting(new), setting(old)].into_iter().flatten().flat_map(list).collect::<Vec<_>>();
        let mut deny = Vec::new();
        for n in both(DENY_ADDRESSES, OLD_DENY_ADDRESSES) {
            deny.push(Net::parse(&n).ok_or_else(|| format!("[{DENY_ADDRESSES}] holds [{n}], which isn't an address or network"))?);
        }
        let mut deny_names: Vec<String> = both(DENY_NAMES, OLD_DENY_NAMES).iter().map(|n| normal_host(n)).collect();
        deny_names.dedup();
        Ok(Egress { allow_private, deny: Arc::new(deny), deny_names: Arc::new(deny_names) })
    }

    /// The rules for a feature of `cluster` whose node setting `allow_private_key` lifts the
    /// private-address check. Settings that don't parse deny everything rather than nothing.
    pub fn of_cluster(cluster: &crate::Cluster, allow_private_key: &str) -> Egress {
        let allow_private = cluster.config.node_setting(allow_private_key).is_some_and(|v| v.eq_ignore_ascii_case("true"));
        Egress::of(|k| cluster.config.node_setting(k), allow_private).unwrap_or_else(|_| Egress::deny_all())
    }

    /// Rules that let nothing out (node settings that don't parse).
    fn deny_all() -> Egress {
        let all = [Net { addr: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), prefix: 0 }, Net { addr: IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), prefix: 0 }];
        Egress { allow_private: false, deny: Arc::new(all.to_vec()), deny_names: Arc::default() }
    }

    pub fn allow_private(&self) -> bool {
        self.allow_private
    }

    fn denied(&self, ip: IpAddr, own: &[IpAddr]) -> bool {
        self.deny.iter().any(|n| n.contains(ip)) || own.contains(&unwrap_v4(ip))
    }

    /// `host` (a URL's host: a name, an address, `[v6]`) when it's an address written out, or
    /// `localhost`: checked against private ranges, the deny list and what the node's names
    /// resolved to last. Names are checked when they resolve ([`Egress::resolver`]).
    pub fn check_literal(&self, host: &str) -> Result<(), Refused> {
        self.literal(host, &cached_denied(&self.deny_names))
    }

    /// [`Egress::check_literal`], with the node's names resolved now (again if stale), failing
    /// closed until they've answered: for callers about to connect.
    pub async fn check_literal_now(&self, host: &str) -> Result<(), Refused> {
        let own = denied_now(&self.deny_names).await?;
        self.literal(host, &own)
    }

    /// [`Egress::check_literal_now`] for a URL's host.
    pub async fn check_url_now(&self, url: &reqwest::Url) -> Result<(), Refused> {
        self.check_url(url)?;
        self.check_literal_now(url.host_str().unwrap_or_default()).await
    }

    /// [`Egress::check_literal`] for a URL's host.
    pub fn check_url(&self, url: &reqwest::Url) -> Result<(), Refused> {
        let host = url.host_str().unwrap_or_default();
        if !self.allow_private && roll_net::literal_private_host(url) {
            return Err(Refused::Private(host.to_string()));
        }
        self.check_literal(host)
    }

    fn literal(&self, host: &str, own: &[IpAddr]) -> Result<(), Refused> {
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        let h = normal_host(bare);
        if !self.allow_private && (h == "localhost" || h.ends_with(".localhost")) {
            return Err(Refused::Private(host.to_string()));
        }
        let Ok(ip) = bare.parse::<IpAddr>() else { return Ok(()) };
        if !self.allow_private && roll_net::private_ip(ip) {
            return Err(Refused::Private(host.to_string()));
        }
        if self.denied(ip, own) {
            return Err(Refused::Denied(host.to_string()));
        }
        Ok(())
    }

    /// The addresses `host:port` may be reached at, in the resolver's order: an address written
    /// out checked as [`Egress::check_literal_now`] does, a name resolved and every address it
    /// gives checked. Connect to one of these, never to the name.
    pub async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, Refused> {
        let own = denied_now(&self.deny_names).await?;
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        if bare.parse::<IpAddr>().is_ok() || normal_host(bare) == "localhost" || normal_host(bare).ends_with(".localhost") {
            self.literal(host, &own)?;
        }
        let addrs: Vec<SocketAddr> = match bare.parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, port)],
            Err(_) => tokio::time::timeout(LOOKUP_TIMEOUT, tokio::net::lookup_host((bare, port)))
                .await
                .map_err(|_| Refused::Unresolved(host.to_string()))?
                .map_err(|_| Refused::Unresolved(host.to_string()))?
                .collect(),
        };
        self.filter(host, addrs, &own)
    }

    fn filter(&self, host: &str, addrs: Vec<SocketAddr>, own: &[IpAddr]) -> Result<Vec<SocketAddr>, Refused> {
        if addrs.is_empty() {
            return Err(Refused::Unresolved(host.to_string()));
        }
        let private = addrs.iter().any(|a| !self.allow_private && roll_net::private_ip(a.ip()));
        let ok: Vec<SocketAddr> = addrs.into_iter().filter(|a| (self.allow_private || !roll_net::private_ip(a.ip())) && !self.denied(a.ip(), own)).collect();
        match ok.is_empty() {
            false => Ok(ok),
            true if private => Err(Refused::Private(host.to_string())),
            true => Err(Refused::Denied(host.to_string())),
        }
    }

    /// A `reqwest` resolver applying these rules to every address a name resolves to.
    pub fn resolver(&self) -> Arc<Egress> {
        Arc::new(self.clone())
    }
}

impl reqwest::dns::Resolve for Egress {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let me = self.clone();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let own = denied_now(&me.deny_names).await?;
            let ok = me.filter(&host, addrs, &own).map_err(|r| match r {
                // As `roll_net::PublicOnly` says it: what transport errors are matched on.
                Refused::Private(h) => format!("{h} resolves to a private address"),
                Refused::Denied(h) => format!("{h} resolves to an address roll may not reach"),
                other => other.to_string(),
            })?;
            Ok(Box::new(ok.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One check for every feature: a denied network, a denied literal in another spelling,
    /// private literals unless allowed, and IPv4 inside IPv6.
    #[tokio::test]
    async fn literals_are_checked_against_the_deny_list_and_private_ranges() {
        let settings = [(DENY_ADDRESSES, "1.2.3.0/24"), (OLD_DENY_ADDRESSES, "9.9.9.9")];
        let get = |k: &str| settings.iter().find(|(n, _)| *n == k).map(|(_, v)| *v);
        let e = Egress::of(get, false).unwrap();
        assert_eq!(e.check_literal("1.2.3.9"), Err(Refused::Denied("1.2.3.9".into())));
        assert_eq!(e.check_literal("[::ffff:9.9.9.9]"), Err(Refused::Denied("[::ffff:9.9.9.9]".into())), "the old setting's name is read too");
        assert_eq!(e.check_literal("127.0.0.1"), Err(Refused::Private("127.0.0.1".into())));
        assert_eq!(e.check_literal("LOCALHOST."), Err(Refused::Private("LOCALHOST.".into())));
        assert_eq!(e.check_literal("db.example.com"), Ok(()));
        assert!(matches!(e.resolve("1.2.3.9", 5432).await, Err(Refused::Denied(_))));
        let dev = Egress::of(get, true).unwrap();
        assert_eq!(dev.check_literal("127.0.0.1"), Ok(()));
        assert_eq!(dev.check_literal("1.2.3.9"), Err(Refused::Denied("1.2.3.9".into())), "allowing private hosts never lifts the deny list");
        assert!(Egress::of(|k| (k == DENY_ADDRESSES).then_some("nope"), false).is_err());
    }

    #[test]
    fn ipv4_inside_ipv6_is_denied_as_ipv4() {
        let n = Net::parse("203.0.113.0/24").unwrap();
        for ip in ["::ffff:203.0.113.9", "64:ff9b::cb00:7109", "2002:cb00:7109::1", "203.0.113.9"] {
            assert!(n.contains(ip.parse().unwrap()), "{ip}");
        }
        assert!(!n.contains("2001:db8::1".parse().unwrap()));
        // Networks keep their own size: 6to4 as a whole, a mapped IPv4 network as that network.
        let six_to_four = Net::parse("2002::/16").unwrap();
        assert!(six_to_four.contains("2002:cb00:7109::1".parse().unwrap()) && six_to_four.contains("2002:a00:1::1".parse().unwrap()));
        assert!(!six_to_four.contains("203.0.113.9".parse().unwrap()));
        let mapped = Net::parse("::ffff:203.0.113.0/120").unwrap();
        assert!(mapped.contains("203.0.113.200".parse().unwrap()) && !mapped.contains("203.0.114.1".parse().unwrap()));
    }
}
