//! One daemon's WebSocket (docs/code/agents.md, "The relay protocol"): `hello` → `welcome`, then
//! entity batches (persisted, then acked and published), deltas (published only), replies to
//! commands, and pings both ways. 60 s without a frame from the daemon drops the link. After
//! `hello`, a frame that doesn't parse is skipped (what can be read of it is kept), never the end
//! of the link.

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
/// How often a link tells its daemon that records are over the quota.
const NOTICE_EVERY: Duration = Duration::from_secs(60);

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

/// A frame that isn't a [`DaemonFrame`] this relay reads: why, and its text if it was text.
struct Bad {
    why: String,
    text: Option<Utf8Bytes>,
}

/// Receive the next daemon frame. A transport ping or pong counts as a `pong` (the daemon is
/// alive). `None`: the socket ended.
async fn recv(socket: &mut WebSocket) -> Option<Result<DaemonFrame, Bad>> {
    match socket.recv().await? {
        Ok(Message::Text(t)) => Some(serde_json::from_str(t.as_str()).map_err(|e| Bad { why: format!("bad frame: {e}"), text: Some(t) })),
        Ok(Message::Binary(_)) => Some(Err(Bad { why: "binary frames aren't part of the protocol".into(), text: None })),
        Ok(Message::Close(_)) | Err(_) => None,
        Ok(Message::Ping(_) | Message::Pong(_)) => Some(Ok(DaemonFrame::Pong)),
    }
}

/// What can still be read of a frame that didn't parse, so one bad frame (a newer daemon's, a
/// record of a kind this relay doesn't know) costs only itself, never the link: a reply's id
/// (its waiters get an error instead of waiting out the timeout), or an entities batch's
/// records that do parse. Anything else is skipped.
fn salvage(bad: Bad) -> Option<DaemonFrame> {
    let v: serde_json::Value = bad.text.as_ref().and_then(|t| serde_json::from_str(t.as_str()).ok())?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("reply") => {
            let id = v.get("id").and_then(|i| i.as_str())?.to_string();
            Some(DaemonFrame::Reply { id, result: super::error(agent_proto::ErrorCode::Internal, &format!("the daemon's reply couldn't be read ({})", bad.why)) })
        }
        Some("entities") => {
            let items = v.get("items").and_then(|i| i.as_array())?;
            Some(DaemonFrame::Entities { items: items.iter().filter_map(|i| serde_json::from_value(i.clone()).ok()).collect() })
        }
        _ => None,
    }
}

pub(crate) async fn serve(code: Arc<Code>, machine: MachineRow, mut socket: WebSocket) {
    let info = match tokio::time::timeout(HELLO_WAIT, recv(&mut socket)).await {
        Ok(Some(Ok(DaemonFrame::Hello { protocol, info }))) if protocol == PROTOCOL => info,
        Ok(Some(Ok(DaemonFrame::Hello { protocol, .. }))) => {
            return close(socket, CLOSE_PROTOCOL, &format!("protocol {protocol} isn't supported; this relay speaks {PROTOCOL}: update roll")).await;
        }
        Ok(Some(Ok(_))) => return close(socket, CLOSE_PROTOCOL, "the first frame must be hello").await,
        Ok(Some(Err(e))) => return close(socket, CLOSE_PROTOCOL, &e.why).await,
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
    super::pull::start(&code);
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
    // A new sandbox joins the owner's gateway when the owner said so (in the background).
    super::gateway::sandbox_hello(&code, &machine);
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
    let mut noticed: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            frame = recv(socket) => {
                heard = tokio::time::Instant::now();
                let frame = match frame {
                    None => return End::Gone,
                    Some(Err(bad)) => {
                        tracing::warn!(machine = %machine.id, error = %bad.why, "agent link: a frame that doesn't parse");
                        match salvage(bad) {
                            Some(f) => f,
                            None => continue,
                        }
                    }
                    Some(Ok(f)) => f,
                };
                match frame {
                    DaemonFrame::Entities { items } => {
                        if items.is_empty() {
                            continue;
                        }
                        let _order = hub.order.lock().await;
                        let quota = code.agents.quota();
                        match store::mirror(code.db.sql(), &machine.owner_id, &machine.id, &items, quota) {
                            Ok(Some(m)) => {
                                if m.automated {
                                    code.wake_automations(&machine.owner_id);
                                }
                                for e in m.changed {
                                    let _ = hub.events.send(Arc::new(StreamEvent::Entity(Box::new(e))));
                                }
                                drop(_order);
                                if socket.send(text(&RelayFrame::Ack { seq: m.ack })).await.is_err() {
                                    return End::Gone;
                                }
                                // Over the quota: say so (once a minute at most), and go on.
                                if m.refused > 0 && noticed.is_none_or(|t: tokio::time::Instant| t.elapsed() >= NOTICE_EVERY) {
                                    noticed = Some(tokio::time::Instant::now());
                                    tracing::warn!(machine = %machine.id, refused = m.refused, "agent link: records over the quota");
                                    let message = format!(
                                        "roll didn't keep {} of this machine's records: it keeps {} records and {} MB per machine and {} MB per owner; delete threads you don't need",
                                        m.refused,
                                        quota.records,
                                        quota.size >> 20,
                                        quota.owner_size >> 20
                                    );
                                    if socket.send(text(&RelayFrame::Notice { message })).await.is_err() {
                                        return End::Gone;
                                    }
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
                        // Terminal traffic keeps a sandbox awake (provider logins happen there).
                        if matches!(delta, agent_proto::Delta::Terminal { .. }) {
                            code.agents.touch(&machine.id);
                        }
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
