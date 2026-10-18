//! Stream and super stream management (`rabbit_stream_manager`): create/delete, partitions,
//! routing. Errors are stream protocol response codes.

use crate::proto::*;
use bytes::Bytes;
use mq_core::error::{ACCESS_REFUSED as AMQP_ACCESS_REFUSED, PRECONDITION_FAILED as AMQP_PRECONDITION_FAILED};
use mq_core::exchange::DestKind;
use mq_core::message::{Message, Properties};
use mq_core::queue::QueueRef;
use mq_core::value::{FieldTable, FieldValue};
use mq_core::vhost::{DeclareQueue, VHost};

/// Binding argument ordering super stream partitions.
pub const PARTITION_ORDER: &str = "x-stream-partition-order";

/// `rabbit_stream_utils:enforce_correct_name`: CR/LF stripped; empty and `amq.*` refused.
pub fn enforce_name(name: &str) -> Option<String> {
    let n: String = name.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    (!n.is_empty() && !n.starts_with("amq.")).then_some(n)
}

/// Where a stream lives: on this node, or on another cluster node (its stream listener).
pub enum Found {
    Here(QueueRef),
    Elsewhere(Option<std::net::SocketAddr>),
}

/// A stream by name, wherever it runs.
pub fn find(vh: &VHost, name: &str) -> Result<Found, u16> {
    use mq_core::cluster::Located;
    match vh.locate_queue(name) {
        Some(Located::Local(q)) if q.is_stream() && !q.is_deleted() => Ok(Found::Here(q)),
        Some(Located::Remote { listeners, .. }) if vh.queue(name).is_some_and(|q| q.kind == mq_core::queue::QueueType::Stream) => {
            Ok(Found::Elsewhere(listeners.get("stream").copied()))
        }
        // A clustered stream between owners (moving, failing over): ask again shortly.
        Some(Located::Local(q)) if q.kind == mq_core::queue::QueueType::Stream && !q.is_deleted() => Err(STREAM_NOT_AVAILABLE),
        _ => Err(STREAM_DOES_NOT_EXIST),
    }
}

/// A stream served by this node (publishing and reading need the stream's own node; clients
/// find it through `Metadata`).
pub fn lookup(vh: &VHost, name: &str) -> Result<QueueRef, u16> {
    match find(vh, name)? {
        Found::Here(q) => Ok(q),
        Found::Elsewhere(_) => Err(STREAM_NOT_AVAILABLE),
    }
}

/// Whether a stream exists anywhere in the cluster.
pub fn exists(vh: &VHost, name: &str) -> bool {
    find(vh, name).is_ok()
}

/// Stream protocol creation arguments as queue arguments (`stream_queue_arguments`), validated.
pub fn stream_args(args: &[(String, String)]) -> Result<FieldTable, ()> {
    let mut t = FieldTable::new();
    t.insert("x-queue-type", "stream");
    let long = |v: &str| v.trim().parse::<i64>().map_err(|_| ());
    for (k, v) in args {
        match k.as_str() {
            "max-length-bytes" => t.insert("x-max-length-bytes", long(v)?),
            "max-age" => t.insert("x-max-age", v.as_str()),
            "stream-max-segment-size-bytes" => t.insert("x-stream-max-segment-size-bytes", long(v)?),
            "initial-cluster-size" => {
                let n = long(v)?;
                if n <= 0 {
                    return Err(());
                }
                t.insert("x-initial-cluster-size", n)
            }
            "queue-leader-locator" => {
                if !matches!(v.as_str(), "client-local" | "balanced" | "random" | "least-leaders") {
                    return Err(());
                }
                t.insert("x-queue-leader-locator", v.as_str())
            }
            "stream-filter-size-bytes" => {
                let n = long(v)?;
                if !(16..=255).contains(&n) {
                    return Err(());
                }
                t.insert("x-stream-filter-size-bytes", n)
            }
            "stream-initial-offset" => {
                let n = long(v)?;
                if n < 0 {
                    return Err(());
                }
                t.insert("x-stream-initial-offset", n)
            }
            _ => {}
        }
    }
    Ok(t)
}

fn declare_code(e: &mq_core::AmqpError) -> u16 {
    match e.code {
        AMQP_PRECONDITION_FAILED => PRECONDITION_FAILED,
        AMQP_ACCESS_REFUSED => ACCESS_REFUSED,
        _ => PRECONDITION_FAILED,
    }
}

/// Create a stream: `STREAM_ALREADY_EXISTS` if an equivalent one exists, `PRECONDITION_FAILED`
/// if a different queue has the name.
pub async fn create_stream(vh: &VHost, name: &str, args: FieldTable) -> u16 {
    let existed = vh.queue(name).is_some();
    let r = vh.declare_queue(DeclareQueue { name: name.to_string(), durable: true, exclusive_owner: None, auto_delete: false, args, passive: false }).await;
    match r {
        Ok(_) if existed => STREAM_ALREADY_EXISTS,
        Ok(_) => OK,
        Err(e) => declare_code(&e),
    }
}

pub async fn delete_stream(vh: &VHost, name: &str) -> u16 {
    if !exists(vh, name) {
        return STREAM_DOES_NOT_EXIST;
    }
    match vh.delete_queue(name, false, false, None).await {
        Ok(_) => OK,
        Err(e) if e.code == mq_core::error::NOT_FOUND => STREAM_DOES_NOT_EXIST,
        Err(_) => INTERNAL_ERROR,
    }
}

fn order(args: &FieldTable) -> Option<i64> {
    let v = args.get(PARTITION_ORDER)?;
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Partition streams of a super stream, in partition order.
pub fn partitions(vh: &VHost, super_stream: &str) -> Result<Vec<String>, u16> {
    let ex = vh.exchange(super_stream).ok_or(STREAM_DOES_NOT_EXIST)?;
    let mut b: Vec<_> = ex.bindings().into_iter().filter(|b| b.dest.kind == DestKind::Queue && exists(vh, &b.dest.name)).collect();
    // Bindings without an order sort first, like Erlang's term order.
    b.sort_by_key(|b| order(&b.args).map_or((0, 0), |o| (1, o)));
    let mut out: Vec<String> = Vec::with_capacity(b.len());
    for x in b {
        if !out.contains(&x.dest.name) {
            out.push(x.dest.name);
        }
    }
    Ok(out)
}

/// Index of `stream` in `super_stream` for single active consumer placement: the binding's
/// order argument, else a trailing `-N` in the name, else -1.
pub fn partition_index(vh: &VHost, super_stream: &str, stream: &str) -> i64 {
    let Some(ex) = vh.exchange(super_stream) else { return -1 };
    let mut b: Vec<_> = ex.bindings().into_iter().filter(|b| b.dest.kind == DestKind::Queue && b.dest.name == stream).collect();
    b.sort_by_key(|b| order(&b.args).map_or((0, 0), |o| (1, o)));
    let Some(first) = b.first() else { return -1 };
    if let Some(o) = order(&first.args) {
        return o;
    }
    stream.rsplit_once('-').and_then(|(_, n)| n.parse().ok()).unwrap_or(-1)
}

/// Streams a routing key goes to (`rabbit_stream_manager:route`).
pub fn route(vh: &VHost, super_stream: &str, routing_key: &str) -> Result<Vec<String>, u16> {
    let ex = vh.exchange(super_stream).ok_or(STREAM_DOES_NOT_EXIST)?;
    let msg = Message::new(super_stream, routing_key, Properties::default(), Bytes::new());
    Ok(vh.route(&ex, &msg).into_iter().filter(|q| exists(vh, q)).collect())
}

/// Create a super stream: a direct exchange (`x-super-stream: true`) with one stream per
/// partition bound by its binding key and ordered by `x-stream-partition-order`.
pub async fn create_super_stream(vh: &VHost, name: &str, partitions: &[String], keys: &[String], args: &[(String, String)]) -> u16 {
    if partitions.len() != keys.len() {
        return PRECONDITION_FAILED;
    }
    let mut seen = std::collections::HashSet::new();
    if partitions.iter().any(|p| !seen.insert(p) || enforce_name(p).as_deref() != Some(p.as_str())) {
        return PRECONDITION_FAILED;
    }
    if vh.exchange(name).is_some() || partitions.iter().any(|p| vh.queue(p).is_some()) {
        return STREAM_ALREADY_EXISTS;
    }
    let mut args = args.to_vec();
    if !args.iter().any(|(k, _)| k == "queue-leader-locator") {
        args.push(("queue-leader-locator".into(), "balanced".into()));
    }
    let Ok(qargs) = stream_args(&args) else { return PRECONDITION_FAILED };
    let mut xargs = FieldTable::new();
    xargs.insert("x-super-stream", FieldValue::Bool(true));
    if let Err(e) = vh.declare_exchange(name, "direct", true, false, false, xargs, false).await {
        return declare_code(&e);
    }
    let mut created: Vec<&str> = Vec::new();
    let mut code = OK;
    for p in partitions {
        match create_stream(vh, p, qargs.clone()).await {
            OK => created.push(p),
            c => {
                code = c;
                break;
            }
        }
    }
    if code == OK {
        for (i, (p, k)) in partitions.iter().zip(keys).enumerate() {
            let mut bargs = FieldTable::new();
            bargs.insert(PARTITION_ORDER, FieldValue::I64(i as i64));
            if let Err(e) = vh.bind(name, mq_core::exchange::Dest::queue(p.as_str()), k, bargs, None).await {
                code = declare_code(&e);
                break;
            }
        }
    }
    if code != OK {
        for p in created {
            let _ = vh.delete_queue(p, false, false, None).await;
        }
        let _ = vh.delete_exchange(name, false).await;
    }
    code
}

/// Delete a super stream's partitions, then its exchange.
pub async fn delete_super_stream(vh: &VHost, name: &str, partitions: &[String]) -> u16 {
    let mut code = OK;
    for p in partitions {
        if exists(vh, p) && vh.delete_queue(p, false, false, None).await.is_err() {
            code = INTERNAL_ERROR;
        }
    }
    if vh.delete_exchange(name, false).await.is_err() {
        code = INTERNAL_ERROR;
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_args() {
        assert_eq!(enforce_name("a\r\nb").as_deref(), Some("ab"));
        assert_eq!(enforce_name("amq.x"), None);
        assert_eq!(enforce_name("\n"), None);
        let a = stream_args(&[("max-length-bytes".into(), "1000".into()), ("max-age".into(), "1h".into()), ("ignored".into(), "x".into())]).unwrap();
        assert_eq!(a.get_i64("x-max-length-bytes"), Some(1000));
        assert_eq!(a.get_str("x-max-age"), Some("1h"));
        assert_eq!(a.get_str("x-queue-type"), Some("stream"));
        assert!(stream_args(&[("initial-cluster-size".into(), "0".into())]).is_err());
        assert!(stream_args(&[("stream-filter-size-bytes".into(), "8".into())]).is_err());
        assert!(stream_args(&[("queue-leader-locator".into(), "nearest".into())]).is_err());
        assert!(stream_args(&[("max-length-bytes".into(), "lots".into())]).is_err());
    }
}
