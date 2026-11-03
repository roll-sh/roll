//! One daemon's WebSocket (docs/code/agents.md, "The relay protocol"): `hello` → `welcome`, then
//! entity batches (persisted, then acked and published), deltas (published only), replies to
//! commands, and pings both ways. 60 s without a frame from the daemon drops the link.

use super::store::{self, MachineRow};
use super::{Closer, Kick};
use crate::Code;
use agent_proto::{DaemonFrame, MachineId, PROTOCOL, RelayFrame, StreamEvent};
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket};
use std::sync::Arc;
use std::time::Duration;

const HELLO_WAIT: Duration = Duration::from_secs(10);
const PING_EVERY: Duration = Duration::from_secs(20);
const SILENCE: Duration = Duration::from_secs(60);

/// Close codes the daemon acts on: 4401 means the token no longer works (stop, don't retry).
pub const CLOSE_REVOKED: u16 = 4401;
const CLOSE_PROTOCOL: u16 = 4400;
const CLOSE_REPLACED: u16 = 4409;

async fn close(mut socket: WebSocket, code: u16, reason: &str) {
    let _ = socket.send(Message::Close(Some(CloseFrame { code, reason: Utf8Bytes::from(reason) }))).await;
}

fn text(f: &RelayFrame) -> Message {
    Message::Text(Utf8Bytes::from(serde_json::to_string(f).unwrap_or_default()))
}

/// Receive the next daemon frame. A transport ping or pong counts as a `pong` (the daemon is
/// alive). `None`: the socket ended.
async fn recv(socket: &mut WebSocket) -> Option<Result<DaemonFrame, String>> {
    match socket.recv().await? {
        Ok(Message::Text(t)) => Some(serde_json::from_str(t.as_str()).map_err(|e| format!("bad frame: {e}"))),
        Ok(Message::Binary(_)) => Some(Err("binary frames aren't part of the protocol".into())),
        Ok(Message::Close(_)) | Err(_) => None,
        Ok(Message::Ping(_) | Message::Pong(_)) => Some(Ok(DaemonFrame::Pong)),
    }
}

pub(crate) async fn serve(code: Arc<Code>, machine: MachineRow, mut socket: WebSocket) {
    let info = match tokio::time::timeout(HELLO_WAIT, recv(&mut socket)).await {
        Ok(Some(Ok(DaemonFrame::Hello { protocol, info }))) if protocol == PROTOCOL => info,
        Ok(Some(Ok(DaemonFrame::Hello { protocol, .. }))) => {
            return close(socket, CLOSE_PROTOCOL, &format!("protocol {protocol} isn't supported; this relay speaks {PROTOCOL}: update roll")).await;
        }
        Ok(Some(Ok(_))) => return close(socket, CLOSE_PROTOCOL, "the first frame must be hello").await,
        Ok(Some(Err(e))) => return close(socket, CLOSE_PROTOCOL, &e).await,
        Ok(None) => return,
        Err(_) => return close(socket, CLOSE_PROTOCOL, "no hello").await,
    };
    let owner = match code.db.owner_by_id(&machine.owner_id) {
        Ok(Some(o)) => o,
        Ok(None) => return close(socket, CLOSE_REVOKED, "the machine's owner is gone").await,
        Err(e) => {
            tracing::warn!(error = %e, "agent link: reading the owner");
            return close(socket, 1011, "try again").await;
        }
    };
    let hub = code.agents.hub(&machine.owner_id);
    let attached = {
        let _order = hub.order.lock().await;
        let attached = code.agents.attach(&machine.id, &machine.owner_id);
        match store::touch_machine(code.db.sql(), &machine.id, Some(&info)) {
            Ok(Some(m)) => {
                code.publish_machine(&m);
                attached
            }
            Ok(None) => {
                code.agents.detach(&machine.id, attached.conn);
                return close(socket, CLOSE_REVOKED, "the machine was revoked").await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "agent link: saving hello");
                code.agents.detach(&machine.id, attached.conn);
                return close(socket, 1011, "try again").await;
            }
        }
    };
    // The newest seq as of now (the hello's write doesn't change it).
    let resume_from = match store::machine(code.db.sql(), &machine.id) {
        Ok(Some(m)) => m.seq,
        _ => machine.seq,
    };
    let welcome = RelayFrame::Welcome { machine: MachineId(machine.id.clone()), owner: owner.name.clone(), resume_from };
    let ended = if socket.send(text(&welcome)).await.is_ok() { run(&code, &machine, &mut socket, attached.rx, &attached.close).await } else { End::Gone };
    match ended {
        End::Revoked => close(socket, CLOSE_REVOKED, "the machine was revoked").await,
        End::Replaced => close(socket, CLOSE_REPLACED, "replaced by a newer connection").await,
        End::Moved => close(socket, 1012, "reconnect").await,
        End::Protocol(e) => close(socket, CLOSE_PROTOCOL, &e).await,
        End::Silent => close(socket, 1001, "no frames for 60 s").await,
        End::Failed => close(socket, 1011, "try again").await,
        End::Gone => {}
    }
    if code.agents.detach(&machine.id, attached.conn) {
        let _order = hub.order.lock().await;
        if let Ok(Some(m)) = store::touch_machine(code.db.sql(), &machine.id, None) {
            code.publish_machine(&m);
        }
    }
}

enum End {
    /// The daemon closed or the socket broke.
    Gone,
    Silent,
    Revoked,
    Replaced,
    Moved,
    Protocol(String),
    Failed,
}

async fn run(code: &Arc<Code>, machine: &MachineRow, socket: &mut WebSocket, mut rx: tokio::sync::mpsc::Receiver<RelayFrame>, close: &Closer) -> End {
    let hub = code.agents.hub(&machine.owner_id);
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
    let mut heard = tokio::time::Instant::now();
    loop {
        tokio::select! {
            frame = recv(socket) => {
                heard = tokio::time::Instant::now();
                let frame = match frame {
                    None => return End::Gone,
                    Some(Err(e)) => return End::Protocol(e),
                    Some(Ok(f)) => f,
                };
                match frame {
                    DaemonFrame::Entities { items } => {
                        if items.is_empty() {
                            continue;
                        }
                        let _order = hub.order.lock().await;
                        match store::mirror(code.db.sql(), &machine.owner_id, &machine.id, &items) {
                            Ok(Some(m)) => {
                                for e in m.changed {
                                    let _ = hub.events.send(Arc::new(StreamEvent::Entity(Box::new(e))));
                                }
                                drop(_order);
                                if socket.send(text(&RelayFrame::Ack { seq: m.ack })).await.is_err() {
                                    return End::Gone;
                                }
                            }
                            Ok(None) => return End::Revoked,
                            Err(e) => {
                                tracing::warn!(machine = %machine.id, error = %e, "agent link: mirroring records");
                                return End::Failed;
                            }
                        }
                    }
                    DaemonFrame::Delta { delta } => {
                        let _ = hub.events.send(Arc::new(StreamEvent::Delta { machine: MachineId(machine.id.clone()), delta }));
                    }
                    DaemonFrame::Reply { id, result } => code.agents.replied(&machine.id, &id, result),
                    DaemonFrame::Hello { protocol, info } => {
                        if protocol != PROTOCOL {
                            return End::Protocol(format!("protocol {protocol} isn't supported"));
                        }
                        let _order = hub.order.lock().await;
                        match store::touch_machine(code.db.sql(), &machine.id, Some(&info)) {
                            Ok(Some(m)) => code.publish_machine(&m),
                            Ok(None) => return End::Revoked,
                            Err(e) => tracing::warn!(machine = %machine.id, error = %e, "agent link: saving hello"),
                        }
                    }
                    DaemonFrame::Pong => {}
                }
            }
            out = rx.recv() => {
                // The channel only closes once the registry dropped this link.
                let Some(f) = out else { return kicked(close.reason()) };
                if socket.send(text(&f)).await.is_err() {
                    return End::Gone;
                }
            }
            _ = ping.tick() => {
                if socket.send(text(&RelayFrame::Ping)).await.is_err() {
                    return End::Gone;
                }
            }
            _ = tokio::time::sleep_until(heard + SILENCE) => return End::Silent,
            why = close.kicked() => return kicked(why),
        }
    }
}

fn kicked(why: Kick) -> End {
    match why {
        Kick::Replaced => End::Replaced,
        Kick::Revoked => End::Revoked,
        Kick::Moved => End::Moved,
    }
}
