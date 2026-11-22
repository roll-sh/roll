//! UDP services (`run.udp`, Fly's UDP services: a game server, DNS, WebRTC's media): a service
//! whose port speaks UDP answers each environment on a UDP port of its own from the node's range
//! (`ROLL_APPS_UDP_PORTS`, one node). Each client address is a flow: its first datagram is let in
//! by the environment's allow list, maintenance and rate limit (flows per client), as a TCP
//! connection is, wakes an instance and holds it (a lease) while the flow lives; its datagrams go
//! to the instance's port from a socket of the flow's own, and the instance's answers back to the
//! client from the environment's port. A flow ends after [`IDLE`] without a datagram either way.
//! Every port is the owner's; a node binds only the services it owns. A closed environment has no
//! UDP port (its sandbox carries only its own sockets).

use crate::{Apps, model::RunType};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

/// A flow without a datagram either way this long ends (and lets its instance sleep).
pub const IDLE: Duration = Duration::from_secs(60);
/// The most flows an environment's port relays at once; a new client past it is dropped.
pub const MAX_FLOWS: usize = 1024;
/// Datagrams held for a flow while its instance wakes, at most; past it they're dropped, as a
/// network would.
const QUEUE: usize = 64;

impl Apps {
    /// The UDP services' ports this node should listen on: `(port, service, env)`, a port given
    /// to each environment that has none yet. None: the store couldn't say.
    pub async fn udp_endpoints(self: &Arc<Self>) -> Option<Vec<(u16, String, String)>> {
        let Some(range) = self.cfg.udp_ports else { return Some(Vec::new()) };
        let services = self.db(|s| s.services(None)).await.ok()?;
        let mut out = Vec::new();
        // Public only, and open: UDP carries no token, and a closed sandbox no UDP.
        for svc in services.into_iter().filter(|s| s.run.udp && s.run.kind == RunType::Web && s.run.public && self.owns(&s.id)) {
            let sid = svc.id.clone();
            let envs = self.db(move |s| s.environments(&sid)).await.ok()?;
            for e in envs.into_iter().filter(|e| !e.egress.closed()) {
                let (sid, en) = (svc.id.clone(), e.name.clone());
                if !matches!(self.db(move |s| s.access(&sid, &en)).await, Ok(None)) {
                    continue;
                }
                let (sid, en) = (svc.id.clone(), e.name.clone());
                match self.db(move |s| s.assign_udp_port(&sid, &en, range)).await {
                    Ok(Some(p)) => out.push((p, svc.id.clone(), e.name.clone())),
                    Ok(None) => tracing::warn!(service = %svc.id, env = %e.name, "apps: no UDP port left in ROLL_APPS_UDP_PORTS"),
                    Err(err) => {
                        tracing::warn!(service = %svc.id, env = %e.name, error = %err, "apps: a UDP port couldn't be given");
                        return None;
                    }
                }
            }
        }
        Some(out)
    }

    /// Listen on every UDP service's port this node owns, binding and closing them as services
    /// come and go (every 2 s), on `host`.
    pub async fn serve_udp_ports(self: Arc<Self>, host: IpAddr) {
        if self.cfg.udp_ports.is_none() {
            return;
        }
        let mut bound: HashMap<u16, ((String, String), tokio::task::JoinHandle<()>)> = HashMap::new();
        loop {
            if let Some(endpoints) = self.udp_endpoints().await {
                let want: HashMap<u16, (String, String)> = endpoints.into_iter().map(|(p, s, e)| (p, (s, e))).collect();
                bound.retain(|p, (who, h)| {
                    let keep = want.get(p) == Some(who);
                    if !keep {
                        h.abort();
                    }
                    keep
                });
                for (port, who) in want {
                    if bound.contains_key(&port) {
                        continue;
                    }
                    match UdpSocket::bind((host, port)).await {
                        Ok(s) => {
                            let (svc, env) = who.clone();
                            // Its flows live in the relay's task: closing the port ends them too.
                            bound.insert(port, (who, tokio::spawn(self.clone().relay_udp(svc, env, Arc::new(s)))));
                        }
                        Err(e) => tracing::warn!(port, error = %e, "apps: a UDP service's port couldn't be bound"),
                    }
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                _ = self.stop.cancelled() => {
                    for (_, (_, h)) in bound {
                        h.abort();
                    }
                    return;
                }
            }
        }
    }

    /// An environment's UDP port: each client's datagrams to its flow, a new flow for a new client.
    pub async fn relay_udp(self: Arc<Self>, service: String, env: String, sock: Arc<UdpSocket>) {
        let mut flows: HashMap<SocketAddr, tokio::sync::mpsc::Sender<Vec<u8>>> = HashMap::new();
        let mut tasks = tokio::task::JoinSet::new();
        let mut buf = vec![0u8; 65_535];
        loop {
            let (n, peer) = tokio::select! {
                r = sock.recv_from(&mut buf) => match r {
                    Ok(x) => x,
                    // An ICMP error from an earlier send: the socket goes on.
                    Err(_) => continue,
                },
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {
                    flows.retain(|_, tx| !tx.is_closed());
                    continue;
                }
            };
            let datagram = buf[..n].to_vec();
            if let Some(tx) = flows.get(&peer).filter(|tx| !tx.is_closed()) {
                // A full queue (its instance still waking) drops it, as a network would.
                let _ = tx.try_send(datagram);
                continue;
            }
            flows.retain(|_, tx| !tx.is_closed());
            if flows.len() >= MAX_FLOWS || !self.udp_admits(&service, &env, peer).await {
                continue;
            }
            let (tx, rx) = tokio::sync::mpsc::channel(QUEUE);
            let _ = tx.try_send(datagram);
            flows.insert(peer, tx);
            tasks.spawn(self.clone().udp_flow(service.clone(), env.clone(), peer, rx, sock.clone()));
        }
    }

    /// Whether a new client's flow gets in: its allow list, maintenance, and its rate limit
    /// counting flows per client (as it counts connections over TCP).
    async fn udp_admits(self: &Arc<Self>, service: &str, env: &str, peer: SocketAddr) -> bool {
        let live = self.live(service, env);
        let Ok(gate) = self.gate(&live).await else { return false };
        let ip = peer.ip().to_string();
        if !gate.admits(Some(&ip)) || gate.maintenance.is_some() {
            return false;
        }
        gate.rate.as_ref().is_none_or(|limit| live.take_token(crate::live::client_ip(Some(&ip)), limit).is_ok())
    }

    /// One client's flow: an instance woken and held, its datagrams relayed both ways until
    /// [`IDLE`] passes without one.
    async fn udp_flow(self: Arc<Self>, service: String, env: String, peer: SocketAddr, mut rx: tokio::sync::mpsc::Receiver<Vec<u8>>, out: Arc<UdpSocket>) {
        let Ok(live) = self.load_live(&service, &env).await else { return };
        let Some(tpl) = live.template().filter(|t| t.run.udp) else { return };
        let Some(port) = tpl.run.port else { return };
        if live.may_wake().is_err() {
            return;
        }
        let began = Instant::now();
        let Ok(lease) = live.acquire_tpl(tpl).await else { return };
        let Some(addr) = self.deps.runtime.udp_address(&lease.inst.name, port).await else {
            live.connect_failed(&lease.inst);
            return;
        };
        let Ok(up) = UdpSocket::bind(SocketAddr::new(if addr.is_ipv4() { IpAddr::from([127, 0, 0, 1]) } else { IpAddr::from([0u16; 8]) }, 0)).await else { return };
        if up.connect(addr).await.is_err() {
            return;
        }
        let reached = began.elapsed();
        let (mut bytes, mut buf) = (0u64, vec![0u8; 65_535]);
        loop {
            tokio::select! {
                d = rx.recv() => match d {
                    Some(d) => {
                        bytes += d.len() as u64;
                        let _ = up.send(&d).await;
                    }
                    None => break,
                },
                r = up.recv(&mut buf) => match r {
                    Ok(n) => {
                        bytes += n as u64;
                        let _ = out.send_to(&buf[..n], peer).await;
                    }
                    // The instance's port refused it (it went): the flow ends.
                    Err(_) => break,
                },
                _ = tokio::time::sleep(IDLE) => break,
            }
        }
        // Metered as a request that took as long as reaching the app did, as a TCP connection is.
        live.meter.lock().request(code_sql::now_ms(), reached.as_secs_f64() * 1000.0, 200, bytes);
        drop(lease);
    }
}
