//! Publishing: one message or a batch, answered once publisher confirms are in. Replays with
//! `Idempotency-Key`, and drops repeated message `id`s within the dedup window.

use crate::HttpApi;
use crate::auth::{Auth, DEFAULT_EXCHANGE, Op, P};
use crate::error::{ApiError, ApiResult};
use crate::idempotency::{self, Begin};
use crate::message::{self, InMessage};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use mq_core::VHost;
use mq_core::auth::Access;
use mq_core::exchange::ExchangeKind;
use mq_core::queue::ConfirmTicket;
use mq_core::value::FieldValue;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::{oneshot, watch};

const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);
const DEDUP_MAX: usize = 100_000;

/// The first publish result for an id, once its confirm is in.
type ResultRx = watch::Receiver<Option<Value>>;

/// Recently published message ids and their results.
#[derive(Default)]
pub struct DedupWindow {
    inner: Mutex<Window>,
}

/// Ids with their results, and their expiry in arrival order.
#[derive(Default)]
struct Window {
    map: HashMap<String, ResultRx>,
    order: VecDeque<(u64, String)>,
}

enum Claim {
    New(watch::Sender<Option<Value>>),
    Dup(watch::Receiver<Option<Value>>),
}

impl DedupWindow {
    fn claim(&self, key: &str, window_ms: u64) -> Claim {
        let now = mq_core::now_ms();
        let mut g = self.inner.lock();
        let Window { map, order } = &mut *g;
        while let Some((exp, k)) = order.front() {
            if *exp > now && map.len() <= DEDUP_MAX {
                break;
            }
            map.remove(k);
            order.pop_front();
        }
        if let Some(rx) = map.get(key) {
            return Claim::Dup(rx.clone());
        }
        let (tx, rx) = watch::channel(None);
        map.insert(key.to_string(), rx);
        order.push_back((now + window_ms, key.to_string()));
        Claim::New(tx)
    }

    fn release(&self, key: &str) {
        self.inner.lock().map.remove(key);
    }
}

/// Messages handed to the engine, awaiting their confirms.
pub(crate) enum Pending {
    Sent { rx: oneshot::Receiver<bool>, routed: bool, id: String, claim: Option<watch::Sender<Option<Value>>> },
    Dup { rx: watch::Receiver<Option<Value>>, id: String },
}

/// Publish `msgs` to `exchange` (`""` = default). `queue` is set for the queue endpoint, whose
/// messages are routed by the queue name. Returns one `{id, routed, confirmed}` per message.
pub(crate) async fn publish_messages(api: &HttpApi, auth: &Auth, vh: &Arc<VHost>, exchange: &str, queue: Option<&str>, msgs: Vec<InMessage>) -> ApiResult<Vec<Value>> {
    let pending = send_messages(api, auth, vh, exchange, queue, msgs).await?;
    Ok(confirms(pending).await)
}

/// Validate, check permissions and hand the messages to the engine, in order.
pub(crate) async fn send_messages(api: &HttpApi, auth: &Auth, vh: &Arc<VHost>, exchange: &str, queue: Option<&str>, msgs: Vec<InMessage>) -> ApiResult<Vec<Pending>> {
    if msgs.is_empty() {
        return Err(ApiError::bad_request("no messages"));
    }
    if msgs.len() > api.config.max_batch {
        return Err(ApiError::bad_request(format!("a batch holds at most {} messages", api.config.max_batch)));
    }
    let broker = &api.broker;
    let ex = vh.exchange_or_404(exchange)?;
    if ex.internal {
        return Err(ApiError::forbidden(format!("ACCESS_REFUSED - cannot publish to internal exchange '{exchange}' in vhost '{}'", vh.name)));
    }
    let mut dedup_header = ex.kind == ExchangeKind::Dedup;
    if let Some(q) = queue {
        let q = vh.queue_for(q, Some(0))?;
        dedup_header = q.meta.arguments.get_bool("x-message-deduplication").unwrap_or(false);
    }
    let perm_name = if exchange.is_empty() { DEFAULT_EXCHANGE } else { exchange };
    let batch = msgs.len() > 1;
    let mut checked: HashSet<String> = HashSet::new();
    let mut built = Vec::with_capacity(msgs.len());
    for (i, m) in msgs.into_iter().enumerate() {
        let at = |e: ApiError| if batch { ApiError { message: format!("messages[{i}]: {}", e.message), ..e } } else { e };
        let rk = match (queue, &m.routing_key) {
            (Some(q), Some(rk)) if rk != q => return Err(at(ApiError::bad_request("routing_key must be empty or the queue name on the queue endpoint"))),
            (Some(q), _) => q.to_string(),
            (None, rk) => rk.clone().unwrap_or_default(),
        };
        if checked.insert(rk.clone()) {
            api.allow(auth, &vh.name, Op::Publish, "exchange", perm_name, exchange.is_empty().then_some(rk.as_str())).map_err(at)?;
            if ex.kind == ExchangeKind::Topic {
                broker.check_topic(&auth.user, &vh.name, exchange, &rk, Access::Write, None).map_err(|e| at(e.into()))?;
            }
        }
        let explicit = m.id.as_ref().is_some_and(|s| !s.is_empty()) || m.properties.get("message_id").is_some_and(|v| v.is_string());
        let mandatory = m.mandatory;
        let (mut msg, id) = message::build(m, exchange, &rk, &auth.user, broker.config.max_message_size).map_err(at)?;
        if dedup_header && msg.props.header("x-deduplication-header").is_none() {
            let mut p = msg.props.clone();
            p.headers.get_or_insert_default().insert("x-deduplication-header", FieldValue::str(id.clone()));
            msg = msg.with_props(&msg.exchange, &msg.routing_key, p);
        }
        built.push((msg, id, mandatory, explicit));
    }
    if broker.alarm.load(Ordering::Acquire) {
        let wait = async {
            while broker.alarm.load(Ordering::Acquire) {
                let n = broker.alarm_cleared.notified();
                if !broker.alarm.load(Ordering::Acquire) {
                    break;
                }
                let _ = tokio::time::timeout(Duration::from_secs(1), n).await;
            }
        };
        if tokio::time::timeout(Duration::from_secs(10), wait).await.is_err() {
            return Err(ApiError::unavailable("publishing is blocked by a resource alarm; retry later"));
        }
    }
    let mut pending = Vec::with_capacity(built.len());
    for (msg, id, mandatory, explicit) in built {
        let dkey = explicit.then(|| format!("{}\u{1f}{exchange}\u{1f}{}\u{1f}{id}", vh.name, msg.routing_key));
        let claim = match &dkey {
            Some(k) => match api.dedup.claim(k, api.config.dedup_window_ms) {
                Claim::Dup(rx) => {
                    pending.push(Pending::Dup { rx, id });
                    continue;
                }
                Claim::New(tx) => Some(tx),
            },
            None => None,
        };
        let (tx, rx) = oneshot::channel();
        let ticket = ConfirmTicket::new(move |ok| {
            let _ = tx.send(ok);
        });
        match vh.publish(exchange, msg, Some(ticket)).await {
            Ok(out) => {
                if let Some(t) = out.confirm {
                    t.settle(true);
                }
                if out.routed == 0 && mandatory {
                    vh.stats.returned.fetch_add(1, Ordering::Relaxed);
                }
                pending.push(Pending::Sent { rx, routed: out.routed > 0, id, claim });
            }
            Err(e) => {
                if let Some(k) = &dkey {
                    api.dedup.release(k);
                }
                return Err(e.into());
            }
        }
    }
    Ok(pending)
}

/// Wait for the confirms: one `{id, routed, confirmed}` (plus `duplicate`) per message.
pub(crate) async fn confirms(pending: Vec<Pending>) -> Vec<Value> {
    let mut out = Vec::with_capacity(pending.len());
    for p in pending {
        match p {
            Pending::Sent { rx, routed, id, claim } => {
                let confirmed = matches!(tokio::time::timeout(CONFIRM_TIMEOUT, rx).await, Ok(Ok(true)));
                let r = json!({"id": id, "routed": routed, "confirmed": confirmed});
                if let Some(c) = claim {
                    c.send_replace(Some(r.clone()));
                }
                out.push(r);
            }
            Pending::Dup { mut rx, id } => {
                let first = tokio::time::timeout(CONFIRM_TIMEOUT, rx.wait_for(|v| v.is_some())).await.ok().and_then(|r| r.ok().and_then(|v| v.clone()));
                let mut r = first.unwrap_or_else(|| json!({"id": id, "routed": true, "confirmed": false}));
                r["duplicate"] = true.into();
                out.push(r);
            }
        }
    }
    out
}

/// Run `f` under the request's `Idempotency-Key` (if any): successes are stored and replayed.
async fn idempotent(api: &HttpApi, auth: &Auth, path: &str, headers: &HeaderMap, body: &[u8], f: impl Future<Output = ApiResult<Value>>) -> Response {
    let key = match idempotency::header_key(headers) {
        Ok(k) => k,
        Err(e) => return e.into_response(),
    };
    let Some(key) = key else {
        return f.await.map(Json).into_response();
    };
    let skey = idempotency::scoped_key(&auth.identity(), "POST", path, &key);
    match api.idem.begin(&skey, &idempotency::fingerprint(body)).await {
        Err(e) => e.into_response(),
        Ok(Begin::Replay(s)) => (s.status, [("idempotent-replayed", "true")], Json(s.body)).into_response(),
        Ok(Begin::Fresh) => {
            let guard = AbortGuard { api, key: &skey, armed: true };
            let r = f.await;
            match &r {
                Ok(v) => api.idem.finish(&skey, StatusCode::OK, v).await,
                Err(_) => api.idem.abort(&skey),
            }
            guard.disarm();
            r.map(Json).into_response()
        }
    }
}

/// Releases an in-flight idempotency key if the request is cancelled (client went away).
struct AbortGuard<'a> {
    api: &'a HttpApi,
    key: &'a str,
    armed: bool,
}

impl AbortGuard<'_> {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for AbortGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.api.idem.abort(self.key);
        }
    }
}

fn shape(results: Vec<Value>, batch: bool) -> Value {
    if batch { json!({ "results": results }) } else { results.into_iter().next().unwrap_or(Value::Null) }
}

/// `POST /v1/vhosts/{vhost}/exchanges/{exchange}/publish`
pub async fn to_exchange(State(api): State<Arc<HttpApi>>, auth: Auth, P((vhost, exchange)): P<(String, String)>, headers: HeaderMap, body: Bytes) -> Response {
    let path = format!("/v1/vhosts/{vhost}/exchanges/{exchange}/publish");
    let exchange = if exchange == DEFAULT_EXCHANGE { String::new() } else { exchange };
    idempotent(&api, &auth, &path, &headers, &body, async {
        let vh = api.vhost(&auth, &vhost).await?;
        let (msgs, batch) = message::parse_publish(&body)?;
        Ok(shape(publish_messages(&api, &auth, &vh, &exchange, None, msgs).await?, batch))
    })
    .await
}

/// `POST /v1/vhosts/{vhost}/queues/{queue}/messages`
pub async fn to_queue(State(api): State<Arc<HttpApi>>, auth: Auth, P((vhost, queue)): P<(String, String)>, headers: HeaderMap, body: Bytes) -> Response {
    let path = format!("/v1/vhosts/{vhost}/queues/{queue}/messages");
    idempotent(&api, &auth, &path, &headers, &body, async {
        let vh = api.vhost(&auth, &vhost).await?;
        let (msgs, batch) = message::parse_publish(&body)?;
        Ok(shape(publish_messages(&api, &auth, &vh, "", Some(&queue), msgs).await?, batch))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_window() {
        let d = DedupWindow::default();
        let Claim::New(tx) = d.claim("a", 60_000) else { panic!("first claim") };
        let Claim::Dup(rx) = d.claim("a", 60_000) else { panic!("duplicate") };
        tx.send_replace(Some(json!(1)));
        assert_eq!(*rx.borrow(), Some(json!(1)));
        d.release("a");
        assert!(matches!(d.claim("a", 60_000), Claim::New(_)));
        // Expired entries make way.
        let d = DedupWindow::default();
        assert!(matches!(d.claim("b", 0), Claim::New(_)));
        assert!(matches!(d.claim("b", 0), Claim::New(_)));
    }
}
