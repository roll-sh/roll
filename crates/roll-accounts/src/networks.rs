//! Allowed networks (docs/accounts.md, "Allowed networks"), as GitHub Enterprise's IP allow list:
//! an admin lists the company's networks, and a person's session is used only from them (rolld's
//! gate asks on every request, `roll_auth::Sessions::network_ok`). The operator's token and roll
//! tokens keep their own lists. Saving a list that leaves out the saving admin's own address is
//! refused: nobody locks themselves out by accident.

use crate::{Accounts, Error, Result};
use code_sql::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

/// The most networks a list holds.
const MAX_NETWORKS: usize = 100;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NetworkPolicy {
    /// Only these networks.
    #[serde(default)]
    pub on: bool,
    /// CIDRs (`203.0.113.0/24`, `2001:db8::/32`) or single addresses.
    #[serde(default)]
    pub networks: Vec<String>,
}

fn parse(n: &str) -> Option<ipnet::IpNet> {
    let n = n.trim();
    n.parse::<ipnet::IpNet>().ok().or_else(|| n.parse::<IpAddr>().ok().map(ipnet::IpNet::from)).map(|n| n.trunc())
}

impl NetworkPolicy {
    fn checked(mut self) -> Result<Self> {
        if self.networks.len() > MAX_NETWORKS {
            return Err(Error::BadRequest(format!("at most {MAX_NETWORKS} networks"), Some("networks")));
        }
        let mut out = Vec::new();
        for n in &self.networks {
            let net = parse(n).ok_or_else(|| Error::BadRequest(format!("{n:?} isn't a network (203.0.113.0/24) or an address"), Some("networks")))?;
            let s = net.to_string();
            if !out.contains(&s) {
                out.push(s);
            }
        }
        if self.on && out.is_empty() {
            return Err(Error::BadRequest("turning it on needs at least one network".into(), Some("networks")));
        }
        self.networks = out;
        Ok(self)
    }

    /// Whether `ip` is on the list (always, when it's off; never an unknown address when on).
    pub fn allows(&self, ip: Option<IpAddr>) -> bool {
        if !self.on {
            return true;
        }
        let Some(ip) = ip.map(|i| i.to_canonical()) else { return false };
        self.networks.iter().filter_map(|n| parse(n)).any(|n| n.contains(&ip))
    }
}

impl Accounts {
    pub fn network_policy(&self) -> Result<NetworkPolicy> {
        let raw = self.db.read(|c| c.query_row("SELECT policy FROM network_policy WHERE id = 1", (), |r| r.get::<_, String>(0)).optional())?;
        Ok(raw.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
    }

    /// An admin sets the list; `from` is their own address (an admin's session), which must be on
    /// it: they'd be locked out otherwise. The operator's token has no session to lock out.
    pub fn set_network_policy(&self, p: NetworkPolicy, from: Option<IpAddr>) -> Result<NetworkPolicy> {
        let p = p.checked()?;
        if let Some(ip) = from
            && !p.allows(Some(ip))
        {
            return Err(Error::BadRequest(format!("you're signed in from {ip}, which isn't on the list: add it, or you'd be locked out"), Some("lockout")));
        }
        let s = serde_json::to_string(&p).map_err(|e| Error::Internal(e.to_string()))?;
        self.db.write(|c| c.execute("INSERT INTO network_policy (id, policy) VALUES (1, ?) ON CONFLICT (id) DO UPDATE SET policy = excluded.policy", [s.as_str()]))?;
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_hold_networks_and_addresses() {
        let p = NetworkPolicy { on: true, networks: vec!["203.0.113.7/24".into(), "198.51.100.9".into(), "2001:db8::/32".into()] }.checked().unwrap();
        assert_eq!(p.networks, ["203.0.113.0/24", "198.51.100.9/32", "2001:db8::/32"]);
        assert!(p.allows(Some("203.0.113.200".parse().unwrap())) && p.allows(Some("2001:db8::1".parse().unwrap())));
        assert!(p.allows(Some("::ffff:198.51.100.9".parse().unwrap())), "an IPv4 address as IPv6");
        assert!(!p.allows(Some("192.0.2.1".parse().unwrap())) && !p.allows(None));
        assert!(NetworkPolicy { on: true, networks: vec![] }.checked().is_err());
        assert!(NetworkPolicy { on: false, networks: vec!["nope".into()] }.checked().is_err());
        assert!(NetworkPolicy::default().allows(None), "off: anywhere");
    }
}
