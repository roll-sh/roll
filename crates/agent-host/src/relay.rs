//! The daemon's link to the relay (docs/code/agents.md, "The relay protocol"): one outbound
//! WebSocket to `/v1/code/agents/connect`, authenticated with the machine token.
//!
//! - `hello` → `welcome {resume_from}`; the records after `resume_from` are replayed from
//!   [`Daemon::since`] in batches, then [`Daemon::subscribe`] is followed live. The subscription
//!   is taken before `hello`, so nothing falls between replay and live; a record broadcast while
//!   replaying is skipped (the replay reads it from the store), and one the replay already sent
//!   is skipped live (its `seq` is at most the newest sent).
//! - Commands run concurrently ([`Daemon::run`]); replies go back in whatever order they finish,
//!   each once every record up to [`Daemon::last_seq`] as of its end is on the link (a command
//!   that finishes mid-replay waits for the replay to get there).
//! - A frame that doesn't parse (a newer relay's, or garbage) is skipped and the link stays up; a
//!   command among them gets an `invalid` reply.
//! - The relay pings every 20 s; 60 s without any frame from it drops the link. Links reconnect
//!   with jittered backoff (1 s doubling to 30 s, reset by a welcome).
//! - A refused token (401, or close code 4401) stops the link with [`LinkError::Revoked`]; so do
//!   a protocol the relay doesn't speak (4400) and another process taking the machine over (4409).

use crate::{Daemon, Outbound};
use agent_proto::{DaemonFrame, PROTOCOL, RelayFrame, Reply, Stamped};
use futures_util::{SinkExt, StreamExt};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest, protocol::WebSocketConfig};

/// Records per `entities` frame.
const BATCH: usize = 256;
/// A batch stops growing past this many bytes of JSON (the relay takes frames up to 16 MiB).
const BATCH_BYTES: usize = 4 << 20;
/// Largest frame taken from the relay.
const MAX_FRAME: usize = 16 << 20;
const SILENCE: Duration = Duration::from_secs(60);
const WELCOME_WAIT: Duration = Duration::from_secs(30);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How long a stopping daemon waits for the relay to acknowledge its last records.
pub const DRAIN: Duration = Duration::from_secs(3);

/// Where the relay is and who this machine is.
#[derive(Clone)]
pub struct LinkConfig {
    /// The roll API (`https://api.roll.sh`, `http://127.0.0.1:7070`); `ws://` and `wss://` work too.
    pub api: String,
    /// The machine token (`rlm_…`).
    pub token: String,
}

impl std::fmt::Debug for LinkConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkConfig").field("api", &self.api).finish_non_exhaustive()
    }
}

/// Why a link stopped for good.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("the relay refused this machine's token (it was revoked or never existed): run `roll agent login` again")]
    Revoked,
    #[error("the relay doesn't speak this protocol: {0}")]
    Protocol(String),
    #[error("another `roll agent up` connected with this machine's token")]
    Replaced,
    #[error("{0}")]
    Config(String),
}

/// What happened to the link, for logs and `roll agent up`'s output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkEvent {
    Connecting,
    /// Welcomed: the machine id and owner the relay knows this token as, and where the replay
    /// starts.
    Connected { machine: String, owner: String, resume_from: u64 },
    /// The link dropped; the next attempt is in `retry`.
    Disconnected { error: String, retry: Duration },
    /// The relay says something the user should know (records over the machine's quota).
    Notice { message: String },
}

/// The relay's WebSocket URL for an API base.
pub fn connect_url(api: &str) -> Result<String, LinkError> {
    let api = api.trim().trim_end_matches('/');
    let rest = if let Some(r) = api.strip_prefix("https://") {
        format!("wss://{r}")
    } else if let Some(r) = api.strip_prefix("http://") {
        format!("ws://{r}")
    } else if api.starts_with("wss://") || api.starts_with("ws://") {
        api.to_string()
    } else {
        return Err(LinkError::Config(format!("{api:?} isn't an http(s):// or ws(s):// URL")));
    };
    Ok(format!("{rest}/v1/code/agents/connect"))
}

/// TLS with the system's roots (and ring, whatever provider the process installed).
fn tls() -> Result<Arc<tokio_rustls::rustls::ClientConfig>, LinkError> {
    use tokio_rustls::rustls;
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err(LinkError::Config("no system root certificates found for wss://".into()));
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| LinkError::Config(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// How one connection attempt ended.
enum End {
    /// Try again (after backoff); `welcomed` resets the backoff.
    Retry { error: String, welcomed: bool },
    Stop(LinkError),
    /// Stopping: the relay has every record, the link is closed.
    Drained,
}

async fn open(cfg: &LinkConfig) -> Result<Ws, End> {
    let url = connect_url(&cfg.api).map_err(End::Stop)?;
    let mut req = url.as_str().into_client_request().map_err(|e| End::Stop(LinkError::Config(format!("{url}: {e}"))))?;
    let auth = format!("Bearer {}", cfg.token).parse().map_err(|_| End::Stop(LinkError::Config("the machine token isn't a valid header".into())))?;
    req.headers_mut().insert("authorization", auth);
    let connector = if url.starts_with("wss://") { Some(tokio_tungstenite::Connector::Rustls(tls().map_err(End::Stop)?)) } else { None };
    let ws_config = WebSocketConfig::default().max_message_size(Some(MAX_FRAME)).max_frame_size(Some(MAX_FRAME));
    match tokio_tungstenite::connect_async_tls_with_config(req, Some(ws_config), false, connector).await {
        Ok((ws, _)) => Ok(ws),
        Err(tungstenite::Error::Http(r)) if r.status() == 401 => Err(End::Stop(LinkError::Revoked)),
        Err(e) => Err(End::Retry { error: format!("connecting to {url}: {e}"), welcomed: false }),
    }
}

/// Check that the relay accepts the token, without going online (no `hello`).
pub async fn check(cfg: &LinkConfig) -> Result<(), LinkError> {
    match open(cfg).await {
        Ok(mut ws) => {
            let _ = ws.close(None).await;
            Ok(())
        }
        Err(End::Stop(e)) => Err(e),
        Err(End::Retry { error, .. }) => Err(LinkError::Config(error)),
        Err(End::Drained) => Ok(()),
    }
}

/// Keep the link up until it stops for good or `shutdown` completes (`Ok`). Once `shutdown`
/// completes (the daemon has stopped its work and stored its last records), the link sends what
/// the relay doesn't have yet and closes when it's acknowledged, waiting at most [`DRAIN`]: a
/// machine going to sleep leaves the relay with its final state. Commands that come meanwhile
/// aren't run: the relay sends them again on the next link.
pub async fn run(daemon: Arc<dyn Daemon>, cfg: LinkConfig, events: mpsc::UnboundedSender<LinkEvent>, shutdown: impl Future<Output = ()>) -> Result<(), LinkError> {
    tokio::pin!(shutdown);
    let (stop, stopping) = watch::channel(false);
    let mut backoff = BACKOFF_MIN;
    loop {
        let _ = events.send(LinkEvent::Connecting);
        let attempt = async {
            match open(&cfg).await {
                Ok(ws) => session(&daemon, ws, &events, stopping.clone()).await,
                Err(e) => e,
            }
        };
        tokio::pin!(attempt);
        let end = tokio::select! {
            e = &mut attempt => e,
            _ = &mut shutdown => {
                let _ = stop.send(true);
                let _ = tokio::time::timeout(DRAIN, &mut attempt).await;
                return Ok(());
            }
        };
        let error = match end {
            End::Stop(e) => return Err(e),
            End::Drained => return Ok(()),
            End::Retry { error, welcomed } => {
                if welcomed {
                    backoff = BACKOFF_MIN;
                }
                error
            }
        };
        // Jitter: between half and all of the backoff.
        let wait = backoff.mul_f64(0.5 + rand::random::<f64>() * 0.5);
        let _ = events.send(LinkEvent::Disconnected { error, retry: wait });
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = &mut shutdown => return Ok(()),
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

fn frame(f: &DaemonFrame) -> Message {
    Message::text(serde_json::to_string(f).unwrap_or_default())
}

/// What came from the relay.
// Moved once per frame, never stored: boxing the frame would buy nothing.
#[allow(clippy::large_enum_variant)]
enum In {
    Frame(RelayFrame),
    /// A text frame this daemon doesn't understand: skipped, the link stays up. `command` is its
    /// id when it looks like a command, which gets an `invalid` reply.
    Unreadable { command: Option<String> },
    /// Anything else (binary frames, pongs): ignored.
    Nothing,
}

/// One text frame: a frame this daemon speaks, or one it skips (a newer relay's frame type or
/// command, or garbage).
fn parse(text: &str) -> In {
    match serde_json::from_str(text) {
        Ok(f) => In::Frame(f),
        Err(e) => {
            let v: Option<serde_json::Value> = serde_json::from_str(text).ok();
            let command = v
                .as_ref()
                .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("command"))
                .and_then(|v| v.get("id")?.as_str())
                .filter(|id| !id.is_empty() && id.len() <= 128)
                .map(str::to_string);
            tracing::warn!("skipping a frame from the relay that doesn't parse: {e}");
            In::Unreadable { command }
        }
    }
}

/// The next relay frame. `Err` ends the session.
async fn recv(ws: &mut Ws) -> Result<In, End> {
    let retry = |error: String| End::Retry { error, welcomed: false };
    match ws.next().await {
        None => Err(retry("the relay closed the connection".into())),
        Some(Err(e)) => Err(retry(format!("the connection broke: {e}"))),
        Some(Ok(Message::Text(t))) => Ok(parse(t.as_str())),
        Some(Ok(Message::Close(c))) => {
            let (code, reason) = c.map(|c| (u16::from(c.code), c.reason.to_string())).unwrap_or((1005, String::new()));
            Err(match code {
                4401 => End::Stop(LinkError::Revoked),
                4400 => End::Stop(LinkError::Protocol(reason)),
                4409 => End::Stop(LinkError::Replaced),
                _ => retry(format!("the relay closed the link ({code} {reason})")),
            })
        }
        Some(Ok(_)) => Ok(In::Nothing),
    }
}

/// Records to send next: the replay's, or what's queued live.
struct Out {
    /// Replaying after this seq (`None`: live).
    replay: Option<u64>,
    /// The newest seq sent this session.
    sent: u64,
}

/// One connected session: hello, welcome, then replay and live until something ends it, or
/// `stopping` turns true and the relay has acknowledged everything.
async fn session(daemon: &Arc<dyn Daemon>, mut ws: Ws, events: &mpsc::UnboundedSender<LinkEvent>, mut stopping: watch::Receiver<bool>) -> End {
    let retry = |error: String, welcomed: bool| End::Retry { error, welcomed };
    let mut live = daemon.subscribe();
    if let Err(e) = ws.send(frame(&DaemonFrame::Hello { protocol: PROTOCOL, info: daemon.info().await })).await {
        return retry(format!("sending hello: {e}"), false);
    }
    let resume_from = loop {
        match tokio::time::timeout(WELCOME_WAIT, recv(&mut ws)).await {
            Err(_) => return retry("no welcome from the relay".into(), false),
            Ok(Err(e)) => return e,
            Ok(Ok(In::Frame(RelayFrame::Welcome { machine, owner, resume_from }))) => {
                let _ = events.send(LinkEvent::Connected { machine: machine.0, owner, resume_from });
                break resume_from;
            }
            Ok(Ok(In::Frame(RelayFrame::Ping))) => {
                let _ = ws.send(frame(&DaemonFrame::Pong)).await;
            }
            Ok(Ok(_)) => {}
        }
    };
    let mut out = Out { replay: Some(resume_from), sent: resume_from };
    let mut acked = resume_from;
    let (reply_tx, mut replies) = mpsc::unbounded_channel::<(String, Reply, u64)>();
    // Replies waiting for the records their command wrote: (the seq to have sent, id, reply).
    let mut held: Vec<(u64, String, Reply)> = Vec::new();
    let mut heard = tokio::time::Instant::now();
    // Stopping: the newest seq as of the stop, which the relay must acknowledge.
    let mut drain: Option<u64> = None;
    loop {
        // Replay a batch whenever nothing else is ready.
        if let Some(after) = out.replay {
            let batch = match daemon.since(after, BATCH).await {
                Ok(b) => b,
                Err(e) => return retry(format!("reading records to replay: {e}"), true),
            };
            let more = batch.len() == BATCH;
            if let Some(last) = batch.last() {
                out.sent = out.sent.max(last.seq);
                out.replay = Some(last.seq);
                for chunk in chunks(batch) {
                    if let Err(e) = ws.send(frame(&DaemonFrame::Entities { items: chunk })).await {
                        return retry(format!("sending records: {e}"), true);
                    }
                }
            }
            if !more {
                out.replay = None;
            }
        }
        // A reply goes once every record up to its command's last write is on the link.
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut held).into_iter().partition(|(upto, ..)| *upto <= out.sent);
        held = waiting;
        for (_, id, result) in ready {
            if let Err(e) = ws.send(frame(&DaemonFrame::Reply { id, result })).await {
                return retry(format!("sending a reply: {e}"), true);
            }
        }
        if drain.is_some_and(|upto| acked >= upto && out.replay.is_none() && held.is_empty()) {
            let _ = ws.close(None).await;
            return End::Drained;
        }
        let replaying = out.replay.is_some();
        let draining = drain.is_some();
        tokio::select! {
            biased;
            _ = stopping.changed(), if !draining => {
                drain = Some(daemon.last_seq().await.unwrap_or(0));
            }
            f = recv(&mut ws) => {
                heard = tokio::time::Instant::now();
                match f {
                    Err(e) => return match e {
                        End::Retry { error, .. } => retry(error, true),
                        stop => stop,
                    },
                    Ok(In::Nothing | In::Unreadable { command: None }) => {}
                    Ok(In::Unreadable { command: Some(id) }) => {
                        let result = Reply::Error(agent_proto::ReplyError {
                            code: agent_proto::ErrorCode::Invalid,
                            message: "this machine's roll doesn't understand that command; update it".into(),
                        });
                        if let Err(e) = ws.send(frame(&DaemonFrame::Reply { id, result })).await {
                            return retry(format!("sending a reply: {e}"), true);
                        }
                    }
                    Ok(In::Frame(RelayFrame::Ack { seq })) => acked = acked.max(seq),
                    Ok(In::Frame(RelayFrame::Ping)) => {
                        if let Err(e) = ws.send(frame(&DaemonFrame::Pong)).await {
                            return retry(format!("answering a ping: {e}"), true);
                        }
                    }
                    // Stopping: the relay sends it again on the next link.
                    Ok(In::Frame(RelayFrame::Command { .. })) if draining => {}
                    Ok(In::Frame(RelayFrame::Command { id, by, command, grant })) => {
                        let (tx, daemon) = (reply_tx.clone(), daemon.clone());
                        tokio::spawn(async move {
                            let result = daemon.run_granted(id.clone(), by, command, grant.map(|g| *g)).await;
                            // Read once the command is done: its records are at or below it.
                            let upto = daemon.last_seq().await.unwrap_or_else(|e| {
                                tracing::warn!("reading the newest seq: {e:#}");
                                0
                            });
                            let _ = tx.send((id, result, upto));
                        });
                    }
                    Ok(In::Frame(RelayFrame::Welcome { .. })) => {}
                    Ok(In::Frame(RelayFrame::Notice { message })) => {
                        tracing::warn!("{message}");
                        let _ = events.send(LinkEvent::Notice { message });
                    }
                }
            }
            Some((id, result, upto)) = replies.recv() => {
                // A command's records were broadcast before its reply: what is queued goes
                // first, and the reply waits in `held` until the replay or the live records reach
                // `upto` (the top of the loop sends it), so a client that has the reply finds
                // them on the relay.
                held.push((upto, id, result));
                let mut items = Vec::new();
                for _ in 0..live.len() {
                    match live.try_recv() {
                        Ok(Outbound::Entity(s)) => {
                            if !replaying && s.seq > out.sent {
                                items.push(*s);
                            }
                        }
                        Ok(o) => {
                            if let Err(e) = send_other(&mut ws, o).await {
                                return retry(e, true);
                            }
                        }
                        Err(broadcast::error::TryRecvError::Lagged(_)) => {
                            live = daemon.subscribe();
                            out.replay = Some(acked);
                            items.clear();
                            break;
                        }
                        Err(_) => break,
                    }
                }
                items.sort_by_key(|s| s.seq);
                if let Some(last) = items.last() {
                    out.sent = last.seq;
                }
                for chunk in chunks(items) {
                    if let Err(e) = ws.send(frame(&DaemonFrame::Entities { items: chunk })).await {
                        return retry(format!("sending records: {e}"), true);
                    }
                }
            }
            ev = live.recv() => match ev {
                Ok(Outbound::Entity(s)) => {
                    // While replaying, the replay reads it from the store.
                    if replaying || s.seq <= out.sent {
                        continue;
                    }
                    let mut items = vec![*s];
                    // Whatever else is queued goes in the same frame.
                    let mut other = Vec::new();
                    while items.len() < BATCH {
                        match live.try_recv() {
                            Ok(Outbound::Entity(s)) => items.push(*s),
                            Ok(o) => {
                                other.push(o);
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    items.retain(|s| s.seq > out.sent);
                    items.sort_by_key(|s| s.seq);
                    if let Some(last) = items.last() {
                        out.sent = last.seq;
                    }
                    for chunk in chunks(items) {
                        if let Err(e) = ws.send(frame(&DaemonFrame::Entities { items: chunk })).await {
                            return retry(format!("sending records: {e}"), true);
                        }
                    }
                    for o in other {
                        if let Err(e) = send_other(&mut ws, o).await {
                            return retry(e, true);
                        }
                    }
                }
                Ok(o) => {
                    if let Err(e) = send_other(&mut ws, o).await {
                        return retry(e, true);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Missed some: start over from what the relay acknowledged.
                    live = daemon.subscribe();
                    out.replay = Some(acked);
                }
                Err(broadcast::error::RecvError::Closed) => return End::Stop(LinkError::Config("the daemon stopped".into())),
            },
            _ = tokio::time::sleep_until(heard + SILENCE) => return retry("nothing from the relay for 60 s".into(), true),
            // Let the replay loop go on when nothing else is ready.
            _ = std::future::ready(()), if replaying => {}
        }
    }
}

/// Deltas and info changes.
async fn send_other(ws: &mut Ws, o: Outbound) -> Result<(), String> {
    let f = match o {
        Outbound::Delta(delta) => DaemonFrame::Delta { delta },
        Outbound::Info(info) => DaemonFrame::Hello { protocol: PROTOCOL, info },
        Outbound::Entity(_) => return Ok(()),
    };
    ws.send(frame(&f)).await.map_err(|e| format!("sending to the relay: {e}"))
}

/// Split records into frames of at most [`BATCH`] records and about [`BATCH_BYTES`].
fn chunks(items: Vec<Stamped>) -> Vec<Vec<Stamped>> {
    let mut out = Vec::new();
    let (mut cur, mut bytes) = (Vec::new(), 0usize);
    for s in items {
        let n = serde_json::to_vec(&s).map(|v| v.len()).unwrap_or(0);
        if !cur.is_empty() && (cur.len() >= BATCH || bytes + n > BATCH_BYTES) {
            out.push(std::mem::take(&mut cur));
            bytes = 0;
        }
        bytes += n;
        cur.push(s);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(connect_url("https://api.roll.sh/").unwrap(), "wss://api.roll.sh/v1/code/agents/connect");
        assert_eq!(connect_url("http://127.0.0.1:7070").unwrap(), "ws://127.0.0.1:7070/v1/code/agents/connect");
        assert_eq!(connect_url("wss://x").unwrap(), "wss://x/v1/code/agents/connect");
        assert!(connect_url("ftp://x").is_err());
    }

    #[test]
    fn tls_has_roots() {
        assert!(tls().is_ok());
    }
}
