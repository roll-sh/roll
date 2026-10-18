//! The `/management` node: RabbitMQ 4's HTTP-over-AMQP topology API used by the official
//! RabbitMQ AMQP 1.0 client libraries. Requests carry the method in `subject`, the path in
//! `to` and a body in an `amqp-value` section; responses carry the status code in `subject`
//! and the request's `message-id` as `correlation-id`.

use crate::address;
use crate::message::{Amqp1Message, from_091, prop, to_091};
use crate::types::Value;
use mq_core::auth::{Access, User};
use mq_core::error::{ACCESS_REFUSED, NOT_FOUND, PRECONDITION_FAILED};
use mq_core::exchange::{Binding, Dest, DestKind};
use mq_core::queue::{ConnId, QueueRef, QueueType};
use mq_core::value::{FieldTable, FieldValue};
use mq_core::vhost::DeclareQueue;
use mq_core::{AmqpError, Broker, VHost};
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub const ADDRESS: &str = "/management";

pub struct Ctx<'a> {
    pub broker: &'a Arc<Broker>,
    pub vhost: &'a Arc<VHost>,
    pub user: &'a User,
    pub conn: ConnId,
}

/// A failed request: HTTP status and explanation.
#[derive(Debug)]
struct Fail(&'static str, String);

type Reply = Result<(&'static str, Value), Fail>;

fn fail(status: &'static str, text: impl Into<String>) -> Fail {
    Fail(status, text.into())
}

/// Map a broker error to a status the way RabbitMQ does for the given operation.
fn status_of(e: &AmqpError, default: &'static str) -> Fail {
    let status = match e.code {
        ACCESS_REFUSED => "403",
        NOT_FOUND if default == "404" => "404",
        PRECONDITION_FAILED if default == "409" => "409",
        _ => "400",
    };
    Fail(status, e.text.clone())
}

/// Handle one request message, returning the response message.
pub async fn handle(ctx: &Ctx<'_>, req: &Amqp1Message) -> Amqp1Message {
    let message_id = req.prop(prop::MESSAGE_ID).clone();
    let method = req.prop_str(prop::SUBJECT).unwrap_or_default().to_string();
    let target = req.prop_str(prop::TO).unwrap_or_default().to_string();
    let body = req.value.clone().unwrap_or(Value::Null);
    let (status, body) = match route(ctx, &method, &target, body).await {
        Ok(r) => r,
        Err(Fail(s, text)) => {
            tracing::warn!(%method, %target, error = %text, "management request failed");
            (s, Value::String(text))
        }
    };
    let mut props = vec![Value::Null; prop::COUNT];
    props[prop::SUBJECT] = Value::str(status);
    props[prop::CORRELATION_ID] = message_id;
    Amqp1Message {
        properties: Some(props),
        application_properties: vec![(Value::str("http:response"), Value::str("1.1"))],
        value: Some(body),
        ..Default::default()
    }
}

/// Path segments and decoded query pairs of a request URI.
type Uri = (Vec<String>, Vec<(String, String)>);

fn parse_uri(uri: &str) -> Result<Uri, Fail> {
    let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
    let mut segs: Vec<String> = path.split('/').map(String::from).collect();
    if segs.first().map(String::as_str) != Some("") {
        return Err(fail("400", format!("failed to normalize URI '{uri}'")));
    }
    segs.remove(0);
    let mut q = Vec::new();
    for kv in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        let dec = |s: &str| address::decode(&s.replace('+', " ")).ok_or_else(|| fail("400", format!("failed to dissect query '{query}'")));
        q.push((dec(k)?, dec(v)?));
    }
    Ok((segs, q))
}

fn seg(s: &str) -> Result<String, Fail> {
    address::decode(s).ok_or_else(|| fail("400", format!("bad path segment '{s}'")))
}

async fn route(ctx: &Ctx<'_>, method: &str, uri: &str, body: Value) -> Reply {
    let (segs, query) = parse_uri(uri)?;
    let s: Vec<&str> = segs.iter().map(String::as_str).collect();
    match (method, s.as_slice()) {
        ("GET", ["queues", q]) => get_queue(ctx, &seg(q)?).await,
        ("PUT", ["queues", q]) => declare_queue(ctx, &seg(q)?, body).await,
        ("DELETE", ["queues", q, "messages"]) => purge_queue(ctx, &seg(q)?).await,
        ("DELETE", ["queues", q]) => delete_queue(ctx, &seg(q)?).await,
        ("GET", ["exchanges", x]) => get_exchange(ctx, &seg(x)?),
        ("PUT", ["exchanges", x]) => declare_exchange(ctx, &seg(x)?, body).await,
        ("DELETE", ["exchanges", x]) => delete_exchange(ctx, &seg(x)?).await,
        ("POST", ["bindings"]) => bind(ctx, body).await,
        ("DELETE", ["bindings", b]) => unbind(ctx, b).await,
        ("GET", ["bindings"]) => list_bindings(ctx, &query),
        ("PUT", ["auth", "tokens"]) => Err(fail("400", "OAuth 2 token refresh is not supported by this broker")),
        _ => Err(fail("400", format!("unsupported request {method} {uri}"))),
    }
}

fn check(ctx: &Ctx<'_>, access: Access, kind: &str, name: &str) -> Result<(), Fail> {
    ctx.broker.check(ctx.user, &ctx.vhost.name, access, kind, name).map_err(|e| fail("403", e.text))
}

fn prohibit_cr_lf(name: &str) -> Result<(), Fail> {
    if name.contains(['\r', '\n']) {
        return Err(fail("400", format!("Bad name '{name}': line feed and carriage return characters not allowed")));
    }
    Ok(())
}

fn rs(kind: &str, name: &str, vhost: &str) -> String {
    format!("{kind} '{name}' in vhost '{vhost}'")
}

/// AMQP map → 0-9-1 arguments (`args_amqp_to_amqpl`).
fn args_from(v: Option<&Value>) -> Result<FieldTable, Fail> {
    let Some(v) = v else { return Ok(FieldTable::new()) };
    let Value::Map(m) = v else { return Err(fail("400", format!("unsupported argument {v:?}"))) };
    let mut t = FieldTable::new();
    for (k, val) in m {
        let key = k.as_str().filter(|k| k.len() <= 255).ok_or_else(|| fail("400", format!("unsupported argument {k:?}")))?;
        let fv = to_091(val).ok_or_else(|| fail("400", format!("unsupported argument name or value {key}")))?;
        t.insert(key, fv);
    }
    Ok(t)
}

fn args_to(t: &FieldTable) -> Value {
    Value::Map(t.iter().map(|(k, v)| (Value::str(k), from_091(v))).collect())
}

fn map_fields(body: &Value, what: &str, allowed: &[&str]) -> Result<Vec<(String, Value)>, Fail> {
    let Value::Map(m) = body else { return Err(fail("400", format!("bad {what} body"))) };
    let mut out = Vec::new();
    for (k, v) in m {
        match k.as_str() {
            Some(k) if allowed.contains(&k) => out.push((k.to_string(), v.clone())),
            _ => return Err(fail("400", format!("bad {what} property {k:?}"))),
        }
    }
    Ok(out)
}

fn field<'a>(fields: &'a [(String, Value)], k: &str) -> Option<&'a Value> {
    fields.iter().find(|(n, _)| n == k).map(|(_, v)| v)
}

fn flag(fields: &[(String, Value)], k: &str, default: bool) -> Result<bool, Fail> {
    match field(fields, k) {
        None => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(v) => Err(fail("400", format!("bad property {k} = {v:?}"))),
    }
}

fn encode_queue(ctx: &Ctx<'_>, q: &QueueRef) -> Value {
    let s = &q.shared;
    let messages = match q.kind {
        QueueType::Stream => s.ready.load(Ordering::Relaxed),
        _ => s.messages(),
    };
    let node = Value::str(ctx.broker.config.node_name.clone());
    Value::Map(vec![
        (Value::str("leader"), node.clone()),
        (Value::str("message_count"), Value::Ulong(messages)),
        (Value::str("consumer_count"), Value::Uint(s.consumers.load(Ordering::Relaxed) as u32)),
        (Value::str("name"), Value::str(q.meta.name.clone())),
        (Value::str("vhost"), Value::str(q.meta.vhost.clone())),
        (Value::str("durable"), Value::Bool(q.meta.durable)),
        (Value::str("auto_delete"), Value::Bool(q.meta.auto_delete)),
        (Value::str("exclusive"), Value::Bool(q.meta.exclusive())),
        (Value::str("type"), Value::str(q.kind.name())),
        (Value::str("arguments"), args_to(&q.meta.arguments)),
        (Value::str("replicas"), Value::Array(vec![node])),
    ])
}

async fn get_queue(ctx: &Ctx<'_>, name: &str) -> Reply {
    let vh = &ctx.vhost.name;
    let any = [Access::Read, Access::Write, Access::Configure].iter().any(|a| ctx.broker.check(ctx.user, vh, *a, "queue", name).is_ok());
    if !any {
        check(ctx, Access::Read, "queue", name)?;
    }
    match ctx.vhost.queue(name) {
        Some(q) => Ok(("200", encode_queue(ctx, &q))),
        None => Err(fail("404", format!("{} not found", rs("queue", name, vh)))),
    }
}

async fn declare_queue(ctx: &Ctx<'_>, name: &str, body: Value) -> Reply {
    let f = map_fields(&body, "queue", &["durable", "exclusive", "auto_delete", "arguments"])?;
    let durable = flag(&f, "durable", true)?;
    let exclusive = flag(&f, "exclusive", false)?;
    let auto_delete = flag(&f, "auto_delete", false)?;
    let args = args_from(field(&f, "arguments"))?;
    if name.is_empty() {
        return Err(fail("400", "declare queue with empty name not allowed"));
    }
    prohibit_cr_lf(name)?;
    let vh = &ctx.vhost.name;
    if name.starts_with("amq.") {
        return Err(fail("403", format!("{} starts with reserved prefix 'amq.'", rs("queue", name, vh))));
    }
    check(ctx, Access::Configure, "queue", name)?;
    let existed = ctx.vhost.queue(name).is_some();
    if !existed && let Some(dlx) = args.get_str("x-dead-letter-exchange") {
        check(ctx, Access::Read, "queue", name)?;
        check(ctx, Access::Write, "exchange", dlx)?;
    }
    let d = DeclareQueue {
        name: name.into(),
        durable,
        exclusive_owner: exclusive.then_some(ctx.conn),
        auto_delete,
        args,
        passive: false,
    };
    match ctx.vhost.declare_queue(d).await {
        Ok(ok) => Ok((if existed { "200" } else { "201" }, encode_queue(ctx, &ok.queue))),
        Err(e) if e.text.contains("queue limit") => Err(fail("403", e.text)),
        Err(e) => Err(status_of(&e, "409")),
    }
}

async fn purge_queue(ctx: &Ctx<'_>, name: &str) -> Reply {
    check(ctx, Access::Read, "queue", name)?;
    let q = ctx.vhost.queue_for(name, Some(ctx.conn)).map_err(|e| status_of(&e, "404"))?;
    if q.kind == QueueType::Stream {
        return Err(fail("400", format!("purge not supported by {}", rs("queue", name, &ctx.vhost.name))));
    }
    let n = q.purge().await.map_err(|e| status_of(&e, "404"))?;
    Ok(("200", Value::Map(vec![(Value::str("message_count"), Value::Ulong(n))])))
}

async fn delete_queue(ctx: &Ctx<'_>, name: &str) -> Reply {
    prohibit_cr_lf(name)?;
    check(ctx, Access::Configure, "queue", name)?;
    let n = match ctx.vhost.delete_queue(name, false, false, Some(ctx.conn)).await {
        Ok(n) => n,
        Err(e) if e.code == NOT_FOUND => 0,
        Err(e) => return Err(status_of(&e, "400")),
    };
    Ok(("200", Value::Map(vec![(Value::str("message_count"), Value::Ulong(n))])))
}

fn get_exchange(ctx: &Ctx<'_>, name: &str) -> Reply {
    check(ctx, Access::Read, "exchange", name).or_else(|_| check(ctx, Access::Configure, "exchange", name))?;
    let x = ctx.vhost.exchange(name).ok_or_else(|| fail("404", format!("{} not found", rs("exchange", name, &ctx.vhost.name))))?;
    Ok((
        "200",
        Value::Map(vec![
            (Value::str("name"), Value::str(x.name.clone())),
            (Value::str("type"), Value::str(x.kind.name())),
            (Value::str("durable"), Value::Bool(x.durable)),
            (Value::str("auto_delete"), Value::Bool(x.auto_delete)),
            (Value::str("internal"), Value::Bool(x.internal)),
            (Value::str("arguments"), args_to(&x.args)),
        ]),
    ))
}

fn prohibit_default_exchange(name: &str) -> Result<(), Fail> {
    if name.is_empty() {
        return Err(fail("403", "operation not permitted on the default exchange"));
    }
    Ok(())
}

async fn declare_exchange(ctx: &Ctx<'_>, name: &str, body: Value) -> Reply {
    let f = map_fields(&body, "exchange", &["type", "durable", "auto_delete", "internal", "arguments"])?;
    let kind = match field(&f, "type") {
        None => "direct".to_string(),
        Some(v) => v.as_str().ok_or_else(|| fail("400", format!("bad exchange property type = {v:?}")))?.to_string(),
    };
    let durable = flag(&f, "durable", true)?;
    let auto_delete = flag(&f, "auto_delete", false)?;
    let internal = flag(&f, "internal", false)?;
    let args = args_from(field(&f, "arguments"))?;
    mq_core::exchange::ExchangeKind::parse(&kind, &args).map_err(|e| fail("400", e.text))?;
    prohibit_default_exchange(name)?;
    check(ctx, Access::Configure, "exchange", name)?;
    let vh = &ctx.vhost.name;
    if ctx.vhost.exchange(name).is_none() {
        prohibit_cr_lf(name)?;
        if name.starts_with("amq.") {
            return Err(fail("403", format!("{} starts with reserved prefix 'amq.'", rs("exchange", name, vh))));
        }
        if let Some(ae) = args.get_str("alternate-exchange") {
            check(ctx, Access::Read, "exchange", name)?;
            check(ctx, Access::Write, "exchange", ae)?;
        }
    }
    ctx.vhost.declare_exchange(name, &kind, durable, auto_delete, internal, args, false).await.map_err(|e| status_of(&e, "409"))?;
    Ok(("204", Value::Null))
}

async fn delete_exchange(ctx: &Ctx<'_>, name: &str) -> Reply {
    prohibit_cr_lf(name)?;
    prohibit_default_exchange(name)?;
    if name.starts_with("amq.") {
        return Err(fail("403", format!("{} starts with reserved prefix 'amq.'", rs("exchange", name, &ctx.vhost.name))));
    }
    check(ctx, Access::Configure, "exchange", name)?;
    match ctx.vhost.delete_exchange(name, false).await {
        Ok(()) => Ok(("204", Value::Null)),
        Err(e) if e.code == NOT_FOUND => Ok(("204", Value::Null)),
        Err(e) => Err(status_of(&e, "400")),
    }
}

fn binding_checks(ctx: &Ctx<'_>, source: &str, dest: &Dest, key: &str) -> Result<(), Fail> {
    for n in [source, dest.name.as_str()] {
        prohibit_cr_lf(n)?;
    }
    prohibit_default_exchange(source)?;
    if dest.kind == DestKind::Exchange {
        prohibit_default_exchange(&dest.name)?;
    }
    let dkind = if dest.kind == DestKind::Queue { "queue" } else { "exchange" };
    check(ctx, Access::Write, dkind, &dest.name)?;
    check(ctx, Access::Read, "exchange", source)?;
    if let Some(x) = ctx.vhost.exchange(source)
        && x.kind == mq_core::exchange::ExchangeKind::Topic
    {
        ctx.broker.check_topic(ctx.user, &ctx.vhost.name, source, key, Access::Read, None).map_err(|e| fail("403", e.text))?;
    }
    Ok(())
}

async fn bind(ctx: &Ctx<'_>, body: Value) -> Reply {
    let f = map_fields(&body, "binding", &["source", "destination_queue", "destination_exchange", "binding_key", "arguments"])?;
    let s = |k: &str| field(&f, k).and_then(|v| v.as_str()).map(String::from);
    let source = s("source").ok_or_else(|| fail("400", "missing binding field source"))?;
    let key = s("binding_key").unwrap_or_default();
    let args = args_from(field(&f, "arguments"))?;
    let dest = match (s("destination_queue"), s("destination_exchange")) {
        (Some(q), _) => Dest::queue(q),
        (None, Some(x)) => Dest::exchange(x),
        _ => return Err(fail("400", "missing binding field destination_queue or destination_exchange")),
    };
    binding_checks(ctx, &source, &dest, &key)?;
    ctx.vhost.bind(&source, dest, &key, args, Some(ctx.conn)).await.map_err(|e| status_of(&e, "400"))?;
    Ok(("204", Value::Null))
}

/// `args=` component of a binding location: empty for no arguments, else a stable hash.
fn args_hash(args: &FieldTable) -> String {
    use base64::Engine as _;
    if args.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(&str, &FieldValue)> = args.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    let mut b = bytes::BytesMut::new();
    FieldTable(pairs.into_iter().map(|(k, v)| (k.to_string(), v.clone())).collect()).encode(&mut b);
    let mut h: u32 = 0x811c9dc5;
    for x in b.iter() {
        h ^= *x as u32;
        h = h.wrapping_mul(0x01000193);
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(h.to_be_bytes())
}

fn location(b: &Binding) -> String {
    let dst = if b.dest.kind == DestKind::Queue { 'q' } else { 'e' };
    format!(
        "/bindings/src={};dst{dst}={};key={};args={}",
        address::encode(&b.source),
        address::encode(&b.dest.name),
        address::encode(&b.routing_key),
        args_hash(&b.args)
    )
}

fn matching(ctx: &Ctx<'_>, source: &str, dest: &Dest) -> Vec<Binding> {
    ctx.vhost.exchange(source).map(|x| x.bindings().into_iter().filter(|b| b.dest == *dest).collect()).unwrap_or_default()
}

async fn unbind(ctx: &Ctx<'_>, segment: &str) -> Reply {
    let bad = || fail("400", format!("bad binding path segment '{segment}'"));
    let mut src = None;
    let mut dest = None;
    let mut key = None;
    let mut hash = None;
    for part in segment.split(';') {
        let (k, v) = part.split_once('=').ok_or_else(bad)?;
        match k {
            "src" => src = Some(seg(v)?),
            "dstq" => dest = Some(Dest::queue(seg(v)?)),
            "dste" => dest = Some(Dest::exchange(seg(v)?)),
            "key" => key = Some(seg(v)?),
            "args" => hash = Some(v.to_string()),
            _ => return Err(bad()),
        }
    }
    let (Some(src), Some(dest), Some(key), Some(hash)) = (src, dest, key, hash) else { return Err(bad()) };
    if src.is_empty() || dest.name.is_empty() {
        return Err(bad());
    }
    binding_checks(ctx, &src, &dest, &key)?;
    if let Some(b) = matching(ctx, &src, &dest).into_iter().find(|b| b.routing_key == key && args_hash(&b.args) == hash) {
        ctx.vhost.unbind(&src, b.dest, &b.routing_key, b.args, Some(ctx.conn)).await.map_err(|e| status_of(&e, "400"))?;
    }
    Ok(("204", Value::Null))
}

fn list_bindings(ctx: &Ctx<'_>, query: &[(String, String)]) -> Reply {
    let q = |k: &str| query.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let (Some(src), Some(key)) = (q("src"), q("key")) else { return Err(fail("400", "missing 'src' or 'key' in query")) };
    let dest = match (q("dste"), q("dstq")) {
        (Some(x), _) => Dest::exchange(x),
        (None, Some(qn)) => Dest::queue(qn),
        _ => return Err(fail("400", "missing 'dste' or 'dstq' in query")),
    };
    binding_checks(ctx, &src, &dest, &key)?;
    let list = matching(ctx, &src, &dest)
        .into_iter()
        .filter(|b| b.routing_key == key)
        .map(|b| {
            let dk = if b.dest.kind == DestKind::Queue { "destination_queue" } else { "destination_exchange" };
            Value::Map(vec![
                (Value::str("source"), Value::str(b.source.clone())),
                (Value::str(dk), Value::str(b.dest.name.clone())),
                (Value::str("binding_key"), Value::str(b.routing_key.clone())),
                (Value::str("arguments"), args_to(&b.args)),
                (Value::str("location"), Value::str(location(&b))),
            ])
        })
        .collect();
    Ok(("200", Value::List(list)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_parsing() {
        let (s, q) = parse_uri("/bindings?src=e1&dstq=q%201&key=a+b").unwrap();
        assert_eq!(s, vec!["bindings"]);
        assert_eq!(q, vec![("src".into(), "e1".into()), ("dstq".into(), "q 1".into()), ("key".into(), "a b".into())]);
        assert!(parse_uri("queues/x").is_err());
    }

    #[test]
    fn binding_location() {
        let b = Binding { source: "e 1".into(), dest: Dest::queue("q/1"), routing_key: "k".into(), args: FieldTable::new() };
        assert_eq!(location(&b), "/bindings/src=e%201;dstq=q%2F1;key=k;args=");
        let mut a = FieldTable::new();
        a.insert("x", 1i64);
        assert!(!args_hash(&a).is_empty());
    }
}
