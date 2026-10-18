//! Push consumption over Server-Sent Events and WebSocket. Each stream is a consumer session
//! registered with the broker like an AMQP connection with one channel: prefetch credit
//! bounds what is in flight, force-close and draining end it, and everything it still holds
//! goes back to the queue when it ends.

use crate::HttpApi;
use crate::auth::{Auth, Op, P, Q};
use crate::error::{ApiError, ApiResult};
use crate::message::{self, InMessage};
use crate::publish;
use crate::receipt::{Holder, Receipt};
use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message as WsMsg, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use mq_core::broker::{Broker, ChannelInfo, ConnectionInfo};
use mq_core::queue::{ChannelId, ConnId, ConsumerSink, ConsumerSpec, Delivery, Outcome, QueueRef, QueueType};
use mq_core::value::{FieldTable, FieldValue};
use mq_core::VHost;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::{mpsc, watch};

const MAX_PREFETCH: u32 = 10_000;

pub(crate) enum Event {
    Deliver(Delivery),
    Cancelled,
    TimedOut(u64),
}

/// A streaming consumer: the queue's view of an SSE or WebSocket client.
pub(crate) struct Session {
    pub id: ChannelId,
    pub conn: ConnId,
    pub queue: QueueRef,
    pub tag: Arc<str>,
    vhost: Arc<VHost>,
    prefetch: AtomicU32,
    unacked: AtomicU32,
    held: Mutex<HashSet<u64>>,
    tx: mpsc::UnboundedSender<Event>,
    closed: AtomicBool,
    pub info: Arc<ConnectionInfo>,
    chan: Arc<ChannelInfo>,
}

impl ConsumerSink for Session {
    fn has_credit(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && self.unacked.load(Ordering::Acquire) < self.prefetch.load(Ordering::Acquire)
    }

    fn deliver(&self, d: Delivery) {
        self.held.lock().insert(d.seq);
        self.unacked.fetch_add(1, Ordering::AcqRel);
        self.chan.unacked.fetch_add(1, Ordering::Relaxed);
        self.chan.delivered.fetch_add(1, Ordering::Relaxed);
        if d.redelivered {
            self.chan.redelivered.fetch_add(1, Ordering::Relaxed);
        }
        let vs = &self.vhost.stats;
        vs.delivered.fetch_add(1, Ordering::Relaxed);
        vs.delivered_bytes.fetch_add(d.msg.body.len() as u64, Ordering::Relaxed);
        let _ = self.tx.send(Event::Deliver(d));
    }

    fn cancelled(&self, _tag: &str) {
        let _ = self.tx.send(Event::Cancelled);
    }

    fn timed_out(&self, _tag: &str, timeout_ms: u64) {
        let _ = self.tx.send(Event::TimedOut(timeout_ms));
    }
}

impl Session {
    /// Settle a message this session holds; false if it doesn't hold it (any more).
    pub async fn settle(&self, seq: u64, outcome: Outcome) -> bool {
        if !self.held.lock().remove(&seq) {
            return false;
        }
        self.unacked.fetch_sub(1, Ordering::AcqRel);
        self.chan.unacked.fetch_sub(1, Ordering::Relaxed);
        if outcome == Outcome::Ack {
            self.chan.acked.fetch_add(1, Ordering::Relaxed);
            self.vhost.stats.acked.fetch_add(1, Ordering::Relaxed);
        }
        self.queue.settle(self.id, vec![seq], outcome).await;
        self.queue.wake();
        true
    }

    fn set_prefetch(&self, n: u32) {
        self.prefetch.store(n, Ordering::Release);
        self.chan.prefetch_count.store(n, Ordering::Relaxed);
        self.queue.wake();
    }
}

/// Ends the session when dropped (client gone, error, shutdown): deregisters it and returns
/// what it holds to the queue.
pub(crate) struct Guard {
    api: Weak<HttpApi>,
    broker: Arc<Broker>,
    session: Arc<Session>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let s = self.session.clone();
        s.closed.store(true, Ordering::Release);
        if let Some(api) = self.api.upgrade() {
            api.sessions.write().remove(&s.id);
        }
        let broker = self.broker.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                s.queue.cancel(s.id, s.tag.clone()).await;
                s.queue.channel_closed(s.id).await;
                broker.channel_closed(s.id);
                broker.connection_closed(s.conn).await;
            });
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StreamQuery {
    /// `auto` (default) or `manual`.
    #[serde(default)]
    ack: Option<String>,
    #[serde(default)]
    prefetch: Option<u32>,
    /// Streams: where to start (`first`, `last`, `next`, an offset, or an interval like `1h`).
    #[serde(default)]
    offset: Option<String>,
    /// Consumer priority (`x-priority`).
    #[serde(default)]
    priority: Option<i32>,
    /// Bearer token for clients that can't send headers (read by the auth extractor).
    #[serde(default)]
    #[allow(dead_code)]
    access_token: Option<String>,
}

struct Opts {
    auto: bool,
    prefetch: u32,
    offset: Option<String>,
    priority: i32,
}

impl StreamQuery {
    fn opts(self) -> ApiResult<Opts> {
        let auto = match self.ack.as_deref() {
            None | Some("auto") => true,
            Some("manual") => false,
            Some(o) => return Err(ApiError::bad_request(format!("ack must be 'auto' or 'manual', not '{o}'"))),
        };
        let prefetch = self.prefetch.unwrap_or(if auto { 100 } else { 10 });
        if !(1..=MAX_PREFETCH).contains(&prefetch) {
            return Err(ApiError::bad_request(format!("prefetch must be between 1 and {MAX_PREFETCH}")));
        }
        Ok(Opts { auto, prefetch, offset: self.offset, priority: self.priority.unwrap_or(0) })
    }
}

fn offset_arg(offset: Option<&str>, last_event_id: Option<&str>) -> ApiResult<Option<FieldValue>> {
    if let Some(id) = last_event_id.map(str::trim).filter(|s| !s.is_empty()) {
        let n: u64 = id.parse().map_err(|_| ApiError::bad_request(format!("Last-Event-ID '{id}' is not a stream offset")))?;
        return Ok(Some(FieldValue::I64(n.saturating_add(1) as i64)));
    }
    let Some(o) = offset else { return Ok(None) };
    let v = match o.parse::<i64>() {
        Ok(n) => FieldValue::I64(n),
        Err(_) => FieldValue::str(o),
    };
    mq_core::queue::stream::parse_offset(Some(&v)).map_err(|e| ApiError::bad_request(e.text))?;
    Ok(Some(v))
}

struct Opened {
    session: Arc<Session>,
    rx: mpsc::UnboundedReceiver<Event>,
    guard: Guard,
    vh: Arc<VHost>,
}

async fn open(api: &Arc<HttpApi>, auth: &Auth, vhost: &str, queue: &str, protocol: &str, opts: &Opts, last_event_id: Option<&str>) -> ApiResult<Opened> {
    if api.is_stopping() {
        return Err(ApiError::unavailable("the broker is shutting down; reconnect"));
    }
    let broker = &api.broker;
    let vh = api.vhost(auth, vhost).await?;
    api.allow(auth, vhost, Op::Consume, "queue", queue, None)?;
    broker.check_connection_limits(&auth.user, vhost).map_err(|e| ApiError::forbidden(e.reply_text()))?;
    let conn = broker.next_id();
    let q = vh.queue_for(queue, Some(conn))?;
    let mut args = FieldTable::new();
    if q.kind == QueueType::Stream {
        if let Some(v) = offset_arg(opts.offset.as_deref(), last_event_id)? {
            args.insert("x-stream-offset", v);
        }
    } else if opts.offset.is_some() {
        return Err(ApiError::bad_request("offset applies to stream queues only"));
    }
    if opts.priority != 0 {
        args.insert("x-priority", FieldValue::I64(opts.priority as i64));
    }
    let peer = auth.peer.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
    let info = Arc::new(ConnectionInfo {
        id: conn,
        name: format!("{peer} -> http {protocol} {queue}"),
        protocol: protocol.into(),
        vhost: vhost.into(),
        user: auth.user.name.clone(),
        peer,
        local: api.config.listen,
        client_properties: json!({"product": "roll queues HTTP API", "token": auth.token.as_ref().map(|t| t.name.clone())}),
        connected_at_ms: mq_core::now_ms(),
        tls: false,
        auth_mechanism: if auth.token.is_some() { "BEARER".into() } else { "PLAIN".into() },
        frame_max: 0.into(),
        channel_max: 1.into(),
        heartbeat: (api.config.heartbeat.as_secs() as u32).into(),
        recv_oct: 0.into(),
        send_oct: 0.into(),
        recv_cnt: 0.into(),
        send_cnt: 0.into(),
        channels: 1.into(),
        blocked: AtomicBool::new(false),
        close_reason: Mutex::new(None),
        close: tokio::sync::Notify::new(),
    });
    broker.register_connection(info.clone());
    let id = broker.next_id();
    let chan = Arc::new(ChannelInfo {
        id,
        conn,
        number: 1,
        conn_name: info.name.clone(),
        vhost: vhost.into(),
        user: auth.user.name.clone(),
        prefetch_count: opts.prefetch.into(),
        global_prefetch_count: 0.into(),
        unacked: 0.into(),
        consumers: 1.into(),
        confirm: true.into(),
        transactional: false.into(),
        published: 0.into(),
        delivered: 0.into(),
        acked: 0.into(),
        confirmed: 0.into(),
        returned: 0.into(),
        redelivered: 0.into(),
    });
    broker.register_channel(chan.clone());
    let (tx, rx) = mpsc::unbounded_channel();
    let tag: Arc<str> = Arc::from(format!("http.ctag-{id:x}").as_str());
    let session = Arc::new(Session {
        id,
        conn,
        queue: q.clone(),
        tag: tag.clone(),
        vhost: vh.clone(),
        prefetch: AtomicU32::new(opts.prefetch),
        unacked: AtomicU32::new(0),
        held: Mutex::new(HashSet::new()),
        tx,
        closed: AtomicBool::new(false),
        info,
        chan,
    });
    let guard = Guard { api: Arc::downgrade(api), broker: broker.clone(), session: session.clone() };
    let spec = ConsumerSpec { tag, channel: id, conn, no_ack: false, exclusive: false, priority: opts.priority, args, sink: session.clone() };
    q.consume(spec).await?;
    api.sessions.write().insert(id, session.clone());
    q.wake();
    Ok(Opened { session, rx, guard, vh })
}

fn opened_json(s: &Session, opts: &Opts) -> Value {
    json!({"consumer_tag": &*s.tag, "queue": s.queue.meta.name, "prefetch": opts.prefetch, "ack": if opts.auto { "auto" } else { "manual" }})
}

/// Resolves once `rx` reads true.
pub(crate) async fn stopped(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|s| *s).await;
}

// ── SSE ──

struct SseState {
    key: Vec<u8>,
    session: Arc<Session>,
    rx: mpsc::UnboundedReceiver<Event>,
    _guard: Guard,
    auto: bool,
    stop: watch::Receiver<bool>,
    first: Option<SseEvent>,
    done: bool,
}

impl SseState {
    async fn next(&mut self) -> Option<SseEvent> {
        if let Some(e) = self.first.take() {
            return Some(e);
        }
        if self.done {
            return None;
        }
        let info = self.session.info.clone();
        let next = tokio::select! {
            e = self.rx.recv() => e,
            _ = info.close.notified() => {
                self.done = true;
                let reason = info.close_reason.lock().clone().unwrap_or_else(|| "closed via management plugin".into());
                return Some(SseEvent::default().event("close").data(json!({"reason": reason}).to_string()));
            }
            _ = stopped(&mut self.stop) => {
                self.done = true;
                return Some(SseEvent::default().event("close").data(json!({"reason": "the broker is shutting down; reconnect"}).to_string()));
            }
        };
        match next {
            Some(Event::Deliver(d)) => Some(self.deliver(d).await),
            Some(Event::TimedOut(ms)) => {
                self.done = true;
                Some(SseEvent::default().event("error").data(json!({"code": "precondition_failed", "message": format!("delivery acknowledgement timed out after {ms} ms")}).to_string()))
            }
            Some(Event::Cancelled) | None => {
                self.done = true;
                Some(SseEvent::default().event("cancelled").data(json!({"reason": "the queue was deleted or the consumer was cancelled"}).to_string()))
            }
        }
    }

    async fn deliver(&mut self, d: Delivery) -> SseEvent {
        let s = &self.session;
        let mut m = message::render(s.queue.kind, &s.queue.meta.name, d.seq, &d.msg, d.redelivered, d.delivery_count);
        if !self.auto {
            let r = Receipt { vhost: s.vhost.name.clone(), queue: s.queue.meta.name.clone(), seq: d.seq, delivery_count: d.delivery_count, holder: Holder::Session { channel: s.id } };
            m.insert("receipt".into(), r.encode(&self.key).into());
        }
        let data = Value::Object(m).to_string();
        s.info.send_oct.fetch_add(data.len() as u64, Ordering::Relaxed);
        s.info.send_cnt.fetch_add(1, Ordering::Relaxed);
        if self.auto {
            s.settle(d.seq, Outcome::Ack).await;
        }
        SseEvent::default().id(d.seq.to_string()).event("message").data(data)
    }
}

/// `GET .../queues/{queue}/events?ack=auto|manual&prefetch=N[&offset=..]`
pub async fn events(State(api): State<Arc<HttpApi>>, auth: Auth, P((vhost, queue)): P<(String, String)>, Q(query): Q<StreamQuery>, headers: HeaderMap) -> ApiResult<Response> {
    let opts = query.opts()?;
    let last_event_id = headers.get("last-event-id").and_then(|v| v.to_str().ok());
    let o = open(&api, &auth, &vhost, &queue, "HTTP SSE", &opts, last_event_id).await?;
    let first = SseEvent::default().event("open").data(opened_json(&o.session, &opts).to_string());
    let state = SseState {
        key: api.receipt_key().to_vec(),
        session: o.session,
        rx: o.rx,
        _guard: o.guard,
        auto: opts.auto,
        stop: api.stop_rx(),
        first: Some(first),
        done: false,
    };
    let stream = futures_util::stream::unfold(state, |mut s| async move { s.next().await.map(|e| (Ok::<_, Infallible>(e), s)) });
    let sse = Sse::new(stream).keep_alive(KeepAlive::new().interval(api.config.heartbeat).text("heartbeat"));
    Ok(([("cache-control", "no-cache"), ("x-accel-buffering", "no")], sse).into_response())
}

// ── WebSocket ──

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Frame {
    Ack {
        delivery_tag: u64,
        #[serde(default)]
        multiple: bool,
    },
    Nack {
        delivery_tag: u64,
        #[serde(default)]
        multiple: bool,
        #[serde(default = "yes")]
        requeue: bool,
    },
    Credit {
        prefetch: u32,
    },
    Publish {
        #[serde(default, rename = "ref")]
        reference: Option<Value>,
        #[serde(default)]
        exchange: Option<String>,
        #[serde(flatten)]
        message: Map<String, Value>,
    },
}

/// A WebSocket publish: `ref`, `exchange` and the message fields.
type PublishFrame = (Option<Value>, Option<String>, Map<String, Value>);

fn yes() -> bool {
    true
}

fn err_frame(reference: Option<&Value>, e: &ApiError) -> Value {
    let mut v = json!({"type": "error", "error": e.body()});
    if let Some(r) = reference {
        v["ref"] = r.clone();
    }
    v
}

/// `GET .../queues/{queue}/ws?ack=auto|manual&prefetch=N[&offset=..]`
pub async fn websocket(
    State(api): State<Arc<HttpApi>>,
    auth: Auth,
    P((vhost, queue)): P<(String, String)>,
    Q(query): Q<StreamQuery>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> ApiResult<Response> {
    let ws = ws.map_err(|e| ApiError::bad_request(e.body_text()))?;
    let opts = query.opts()?;
    let last_event_id = headers.get("last-event-id").and_then(|v| v.to_str().ok());
    let o = open(&api, &auth, &vhost, &queue, "WebSocket", &opts, last_event_id).await?;
    Ok(ws.on_upgrade(move |socket| run_ws(api, auth, o, opts, socket)))
}

/// Publishes from a WebSocket go through one task, so they reach the queue in order; their
/// confirms are awaited concurrently.
async fn ws_publisher(api: Arc<HttpApi>, auth: Auth, vh: Arc<VHost>, queue: String, mut rx: mpsc::Receiver<PublishFrame>, out: mpsc::UnboundedSender<Value>) {
    while let Some((reference, exchange, mut fields)) = rx.recv().await {
        let exchange = exchange.map(|e| if e == crate::auth::DEFAULT_EXCHANGE { String::new() } else { e }).unwrap_or_default();
        if exchange.is_empty() && !fields.contains_key("routing_key") {
            fields.insert("routing_key".into(), queue.clone().into());
        }
        let msg: Result<InMessage, _> = serde_json::from_value(Value::Object(fields));
        let sent = match msg {
            Ok(m) => publish::send_messages(&api, &auth, &vh, &exchange, None, vec![m]).await,
            Err(e) => Err(ApiError::bad_request(format!("invalid publish frame: {e}"))),
        };
        match sent {
            Ok(pending) => {
                let out = out.clone();
                tokio::spawn(async move {
                    let mut v = publish::confirms(pending).await.into_iter().next().unwrap_or(Value::Null);
                    v["type"] = "confirm".into();
                    if let Some(r) = reference {
                        v["ref"] = r;
                    }
                    let _ = out.send(v);
                });
            }
            Err(e) => {
                let _ = out.send(err_frame(reference.as_ref(), &e));
            }
        }
    }
}

async fn run_ws(api: Arc<HttpApi>, auth: Auth, o: Opened, opts: Opts, socket: WebSocket) {
    let Opened { session, mut rx, guard, vh } = o;
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let (pub_tx, pub_rx) = mpsc::channel(256);
    let publisher = tokio::spawn(ws_publisher(api.clone(), auth, vh, session.queue.meta.name.clone(), pub_rx, out_tx.clone()));
    let mut stop = api.stop_rx();
    let mut ping = tokio::time::interval(api.config.heartbeat);
    ping.tick().await;
    let mut tags: BTreeMap<u64, u64> = BTreeMap::new();
    let mut next_tag = 0u64;
    let info = session.info.clone();
    let send = |v: Value| WsMsg::Text(v.to_string().into());
    if sink.send(send(json!({"type": "open", "consumer": opened_json(&session, &opts)}))).await.is_err() {
        return;
    }
    let close = |code: u16, reason: &str| WsMsg::Close(Some(CloseFrame { code, reason: reason.into() }));
    loop {
        let msg = tokio::select! {
            e = rx.recv() => match e {
                Some(Event::Deliver(d)) => {
                    next_tag += 1;
                    let m = message::render(session.queue.kind, &session.queue.meta.name, d.seq, &d.msg, d.redelivered, d.delivery_count);
                    let frame = json!({"type": "deliver", "delivery_tag": next_tag, "message": m});
                    let text = frame.to_string();
                    info.send_oct.fetch_add(text.len() as u64, Ordering::Relaxed);
                    if sink.send(WsMsg::Text(text.into())).await.is_err() {
                        break;
                    }
                    if opts.auto {
                        session.settle(d.seq, Outcome::Ack).await;
                    } else {
                        tags.insert(next_tag, d.seq);
                    }
                    continue;
                }
                Some(Event::TimedOut(ms)) => {
                    let e = ApiError::conflict("precondition_failed", format!("delivery acknowledgement timed out after {ms} ms"));
                    let _ = sink.send(send(err_frame(None, &e))).await;
                    close(1008, "delivery acknowledgement timed out")
                }
                Some(Event::Cancelled) | None => {
                    let _ = sink.send(send(json!({"type": "cancelled"}))).await;
                    close(1000, "consumer cancelled")
                }
            },
            m = stream.next() => {
                let text = match m {
                    Some(Ok(WsMsg::Text(t))) => t,
                    Some(Ok(WsMsg::Binary(_))) => {
                        let e = ApiError::bad_request("frames are JSON text");
                        if sink.send(send(err_frame(None, &e))).await.is_err() { break }
                        continue;
                    }
                    Some(Ok(WsMsg::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => continue,
                };
                info.recv_oct.fetch_add(text.len() as u64, Ordering::Relaxed);
                info.recv_cnt.fetch_add(1, Ordering::Relaxed);
                if let Some(reply) = handle_frame(&session, &mut tags, next_tag, &text, &pub_tx).await
                    && sink.send(send(reply)).await.is_err()
                {
                    break;
                }
                continue;
            }
            v = out_rx.recv() => {
                if let Some(v) = v && sink.send(send(v)).await.is_err() { break }
                continue;
            }
            _ = ping.tick() => {
                if sink.send(WsMsg::Ping(Default::default())).await.is_err() { break }
                continue;
            }
            _ = info.close.notified() => {
                let reason = info.close_reason.lock().clone().unwrap_or_else(|| "closed via management plugin".into());
                close(1001, &reason)
            }
            _ = stopped(&mut stop) => close(1001, "the broker is shutting down; reconnect"),
        };
        let _ = sink.send(msg).await;
        break;
    }
    publisher.abort();
    drop(guard);
}

/// Apply one client frame; returns an error frame to send back, if any.
async fn handle_frame(
    session: &Session,
    tags: &mut BTreeMap<u64, u64>,
    last_tag: u64,
    text: &str,
    publish: &mpsc::Sender<PublishFrame>,
) -> Option<Value> {
    let frame: Frame = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => return Some(err_frame(None, &ApiError::bad_request(format!("invalid frame: {e}")))),
    };
    let settle = async |tags: &mut BTreeMap<u64, u64>, tag: u64, multiple: bool, outcome: Outcome| -> Option<Value> {
        let seqs: Vec<u64> = if multiple {
            if tag > last_tag {
                return Some(err_frame(None, &ApiError::conflict("precondition_failed", format!("unknown delivery tag {tag}"))));
            }
            let keys: Vec<u64> = tags.range(..=tag).map(|(k, _)| *k).collect();
            keys.into_iter().filter_map(|k| tags.remove(&k)).collect()
        } else {
            match tags.remove(&tag) {
                Some(s) => vec![s],
                None => return Some(err_frame(None, &ApiError::conflict("precondition_failed", format!("unknown delivery tag {tag}")))),
            }
        };
        for s in seqs {
            session.settle(s, outcome).await;
        }
        None
    };
    match frame {
        Frame::Ack { delivery_tag, multiple } => settle(tags, delivery_tag, multiple, Outcome::Ack).await,
        Frame::Nack { delivery_tag, multiple, requeue } => settle(tags, delivery_tag, multiple, if requeue { Outcome::Requeue } else { Outcome::Reject }).await,
        Frame::Credit { prefetch } => {
            if !(1..=MAX_PREFETCH).contains(&prefetch) {
                return Some(err_frame(None, &ApiError::bad_request(format!("prefetch must be between 1 and {MAX_PREFETCH}"))));
            }
            session.set_prefetch(prefetch);
            None
        }
        Frame::Publish { reference, exchange, message } => {
            let _ = publish.send((reference, exchange, message)).await;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_parse() {
        assert!(matches!(serde_json::from_str::<Frame>(r#"{"type":"ack","delivery_tag":3}"#).unwrap(), Frame::Ack { delivery_tag: 3, multiple: false }));
        assert!(matches!(serde_json::from_str::<Frame>(r#"{"type":"nack","delivery_tag":1}"#).unwrap(), Frame::Nack { requeue: true, .. }));
        match serde_json::from_str::<Frame>(r#"{"type":"publish","ref":7,"routing_key":"q","body":"hi"}"#).unwrap() {
            Frame::Publish { reference, exchange, message } => {
                assert_eq!(reference, Some(json!(7)));
                assert_eq!(exchange, None);
                assert_eq!(message["body"], "hi");
                assert!(message.get("type").is_none());
            }
            _ => panic!("publish"),
        }
        assert!(serde_json::from_str::<Frame>(r#"{"type":"bogus"}"#).is_err());
    }

    #[test]
    fn stream_offsets() {
        assert_eq!(offset_arg(None, Some("41")).unwrap(), Some(FieldValue::I64(42)));
        assert_eq!(offset_arg(Some("first"), None).unwrap(), Some(FieldValue::str("first")));
        assert_eq!(offset_arg(Some("10"), None).unwrap(), Some(FieldValue::I64(10)));
        assert!(offset_arg(Some("sideways"), None).is_err());
        assert!(offset_arg(None, Some("x")).is_err());
        assert_eq!(offset_arg(None, None).unwrap(), None);
    }
}
