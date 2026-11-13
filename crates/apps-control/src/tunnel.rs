//! `GET …/environments/{env}/tunnel` (a WebSocket): the bytes of one TCP connection to a web
//! service's port, so `roll apps proxy` reaches a private service from a person's own machine
//! (Fly's `fly proxy`). It wakes an instance and holds it awake (a lease, as a request does) for
//! as long as the tunnel is open; binary messages are the connection's bytes both ways, an
//! empty one the end of a direction (TCP's half-close: a request sent, its answer still to
//! come), and the tunnel closes once both have ended. Only the service's own port.

use crate::live::Lease;
use crate::{Apps, Error, Result};
use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Tunnels one environment may have open at once.
pub const MAX_TUNNELS: usize = 20;

/// A tunnel's place in its environment's count, given back when it ends.
pub struct TunnelSlot(Arc<AtomicUsize>);

impl Drop for TunnelSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Apps {
    pub fn tunnel_slot(&self, service: &str, env: &str) -> Result<TunnelSlot> {
        let count = self.tunnels.lock().entry(crate::env_key(service, env)).or_default().clone();
        let slot = TunnelSlot(count.clone());
        if count.fetch_add(1, Ordering::SeqCst) + 1 > MAX_TUNNELS {
            return Err(Error::Conflict(format!("{MAX_TUNNELS} tunnels are open to {env}: close one first")));
        }
        Ok(slot)
    }

    /// An instance of the environment's web service, woken if need be, held while the lease is.
    pub async fn tunnel_target(self: &Arc<Self>, service: &str, env: &str) -> Result<Lease> {
        let live = self.load_live(service, env).await?;
        let Some(tpl) = live.template() else { return Err(Error::Conflict(format!("{env} has no deployment yet"))) };
        if tpl.run.kind != crate::model::RunType::Web || tpl.site.is_some() {
            return Err(Error::Conflict("only a web service that runs an instance has a port to reach".into()));
        }
        live.may_wake().map_err(|e| Error::Conflict(format!("the service can't wake: {e}")))?;
        live.acquire_tpl(tpl).await.map_err(|e| Error::Conflict(format!("the service didn't wake: {e}")))
    }

    /// Copy between the WebSocket and the instance's port until either side closes.
    pub async fn bridge_tunnel(self: Arc<Self>, socket: WebSocket, lease: Lease, slot: TunnelSlot) {
        let _slot = slot;
        let (mut tx, mut rx) = socket.split();
        let tcp = match tokio::net::TcpStream::connect(lease.addr).await {
            Ok(t) => t,
            Err(e) => {
                let _ = tx.send(Message::Close(Some(axum::extract::ws::CloseFrame { code: 1011, reason: format!("the service's port didn't answer: {e}").into() }))).await;
                return;
            }
        };
        let _ = tcp.set_nodelay(true);
        let (mut rd, mut wr) = tcp.into_split();
        let mut buf = vec![0u8; 64 << 10];
        let (mut from_client, mut from_app) = (true, true);
        while from_client || from_app {
            tokio::select! {
                m = rx.next(), if from_client => match m {
                    // The client is done sending: the app sees its end, and may still answer.
                    Some(Ok(Message::Binary(b))) if b.is_empty() => {
                        let _ = wr.shutdown().await;
                        from_client = false;
                    }
                    Some(Ok(Message::Binary(b))) => {
                        if wr.write_all(&b).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                },
                n = rd.read(&mut buf), if from_app => match n {
                    Ok(0) | Err(_) => {
                        if tx.send(Message::Binary(bytes::Bytes::new())).await.is_err() {
                            break;
                        }
                        from_app = false;
                    }
                    Ok(n) => {
                        if tx.send(Message::Binary(bytes::Bytes::copy_from_slice(&buf[..n]))).await.is_err() {
                            break;
                        }
                    }
                },
            }
        }
        let _ = wr.shutdown().await;
        let _ = tx.send(Message::Close(None)).await;
        drop(lease);
    }
}
