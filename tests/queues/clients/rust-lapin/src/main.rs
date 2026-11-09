//! roll queues compatibility scenarios for Rust lapin.

use futures_util::StreamExt;
use lapin::options::*;
use lapin::types::{AMQPValue, FieldTable, LongString, ShortString};
use lapin::{BasicProperties, Channel, Connection, ConnectionProperties, ErrorKind, ExchangeKind};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, timeout};

type R<T = String> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Scenario = fn(Ctx) -> Pin<Box<dyn Future<Output = R> + Send>>;

#[derive(Clone)]
struct Ctx {
    url: String,
    p: String,
}

impl Ctx {
    fn name(&self, s: &str) -> ShortString {
        format!("{}.{s}", self.p).into()
    }
    async fn conn(&self) -> R<Connection> {
        Ok(Connection::connect(&self.url, ConnectionProperties::default()).await?)
    }
    async fn channel(&self) -> R<(Connection, Channel)> {
        let c = self.conn().await?;
        let ch = c.create_channel().await?;
        Ok((c, ch))
    }
}

fn s(v: &str) -> ShortString {
    v.into()
}

fn table(pairs: &[(&str, AMQPValue)]) -> FieldTable {
    let mut t = FieldTable::default();
    for (k, v) in pairs {
        t.insert((*k).into(), v.clone());
    }
    t
}

fn lstr(v: &str) -> AMQPValue {
    AMQPValue::LongString(LongString::from(v))
}

fn check(cond: bool, msg: impl Into<String>) -> R<()> {
    if cond { Ok(()) } else { Err(msg.into().into()) }
}

fn code(e: &lapin::Error) -> Option<u16> {
    match e.kind() {
        ErrorKind::ProtocolError(a) => Some(a.get_id()),
        _ => None,
    }
}

async fn declare(ch: &Channel, q: &ShortString, durable: bool, args: FieldTable) -> R<u32> {
    let ok = ch.queue_declare(q.clone(), QueueDeclareOptions { durable, ..Default::default() }, args).await?;
    Ok(ok.message_count())
}

async fn count(ch: &Channel, q: &ShortString) -> R<u32> {
    Ok(ch.queue_declare(q.clone(), QueueDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await?.message_count())
}

async fn publish(ch: &Channel, x: &str, key: &str, body: &[u8], props: BasicProperties) -> R<lapin::Confirmation> {
    Ok(ch.basic_publish(s(x), s(key), BasicPublishOptions::default(), body, props).await?.await?)
}

async fn get(ch: &Channel, q: &ShortString, no_ack: bool) -> R<Option<lapin::message::BasicGetMessage>> {
    Ok(ch.basic_get(q.clone(), BasicGetOptions { no_ack }).await?)
}

async fn get_wait(ch: &Channel, q: &ShortString) -> R<lapin::message::BasicGetMessage> {
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if let Some(m) = get(ch, q, false).await? {
            return Ok(m);
        }
        sleep(Duration::from_millis(50)).await;
    }
    Err(format!("no message on {q} within 5s").into())
}

async fn delete(ch: &Channel, q: &ShortString) -> R<()> {
    ch.queue_delete(q.clone(), QueueDeleteOptions::default()).await?;
    Ok(())
}

async fn next(c: &mut lapin::Consumer) -> R<lapin::message::Delivery> {
    Ok(timeout(Duration::from_secs(5), c.next()).await.map_err(|_| "consumer timed out")?.ok_or("consumer ended")??)
}

async fn connect(cx: Ctx) -> R {
    let (c, ch) = cx.channel().await?;
    check(c.status().connected() && ch.status().connected(), "not connected")?;
    c.close(200, s("bye")).await?;
    Ok(String::new())
}

async fn declare_(cx: Ctx) -> R {
    let (c, ch) = cx.channel().await?;
    let (x, q) = (cx.name("declare.x"), cx.name("declare.q"));
    ch.exchange_declare(x.clone(), ExchangeKind::Direct, ExchangeDeclareOptions { durable: true, ..Default::default() }, FieldTable::default()).await?;
    check(declare(&ch, &q, true, FieldTable::default()).await? == 0, "declare-ok count")?;
    ch.queue_bind(q.clone(), x.clone(), s("k"), QueueBindOptions::default(), FieldTable::default()).await?;
    ch.exchange_declare(x.clone(), ExchangeKind::Direct, ExchangeDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await?;
    let srv = ch.queue_declare(s(""), QueueDeclareOptions { exclusive: true, ..Default::default() }, FieldTable::default()).await?;
    check(srv.name().as_str().starts_with("amq.gen-"), format!("server-named {}", srv.name()))?;
    let e = declare(&ch, &q, false, FieldTable::default()).await.err().ok_or("inequivalent redeclare accepted")?;
    let e = e.downcast_ref::<lapin::Error>().and_then(code);
    check(e == Some(406), format!("expected 406, got {e:?}"))?;
    let ch = c.create_channel().await?;
    let e = count(&ch, &cx.name("missing")).await.err().ok_or("passive declare of a missing queue succeeded")?;
    check(e.downcast_ref::<lapin::Error>().and_then(code) == Some(404), "expected 404")?;
    let ch = c.create_channel().await?;
    ch.queue_unbind(q.clone(), x.clone(), s("k"), FieldTable::default()).await?;
    delete(&ch, &q).await?;
    ch.exchange_delete(x, ExchangeDeleteOptions::default()).await?;
    Ok(String::new())
}

async fn publish_confirms(cx: Ctx) -> R {
    let (_c, ch) = cx.channel().await?;
    let q = cx.name("confirms");
    declare(&ch, &q, true, FieldTable::default()).await?;
    ch.confirm_select(ConfirmSelectOptions::default()).await?;
    let mut pending = Vec::new();
    for i in 0..100 {
        pending.push(ch.basic_publish(s(""), q.clone(), BasicPublishOptions { mandatory: true, ..Default::default() }, format!("m{i}").as_bytes(), BasicProperties::default().with_delivery_mode(2)).await?);
    }
    for p in pending {
        check(p.await?.is_ack(), "nacked")?;
    }
    check(count(&ch, &q).await? == 100, "count")?;
    let c = ch.basic_publish(s(""), cx.name("nowhere"), BasicPublishOptions { mandatory: true, ..Default::default() }, b"x", BasicProperties::default()).await?.await?;
    let ret = c.take_message().ok_or("unroutable mandatory publish was not returned")?;
    check(ret.reply_code == 312, format!("return code {}", ret.reply_code))?;
    delete(&ch, &q).await?;
    Ok("100 acked, mandatory return ok".into())
}

async fn consume_ack(cx: Ctx) -> R {
    let (_c, ch) = cx.channel().await?;
    let q = cx.name("consume");
    declare(&ch, &q, false, FieldTable::default()).await?;
    ch.confirm_select(ConfirmSelectOptions::default()).await?;
    for i in 0..10 {
        publish(&ch, "", q.as_str(), format!("m{i}").as_bytes(), BasicProperties::default()).await?;
    }
    let mut cons = ch.basic_consume(q.clone(), s("c"), BasicConsumeOptions::default(), FieldTable::default()).await?;
    for i in 0..10 {
        let d = next(&mut cons).await?;
        check(d.data == format!("m{i}").as_bytes(), format!("order at {i}"))?;
        d.ack(BasicAckOptions::default()).await?;
    }
    ch.basic_cancel(s("c"), BasicCancelOptions::default()).await?;
    check(count(&ch, &q).await? == 0, "queue not empty after acks")?;
    delete(&ch, &q).await?;
    Ok(String::new())
}

async fn nack_requeue(cx: Ctx) -> R {
    let (_c, ch) = cx.channel().await?;
    let q = cx.name("nack");
    declare(&ch, &q, false, FieldTable::default()).await?;
    publish(&ch, "", q.as_str(), b"again", BasicProperties::default()).await?;
    let mut cons = ch.basic_consume(q.clone(), s("c"), BasicConsumeOptions::default(), FieldTable::default()).await?;
    let d = next(&mut cons).await?;
    check(!d.redelivered, "first delivery flagged redelivered")?;
    d.nack(BasicNackOptions { requeue: true, ..Default::default() }).await?;
    let d = next(&mut cons).await?;
    check(d.redelivered && d.data == b"again", "requeued delivery not redelivered")?;
    d.ack(BasicAckOptions::default()).await?;
    delete(&ch, &q).await?;
    Ok(String::new())
}

async fn reject_dlx(cx: Ctx) -> R {
    let (_c, ch) = cx.channel().await?;
    let (dlx, dlq, q) = (cx.name("dlx"), cx.name("dlq"), cx.name("work"));
    ch.exchange_declare(dlx.clone(), ExchangeKind::Fanout, ExchangeDeclareOptions::default(), FieldTable::default()).await?;
    declare(&ch, &dlq, false, FieldTable::default()).await?;
    ch.queue_bind(dlq.clone(), dlx.clone(), s(""), QueueBindOptions::default(), FieldTable::default()).await?;
    declare(&ch, &q, false, table(&[("x-dead-letter-exchange", lstr(dlx.as_str()))])).await?;
    publish(&ch, "", q.as_str(), b"bad", BasicProperties::default()).await?;
    let m = get_wait(&ch, &q).await?;
    m.delivery.reject(BasicRejectOptions { requeue: false }).await?;
    let m = get_wait(&ch, &dlq).await?;
    let death = m.delivery.properties.headers().as_ref().and_then(|h| h.inner().get("x-death").cloned()).ok_or("no x-death")?;
    let AMQPValue::FieldArray(arr) = death else { return Err("x-death is not an array".into()) };
    let Some(AMQPValue::FieldTable(d)) = arr.as_slice().first() else { return Err("empty x-death".into()) };
    let text = |k: &str| match d.inner().get(k) {
        Some(AMQPValue::LongString(v)) => v.to_string(),
        other => format!("{other:?}"),
    };
    check(text("reason") == "rejected" && text("queue") == q.as_str(), format!("x-death reason {} queue {}", text("reason"), text("queue")))?;
    check(m.delivery.data == b"bad", "body")?;
    m.delivery.ack(BasicAckOptions::default()).await?;
    delete(&ch, &q).await?;
    delete(&ch, &dlq).await?;
    ch.exchange_delete(dlx, ExchangeDeleteOptions::default()).await?;
    Ok(String::new())
}

async fn routed_counts(ch: &Channel, x: &str, msgs: Vec<(&str, BasicProperties)>) -> R<usize> {
    let mut unroutable = 0;
    for (k, props) in msgs {
        let c = ch.basic_publish(s(x), s(k), BasicPublishOptions { mandatory: true, ..Default::default() }, b"m", props).await?.await?;
        if c.take_message().is_some() {
            unroutable += 1;
        }
    }
    Ok(unroutable)
}

async fn topic(cx: Ctx) -> R {
    let (_c, ch) = cx.channel().await?;
    let (x, q1, q2) = (cx.name("topic"), cx.name("t1"), cx.name("t2"));
    ch.exchange_declare(x.clone(), ExchangeKind::Topic, ExchangeDeclareOptions::default(), FieldTable::default()).await?;
    for (q, k) in [(&q1, "a.*"), (&q2, "#.z")] {
        declare(&ch, q, false, FieldTable::default()).await?;
        ch.queue_bind(q.clone(), x.clone(), s(k), QueueBindOptions::default(), FieldTable::default()).await?;
    }
    ch.confirm_select(ConfirmSelectOptions::default()).await?;
    let keys = ["a.b", "a.b.z", "x.z", "a.z", "b.c"];
    let un = routed_counts(&ch, x.as_str(), keys.iter().map(|k| (*k, BasicProperties::default())).collect()).await?;
    let (a, b) = (count(&ch, &q1).await?, count(&ch, &q2).await?);
    check((a, b, un) == (2, 3, 1), format!("counts {a},{b} unroutable {un}"))?;
    delete(&ch, &q1).await?;
    delete(&ch, &q2).await?;
    ch.exchange_delete(x, ExchangeDeleteOptions::default()).await?;
    Ok(String::new())
}

async fn headers(cx: Ctx) -> R {
    let (_c, ch) = cx.channel().await?;
    let (x, qa, qb) = (cx.name("headers"), cx.name("hall"), cx.name("hany"));
    ch.exchange_declare(x.clone(), ExchangeKind::Headers, ExchangeDeclareOptions::default(), FieldTable::default()).await?;
    declare(&ch, &qa, false, FieldTable::default()).await?;
    declare(&ch, &qb, false, FieldTable::default()).await?;
    let all = table(&[("x-match", lstr("all")), ("format", lstr("pdf")), ("type", lstr("report"))]);
    let any = table(&[("x-match", lstr("any")), ("format", lstr("zip")), ("type", lstr("log"))]);
    ch.queue_bind(qa.clone(), x.clone(), s(""), QueueBindOptions::default(), all).await?;
    ch.queue_bind(qb.clone(), x.clone(), s(""), QueueBindOptions::default(), any).await?;
    ch.confirm_select(ConfirmSelectOptions::default()).await?;
    let h = |pairs: &[(&str, &str)]| BasicProperties::default().with_headers(table(&pairs.iter().map(|(k, v)| (*k, lstr(v))).collect::<Vec<_>>()));
    let msgs = vec![
        ("", h(&[("format", "pdf"), ("type", "report")])),
        ("", h(&[("format", "pdf")])),
        ("", h(&[("type", "log")])),
        ("", h(&[("format", "zip"), ("type", "x")])),
    ];
    let un = routed_counts(&ch, x.as_str(), msgs).await?;
    let (a, b) = (count(&ch, &qa).await?, count(&ch, &qb).await?);
    check((a, b, un) == (1, 2, 1), format!("counts {a},{b} unroutable {un}"))?;
    delete(&ch, &qa).await?;
    delete(&ch, &qb).await?;
    ch.exchange_delete(x, ExchangeDeleteOptions::default()).await?;
    Ok(String::new())
}

async fn tx(cx: Ctx) -> R {
    let (c, ch) = cx.channel().await?;
    let q = cx.name("tx");
    declare(&ch, &q, false, FieldTable::default()).await?;
    ch.tx_select().await?;
    ch.basic_publish(s(""), q.clone(), BasicPublishOptions::default(), b"rolled back", BasicProperties::default()).await?;
    ch.tx_rollback().await?;
    check(count(&ch, &q).await? == 0, "rollback left a message")?;
    for _ in 0..2 {
        ch.basic_publish(s(""), q.clone(), BasicPublishOptions::default(), b"committed", BasicProperties::default()).await?;
    }
    ch.tx_commit().await?;
    check(count(&ch, &q).await? == 2, "commit count")?;
    let m = get(&ch, &q, false).await?.ok_or("get")?;
    m.delivery.ack(BasicAckOptions::default()).await?;
    ch.tx_rollback().await?;
    ch.close(200, s("bye")).await?;
    let ch = c.create_channel().await?;
    check(count(&ch, &q).await? == 2, "ack was not rolled back")?;
    ch.tx_select().await?;
    let m = get(&ch, &q, false).await?.ok_or("get")?;
    m.delivery.ack(BasicAckOptions::default()).await?;
    ch.tx_commit().await?;
    ch.close(200, s("bye")).await?;
    let ch = c.create_channel().await?;
    check(count(&ch, &q).await? == 1, "ack was not committed")?;
    delete(&ch, &q).await?;
    Ok(String::new())
}

async fn prefetch(cx: Ctx) -> R {
    let (c, ch) = cx.channel().await?;
    let q = cx.name("prefetch");
    declare(&ch, &q, false, FieldTable::default()).await?;
    ch.confirm_select(ConfirmSelectOptions::default()).await?;
    for i in 0..20 {
        publish(&ch, "", q.as_str(), i.to_string().as_bytes(), BasicProperties::default()).await?;
    }
    ch.basic_qos(5, BasicQosOptions::default()).await?;
    let mut cons = ch.basic_consume(q.clone(), s("c"), BasicConsumeOptions::default(), FieldTable::default()).await?;
    let mut got = Vec::new();
    let drain = async |cons: &mut lapin::Consumer, got: &mut Vec<lapin::message::Delivery>| {
        while let Ok(Some(Ok(d))) = timeout(Duration::from_secs(1), cons.next()).await {
            got.push(d);
        }
    };
    drain(&mut cons, &mut got).await;
    check(got.len() == 5, format!("received {} with prefetch 5", got.len()))?;
    got[4].ack(BasicAckOptions { multiple: true }).await?;
    drain(&mut cons, &mut got).await;
    check(got.len() == 10, format!("received {} after acking 5", got.len()))?;
    ch.close(200, s("bye")).await?;
    let ch = c.create_channel().await?;
    delete(&ch, &q).await?;
    Ok(String::new())
}

async fn ttl(cx: Ctx) -> R {
    let (_c, ch) = cx.channel().await?;
    let (q1, q2) = (cx.name("ttlq"), cx.name("ttlm"));
    declare(&ch, &q1, false, table(&[("x-message-ttl", AMQPValue::LongInt(200))])).await?;
    declare(&ch, &q2, false, FieldTable::default()).await?;
    ch.confirm_select(ConfirmSelectOptions::default()).await?;
    publish(&ch, "", q1.as_str(), b"x", BasicProperties::default()).await?;
    publish(&ch, "", q2.as_str(), b"short", BasicProperties::default().with_expiration(s("200"))).await?;
    publish(&ch, "", q2.as_str(), b"long", BasicProperties::default().with_expiration(s("60000"))).await?;
    sleep(Duration::from_millis(700)).await;
    check(get(&ch, &q1, true).await?.is_none(), "queue TTL did not expire")?;
    let m = get(&ch, &q2, true).await?.ok_or("per-message TTL expired the long message")?;
    check(m.delivery.data == b"long", "per-message TTL kept the short message")?;
    delete(&ch, &q1).await?;
    delete(&ch, &q2).await?;
    Ok(String::new())
}

async fn priority(cx: Ctx) -> R {
    let (_c, ch) = cx.channel().await?;
    let q = cx.name("prio");
    declare(&ch, &q, false, table(&[("x-max-priority", AMQPValue::ShortShortUInt(10))])).await?;
    ch.confirm_select(ConfirmSelectOptions::default()).await?;
    for p in [1u8, 9, 5, 0, 7] {
        publish(&ch, "", q.as_str(), p.to_string().as_bytes(), BasicProperties::default().with_priority(p)).await?;
    }
    let mut got = Vec::new();
    for _ in 0..5 {
        got.push(String::from_utf8(get(&ch, &q, true).await?.ok_or("get")?.delivery.data)?);
    }
    check(got == ["9", "7", "5", "1", "0"], format!("order {got:?}"))?;
    delete(&ch, &q).await?;
    Ok(String::new())
}

async fn exclusive_autodelete(cx: Ctx) -> R {
    let (a, cha) = cx.channel().await?;
    let q = cha.queue_declare(s(""), QueueDeclareOptions { exclusive: true, ..Default::default() }, FieldTable::default()).await?.name().clone();
    let (b, chb) = cx.channel().await?;
    let e = count(&chb, &q).await.err().ok_or("exclusive queue visible to another connection")?;
    check(e.downcast_ref::<lapin::Error>().and_then(code) == Some(405), format!("expected 405, got {e}"))?;
    a.close(200, s("bye")).await?;
    let chb = b.create_channel().await?;
    let e = count(&chb, &q).await.err().ok_or("exclusive queue survived its connection")?;
    check(e.downcast_ref::<lapin::Error>().and_then(code) == Some(404), "expected 404")?;
    let chb = b.create_channel().await?;
    let ad = cx.name("autodel");
    chb.queue_declare(ad.clone(), QueueDeclareOptions { auto_delete: true, ..Default::default() }, FieldTable::default()).await?;
    let _cons = chb.basic_consume(ad.clone(), s("c"), BasicConsumeOptions::default(), FieldTable::default()).await?;
    chb.basic_cancel(s("c"), BasicCancelOptions::default()).await?;
    let e = count(&chb, &ad).await.err().ok_or("auto-delete queue survived its last consumer")?;
    check(e.downcast_ref::<lapin::Error>().and_then(code) == Some(404), "expected 404")?;
    Ok(String::new())
}

async fn direct_reply_to(cx: Ctx) -> R {
    let c = cx.conn().await?;
    let (client, server) = (c.create_channel().await?, c.create_channel().await?);
    let q = cx.name("rpc");
    declare(&server, &q, false, FieldTable::default()).await?;
    let mut requests = server.basic_consume(q.clone(), s("srv"), BasicConsumeOptions::default(), FieldTable::default()).await?;
    let srv = server.clone();
    tokio::spawn(async move {
        while let Some(Ok(d)) = requests.next().await {
            let reply_to = d.properties.reply_to().clone().unwrap_or_default();
            let corr = d.properties.correlation_id().clone().unwrap_or_default();
            let body = d.data.to_ascii_uppercase();
            let _ = srv.basic_publish(s(""), reply_to, BasicPublishOptions::default(), &body, BasicProperties::default().with_correlation_id(corr)).await;
            let _ = d.ack(BasicAckOptions::default()).await;
        }
    });
    let mut replies = client.basic_consume(s("amq.rabbitmq.reply-to"), s("cli"), BasicConsumeOptions { no_ack: true, ..Default::default() }, FieldTable::default()).await?;
    let props = BasicProperties::default().with_reply_to(s("amq.rabbitmq.reply-to")).with_correlation_id(s("c1"));
    client.basic_publish(s(""), q.clone(), BasicPublishOptions::default(), b"ping", props).await?;
    let r = next(&mut replies).await?;
    check(r.data == b"PING" && r.properties.correlation_id().as_ref().map(|c| c.as_str()) == Some("c1"), "bad reply")?;
    delete(&server, &q).await?;
    Ok(String::new())
}

async fn basic_get(cx: Ctx) -> R {
    let (_c, ch) = cx.channel().await?;
    let q = cx.name("get");
    declare(&ch, &q, false, FieldTable::default()).await?;
    ch.confirm_select(ConfirmSelectOptions::default()).await?;
    for i in 0..3 {
        publish(&ch, "", q.as_str(), i.to_string().as_bytes(), BasicProperties::default()).await?;
    }
    let m = get(&ch, &q, false).await?.ok_or("get")?;
    check(m.message_count == 2 && m.delivery.data == b"0", format!("get-ok message_count {}", m.message_count))?;
    m.delivery.ack(BasicAckOptions::default()).await?;
    get(&ch, &q, true).await?.ok_or("get 2")?;
    get(&ch, &q, true).await?.ok_or("get 3")?;
    check(get(&ch, &q, false).await?.is_none(), "expected get-empty")?;
    delete(&ch, &q).await?;
    Ok(String::new())
}

async fn cancel_notify(cx: Ctx) -> R {
    let c = cx.conn().await?;
    let (ch, other) = (c.create_channel().await?, c.create_channel().await?);
    let q = cx.name("cancel");
    declare(&ch, &q, false, FieldTable::default()).await?;
    let mut cons = ch.basic_consume(q.clone(), s("c"), BasicConsumeOptions::default(), FieldTable::default()).await?;
    delete(&other, &q).await?;
    // lapin ends the consumer stream when the broker cancels it.
    let end = timeout(Duration::from_secs(5), cons.next()).await.map_err(|_| "no basic.cancel within 5s")?;
    check(end.is_none(), "expected the consumer stream to end")?;
    check(ch.status().connected(), "channel closed instead of consumer cancelled")?;
    Ok(String::new())
}

/// Reads the server's connection.start and checks its capabilities table (lapin keeps the server
/// properties private), then registers lapin's blocked/unblocked event listener.
async fn connection_blocked(cx: Ctx) -> R {
    let addr = cx.url.split('@').nth(1).and_then(|r| r.split('/').next()).ok_or("bad url")?.to_string();
    let mut sock = tokio::net::TcpStream::connect(&addr).await?;
    sock.write_all(b"AMQP\x00\x00\x09\x01").await?;
    let mut buf = vec![0u8; 4096];
    let mut n = 0;
    while n < 7 || n < 8 + u32::from_be_bytes([buf[3], buf[4], buf[5], buf[6]]) as usize {
        let r = timeout(Duration::from_secs(5), sock.read(&mut buf[n..])).await??;
        check(r > 0, "connection closed during connection.start")?;
        n += r;
    }
    let cap = b"\x12connection.blockedt\x01";
    check(buf[..n].windows(cap.len()).any(|w| w == cap), "connection.blocked capability missing")?;
    let c = cx.conn().await?;
    let _events = c.events_listener();
    c.close(200, s("bye")).await?;
    Ok(String::new())
}

async fn heartbeat(cx: Ctx) -> R {
    let sep = if cx.url.contains('?') { '&' } else { '?' };
    let c = Connection::connect(&format!("{}{sep}heartbeat=2", cx.url), ConnectionProperties::default()).await?;
    check(c.configuration().heartbeat() == 2, format!("negotiated heartbeat {}", c.configuration().heartbeat()))?;
    sleep(Duration::from_secs(7)).await;
    check(c.status().connected(), "connection closed while idle")?;
    let ch = c.create_channel().await?;
    ch.queue_declare(s(""), QueueDeclareOptions { exclusive: true, ..Default::default() }, FieldTable::default()).await?;
    Ok("2s heartbeat, idle 7s".into())
}

async fn large_message(cx: Ctx) -> R {
    let (_c, ch) = cx.channel().await?;
    let q = cx.name("large");
    declare(&ch, &q, false, FieldTable::default()).await?;
    ch.confirm_select(ConfirmSelectOptions::default()).await?;
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let body: Vec<u8> = (0..5 * 1024 * 1024)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    check(publish(&ch, "", q.as_str(), &body, BasicProperties::default()).await?.is_ack(), "nacked")?;
    let m = get(&ch, &q, true).await?.ok_or("get")?;
    check(m.delivery.data == body, "body mismatch")?;
    delete(&ch, &q).await?;
    Ok("5 MiB".into())
}

async fn throughput(cx: Ctx) -> R {
    const N: usize = 10_000;
    let (_c, ch) = cx.channel().await?;
    let q = cx.name("tput");
    declare(&ch, &q, false, FieldTable::default()).await?;
    let body = [b'x'; 100];
    let t0 = Instant::now();
    for _ in 0..N {
        ch.basic_publish(s(""), q.clone(), BasicPublishOptions::default(), &body, BasicProperties::default()).await?;
    }
    let publish = t0.elapsed();
    ch.basic_qos(500, BasicQosOptions::default()).await?;
    let got = Arc::new(AtomicUsize::new(0));
    let mut cons = ch.basic_consume(q.clone(), s("c"), BasicConsumeOptions::default(), FieldTable::default()).await?;
    let t1 = Instant::now();
    while got.load(Ordering::Relaxed) < N {
        let d = timeout(Duration::from_secs(10), cons.next()).await.map_err(|_| format!("timed out after {}", got.load(Ordering::Relaxed)))?.ok_or("consumer ended")??;
        let n = got.fetch_add(1, Ordering::Relaxed) + 1;
        if n % 100 == 0 || n == N {
            d.ack(BasicAckOptions { multiple: true }).await?;
        }
    }
    let consume = t1.elapsed();
    ch.basic_cancel(s("c"), BasicCancelOptions::default()).await?;
    delete(&ch, &q).await?;
    Ok(format!("publish {:.0} msg/s, consume {:.0} msg/s", N as f64 / publish.as_secs_f64(), N as f64 / consume.as_secs_f64()))
}

macro_rules! scenarios {
    ($($name:literal => $f:ident),* $(,)?) => {
        [$(($name, (|cx| Box::pin($f(cx))) as Scenario)),*]
    };
}

#[tokio::main]
async fn main() {
    let url = std::env::var("AMQP_URL").unwrap_or_else(|_| "amqp://guest:guest@localhost:5672/%2F".into());
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let cx = Ctx { url, p: format!("lapin-{:06x}", nanos & 0xff_ffff) };
    let only: Vec<String> = std::env::args().skip(1).collect();
    let all = scenarios![
        "connect" => connect, "declare" => declare_, "publish_confirms" => publish_confirms,
        "consume_ack" => consume_ack, "nack_requeue" => nack_requeue, "reject_dlx" => reject_dlx,
        "topic" => topic, "headers" => headers, "tx" => tx, "prefetch" => prefetch, "ttl" => ttl,
        "priority" => priority, "exclusive_autodelete" => exclusive_autodelete,
        "direct_reply_to" => direct_reply_to, "basic_get" => basic_get, "cancel_notify" => cancel_notify,
        "connection_blocked" => connection_blocked, "heartbeat" => heartbeat,
        "large_message" => large_message, "throughput" => throughput,
    ];
    let mut failed = 0;
    for (name, f) in all {
        if !only.is_empty() && !only.iter().any(|o| o == name) {
            continue;
        }
        let t = Instant::now();
        let limit = if name == "throughput" { 30 } else { 15 };
        match timeout(Duration::from_secs(limit), f(cx.clone())).await {
            Ok(Ok(detail)) => println!("PASS {name} {}({:.1}s)", if detail.is_empty() { String::new() } else { format!("{detail} ") }, t.elapsed().as_secs_f64()),
            Ok(Err(e)) => {
                failed += 1;
                println!("FAIL {name}: {e}");
            }
            Err(_) => {
                failed += 1;
                println!("FAIL {name}: timed out after {limit}s");
            }
        }
    }
    std::process::exit(if failed > 0 { 1 } else { 0 });
}
