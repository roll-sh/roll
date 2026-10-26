//! Outbound connections (dblink, postgres_fdw): an operator's allow-list.
//!
//! In roll, a tenant's database dialing arbitrary hosts would be a server-side request
//! forgery and credential risk, so outbound connections are refused unless the server was
//! started with `--allow-outbound <list>` (`EngineOptions::outbound`). PostgreSQL's own rules
//! (password_required, non-superusers authenticating with a password) apply on top.
//!
//! An entry of the list is
//! - `local`: the engine itself, reached without a host (libpq's default Unix socket, which
//!   here stands for the engine's own listener);
//! - `host` or `host:port` (`[v6addr]:port`): a host name or IP address as the connection
//!   names it (`host` / `hostaddr`), case-insensitive; `*` matches any host;
//! - `addr/bits`: an IPv4 / IPv6 network (CIDR), matching `hostaddr` or the addresses a host
//!   name resolves to (every one must match).

use std::net::IpAddr;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Host {
    Any,
    Name(String),
    Net(IpAddr, u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Rule {
    host: Host,
    port: Option<u16>,
}

/// The parsed allow-list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outbound {
    local: bool,
    rules: Vec<Rule>,
}

impl Outbound {
    /// Parse a list of entries (comma or whitespace separated).
    pub fn parse(list: &[String]) -> Result<Outbound, String> {
        let mut out = Outbound::default();
        for e in list.iter().flat_map(|s| s.split([',', ' ', '\t', '\n'])).filter(|s| !s.is_empty()) {
            if e.eq_ignore_ascii_case("local") {
                out.local = true;
                continue;
            }
            let (host, port) = split_host_port(e)?;
            let host = if host == "*" {
                Host::Any
            } else if let Some((a, bits)) = host.split_once('/') {
                let addr: IpAddr = a.parse().map_err(|_| format!("invalid network \"{e}\""))?;
                let max = if addr.is_ipv4() { 32 } else { 128 };
                let bits: u8 = bits.parse().ok().filter(|b| *b <= max).ok_or_else(|| format!("invalid network \"{e}\""))?;
                Host::Net(addr, bits)
            } else {
                Host::Name(host.to_ascii_lowercase())
            };
            out.rules.push(Rule { host, port });
        }
        Ok(out)
    }

    pub fn allows_local(&self) -> bool {
        self.local
    }

    /// Whether a connection to `host` (as named) at `port`, resolving to `addrs`, is allowed.
    pub fn allows(&self, host: &str, addrs: &[IpAddr], port: u16) -> bool {
        let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
        self.rules.iter().any(|r| {
            r.port.is_none_or(|p| p == port)
                && match &r.host {
                    Host::Any => true,
                    Host::Name(n) => *n == host,
                    Host::Net(net, bits) => !addrs.is_empty() && addrs.iter().all(|a| in_net(*a, *net, *bits)),
                }
        })
    }
}

fn split_host_port(e: &str) -> Result<(String, Option<u16>), String> {
    let bad = || format!("invalid outbound destination \"{e}\"");
    if let Some(r) = e.strip_prefix('[') {
        let (h, rest) = r.split_once(']').ok_or_else(bad)?;
        let port = match rest.strip_prefix(':') {
            Some(p) => Some(p.parse().map_err(|_| bad())?),
            None if rest.is_empty() => None,
            None => return Err(bad()),
        };
        return Ok((h.to_string(), port));
    }
    // A bare IPv6 address (several colons) has no port.
    if e.matches(':').count() > 1 {
        return Ok((e.to_string(), None));
    }
    match e.split_once(':') {
        Some((h, p)) if !h.is_empty() => Ok((h.to_string(), Some(p.parse().map_err(|_| bad())?))),
        Some(_) => Err(bad()),
        None => Ok((e.to_string(), None)),
    }
}

fn in_net(a: IpAddr, net: IpAddr, bits: u8) -> bool {
    match (a, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => {
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
            u32::from(a) & mask == u32::from(n) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(n)) => {
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
            u128::from(a) & mask == u128::from(n) & mask
        }
        (IpAddr::V6(a), IpAddr::V4(_)) => a.to_ipv4_mapped().is_some_and(|a4| in_net(IpAddr::V4(a4), net, bits)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Outbound {
        Outbound::parse(&[s.to_string()]).unwrap()
    }

    #[test]
    fn entries() {
        let o = p("local, db.example.com:5432 10.0.0.0/8 [::1]:6000");
        assert!(o.allows_local());
        assert!(o.allows("DB.example.com", &[], 5432));
        assert!(!o.allows("db.example.com", &[], 5433));
        assert!(o.allows("x", &["10.1.2.3".parse().unwrap()], 1));
        assert!(!o.allows("x", &["10.1.2.3".parse().unwrap(), "11.0.0.1".parse().unwrap()], 1));
        assert!(o.allows("::1", &[], 6000));
        assert!(!p("*:5432").allows_local());
        assert!(p("*").allows("anything", &[], 1));
        assert!(Outbound::parse(&["host:x".into()]).is_err());
        assert!(!Outbound::default().allows("h", &[], 5432));
    }
}
