//! TCP services (`run.tcp`, Railway's TCP proxy): a service whose port isn't HTTP (a game
//! server, MQTT, a database of its own) answers each environment on a port of its own from the
//! node's range (`ROLL_APPS_TCP_PORTS`, one node). A connection is let in by the environment's
//! allow list (its peer), refused in maintenance, wakes an instance and holds it (a lease, as a
//! request does) while it's open, and is spliced to the app's port; half-closes pass. Every
//! port is the owner's; a node binds only the services it owns.

use crate::{Apps, model::RunType};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

impl Apps {
    /// The TCP services' ports this node should listen on: `(port, service, env)`, a port
    /// given to each environment that has none yet. None: the store couldn't say (the
    /// listeners stay as they are).
    pub async fn tcp_endpoints(self: &Arc<Self>) -> Option<Vec<(u16, String, String)>> {
        let Some(range) = self.cfg.tcp_ports else { return Some(Vec::new()) };
        let services = self.db(|s| s.services(None)).await.ok()?;
        let mut out = Vec::new();
        // Public only: a private service, or an environment behind an access token (a preview),
        // has no TCP port (TCP carries no token); the private network and a tunnel reach it.
        for svc in services.into_iter().filter(|s| s.run.tcp && s.run.kind == RunType::Web && s.run.public && self.owns(&s.id)) {
            let sid = svc.id.clone();
            let envs = self.db(move |s| s.environments(&sid)).await.ok()?;
            for e in envs {
                let (sid, en) = (svc.id.clone(), e.name.clone());
                if !matches!(self.db(move |s| s.access(&sid, &en)).await, Ok(None)) {
                    continue;
                }
                let (sid, en) = (svc.id.clone(), e.name.clone());
                match self.db(move |s| s.assign_tcp_port(&sid, &en, range)).await {
                    Ok(Some(p)) => out.push((p, svc.id.clone(), e.name.clone())),
                    Ok(None) => tracing::warn!(service = %svc.id, env = %e.name, "apps: no TCP port left in ROLL_APPS_TCP_PORTS"),
                    Err(err) => {
                        tracing::warn!(service = %svc.id, env = %e.name, error = %err, "apps: a TCP port couldn't be given");
                        return None;
                    }
                }
            }
        }
        Some(out)
    }

    /// One connection to an environment's TCP port, until either side ends it.
    pub async fn serve_tcp(self: Arc<Self>, service: String, env: String, mut conn: tokio::net::TcpStream, peer: SocketAddr) {
        let live = self.live(&service, &env);
        let Ok(gate) = self.gate(&live).await else { return };
        if !gate.admits(Some(&peer.ip().to_string())) || gate.maintenance.is_some() {
            return;
        }
        let Ok(live) = self.load_live(&service, &env).await else { return };
        let Some(tpl) = live.template().filter(|t| t.run.tcp) else { return };
        if live.may_wake().is_err() {
            return;
        }
        let began = std::time::Instant::now();
        let Ok(lease) = live.acquire_tpl(tpl).await else { return };
        let Ok(mut app) = tokio::net::TcpStream::connect(lease.addr).await else {
            live.connect_failed(&lease.inst);
            return;
        };
        // Metered as a request that took as long as it took to reach the app (a connection's
        // lifetime isn't a latency).
        let reached = began.elapsed();
        let _ = (conn.set_nodelay(true), app.set_nodelay(true));
        let (up, down) = tokio::io::copy_bidirectional(&mut conn, &mut app).await.unwrap_or((0, 0));
        live.meter.lock().request(code_sql::now_ms(), reached.as_secs_f64() * 1000.0, 200, up + down);
        drop(lease);
    }

    /// Listen on every TCP service's port this node owns, binding and closing them as services
    /// come and go (every 2 s), on `host`.
    pub async fn serve_tcp_ports(self: Arc<Self>, host: IpAddr) {
        if self.cfg.tcp_ports.is_none() {
            return;
        }
        let mut bound: HashMap<u16, ((String, String), tokio::task::JoinHandle<()>)> = HashMap::new();
        loop {
            let Some(endpoints) = self.tcp_endpoints().await else {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            };
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
                match tokio::net::TcpListener::bind((host, port)).await {
                    Ok(l) => {
                        let (apps, (svc, env)) = (self.clone(), who.clone());
                        // Its connections live in the listener's task: closing the port (tcp off, the
                        // service gone) ends them too.
                        let h = tokio::spawn(async move {
                            let mut open = tokio::task::JoinSet::new();
                            loop {
                                tokio::select! {
                                    a = l.accept() => match a {
                                        Ok((conn, peer)) => {
                                            open.spawn(apps.clone().serve_tcp(svc.clone(), env.clone(), conn, peer));
                                        }
                                        Err(_) => break,
                                    },
                                    Some(_) = open.join_next(), if !open.is_empty() => {}
                                }
                            }
                        });
                        bound.insert(port, (who, h));
                    }
                    Err(e) => tracing::warn!(port, error = %e, "apps: a TCP service's port couldn't be bound"),
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
}
