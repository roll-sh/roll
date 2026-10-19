//! Raw-frame STOMP tests against an in-process broker: every frame type, ack modes,
//! transactions, receipts, heart-beats, destinations and errors.

mod common;

use common::{Client, Harness, wait_until};
use mq_core::auth::{Permission, User};
use mq_core::value::FieldTable;
use mq_core::vhost::DeclareQueue;
use std::time::Duration;

/// A frame to send and the ERROR `message` header and body it must produce.
type Case<'a> = (&'a [(&'a str, &'a str)], &'a str, &'a str);
type CommandCase<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str, &'a str);

#[tokio::test]
async fn connect_negotiates_version_and_registers() {
    let h = Harness::start().await;
    let mut c = Client::raw_connect(h.addr).await;
    let f = c.login(&[("accept-version", "1.0,1.1,1.2"), ("host", "/"), ("login", "guest"), ("passcode", "guest"), ("heart-beat", "0,0")]).await;
    assert_eq!(f.command, "CONNECTED");
    assert_eq!(f.hh("version"), "1.2");
    assert_eq!(f.hh("heart-beat"), "0,0");
    assert!(f.hh("session").starts_with("session-"));
    assert!(f.hh("server").starts_with("RabbitMQ/"));
    wait_until(|| h.broker.connections().iter().any(|c| c.protocol == "STOMP 1.2" && c.user == "guest")).await;
    assert_eq!(h.broker.channels().len(), 1);

    // No accept-version means 1.0; the STOMP command works like CONNECT; login defaults to guest.
    let mut c = Client::raw_connect(h.addr).await;
    c.send("STOMP", &[], b"").await;
    let f = c.expect("CONNECTED").await;
    assert_eq!(f.hh("version"), "1.0");
    let mut c = Client::raw_connect(h.addr).await;
    let f = c.login(&[("accept-version", "1.1,2.0")]).await;
    assert_eq!(f.hh("version"), "1.1");

    drop(c);
    wait_until(|| h.broker.connections().len() == 2).await;
}

#[tokio::test]
async fn connect_errors_close_the_connection() {
    let h = Harness::start().await;
    let cases: [Case; 4] = [
        (&[("accept-version", "2.0")], "Version mismatch", "Supported versions are 1.0,1.1,1.2\n"),
        (&[("accept-version", "1.2"), ("login", "guest"), ("passcode", "nope")], "Bad CONNECT", "Access refused for user 'guest'"),
        (&[("accept-version", "1.2"), ("login", "nobody"), ("passcode", "x")], "Bad CONNECT", "Access refused for user 'nobody'"),
        (&[("accept-version", "1.2"), ("host", "nope"), ("login", "guest"), ("passcode", "guest")], "Bad CONNECT", "Virtual host 'nope' access denied"),
    ];
    for (headers, message, body) in cases {
        let mut c = Client::raw_connect(h.addr).await;
        let f = c.login(headers).await;
        assert_eq!(f.command, "ERROR");
        assert_eq!(f.hh("message"), message);
        assert_eq!(f.hh("content-type"), "text/plain");
        assert_eq!(f.text(), body);
        assert!(c.closed().await);
    }
    // Anything before CONNECT.
    let mut c = Client::raw_connect(h.addr).await;
    c.send("SEND", &[("destination", "/queue/x")], b"hi").await;
    let f = c.expect("ERROR").await;
    assert_eq!(f.hh("message"), "Illegal command");
    assert_eq!(f.text(), "You must log in using CONNECT first");
    assert!(c.closed().await);
    assert!(h.broker.connections().is_empty());
}

#[tokio::test]
async fn implicit_connect_uses_default_credentials() {
    let h = Harness::with(mq_stomp::Config { implicit_connect: true, ..Default::default() }, false).await;
    let mut c = Client::raw_connect(h.addr).await;
    c.escapes = false;
    c.send("SEND", &[("destination", "/queue/implicit"), ("receipt", "r1")], b"x").await;
    let f = c.expect("RECEIPT").await;
    assert_eq!(f.hh("receipt-id"), "r1");
    assert_eq!(h.queue_len("implicit"), Some(1));
}

#[tokio::test]
async fn send_subscribe_and_message_headers() {
    let h = Harness::start().await;
    let mut c = h.client().await;
    c.sync("SUBSCRIBE", &[("destination", "/queue/work"), ("id", "sub-0")], b"").await;
    let q = h.vhost().queue("work").expect("declared on subscribe");
    assert!(q.meta.durable && !q.meta.auto_delete && !q.meta.exclusive(), "/queue/ is durable by default");
    let body = b"bin\0ary";
    c.send(
        "SEND",
        &[
            ("destination", "/queue/work"),
            ("content-type", "application/octet-stream"),
            ("content-length", "7"),
            ("persistent", "true"),
            ("priority", "3"),
            ("correlation-id", "c-1"),
            ("reply-to", "/queue/answers"),
            ("amqp-message-id", "m-1"),
            ("app-id", "tests"),
            ("type", "kind"),
            ("timestamp", "1700000000"),
            ("x-trace", "abc"),
            ("receipt", "sent"),
        ],
        body,
    )
    .await;
    let mut got = [c.recv().await, c.recv().await];
    got.sort_by_key(|f| f.command.clone());
    let (m, r) = (&got[0], &got[1]);
    assert_eq!(r.command, "RECEIPT");
    assert_eq!(r.hh("receipt-id"), "sent");
    assert_eq!(m.command, "MESSAGE");
    assert_eq!(m.body, body);
    assert_eq!(m.hh("subscription"), "sub-0");
    assert_eq!(m.hh("destination"), "/queue/work");
    assert_eq!(m.hh("redelivered"), "false");
    let id = m.hh("message-id");
    assert!(id.starts_with("T_sub-0@@session-") && id.ends_with("@@0"), "{id}");
    assert!(m.h("ack").is_none(), "auto ack has no ack header");
    for (k, v) in [
        ("content-type", "application/octet-stream"),
        ("persistent", "true"),
        ("priority", "3"),
        ("correlation-id", "c-1"),
        ("reply-to", "/queue/answers"),
        ("amqp-message-id", "m-1"),
        ("app-id", "tests"),
        ("type", "kind"),
        ("timestamp", "1700000000"),
        ("x-trace", "abc"),
        ("content-length", "7"),
    ] {
        assert_eq!(m.h(k), Some(v), "{k}");
    }
}

#[tokio::test]
async fn header_escaping_round_trips() {
    let h = Harness::start().await;
    let mut c = h.client().await;
    c.sync("SUBSCRIBE", &[("destination", "/queue/esc"), ("id", "s")], b"").await;
    c.send("SEND", &[("destination", "/queue/esc"), ("we\\cird", "a\\nb\\\\c\\cd")], b"").await;
    let m = c.expect("MESSAGE").await;
    assert_eq!(m.h("we:ird"), Some("a\nb\\c:d"));
    // An undefined escape is a protocol error in 1.2.
    c.send("SEND", &[("destination", "/queue/esc"), ("k", "bad\\t")], b"").await;
    assert_eq!(c.expect("ERROR").await.hh("message"), "Invalid frame");
    assert!(c.closed().await);
}

#[tokio::test]
async fn client_individual_prefetch_and_nack() {
    let h = Harness::start().await;
    let mut c = h.client().await;
    c.sync("SUBSCRIBE", &[("destination", "/queue/ind"), ("id", "s"), ("ack", "client-individual"), ("prefetch-count", "1")], b"").await;
    for i in 0..2 {
        c.send("SEND", &[("destination", "/queue/ind")], format!("m{i}").as_bytes()).await;
    }
    let m0 = c.expect("MESSAGE").await;
    assert_eq!(m0.text(), "m0");
    assert_eq!(m0.h("ack"), m0.h("message-id"), "1.2 adds the ack header in client modes");
    assert!(c.try_recv(300).await.is_none(), "prefetch 1 holds the second message back");
    // NACK requeues by default.
    c.send("NACK", &[("id", &Client::ack_id(&m0))], b"").await;
    let again = c.expect("MESSAGE").await;
    assert_eq!((again.text().as_str(), again.hh("redelivered")), ("m0", "true"));
    c.send("ACK", &[("id", &Client::ack_id(&again))], b"").await;
    let m1 = c.expect("MESSAGE").await;
    assert_eq!(m1.text(), "m1");
    // NACK with requeue:false drops it (no dead-letter exchange).
    c.sync("NACK", &[("id", &Client::ack_id(&m1)), ("requeue", "false")], b"").await;
    wait_until(|| h.queue_len("ind") == Some(0)).await;
    // Acking it again: unknown delivery tag.
    c.send("ACK", &[("id", &Client::ack_id(&m1)), ("receipt", "r")], b"").await;
    let e = c.expect("ERROR").await;
    assert_eq!(e.hh("message"), "Message not found");
    assert_eq!(e.text(), "unknown delivery tag 2\n");
    assert_eq!(e.h("receipt-id"), Some("r"));
    assert!(c.closed().await);
}

#[tokio::test]
async fn client_ack_is_cumulative() {
    let h = Harness::start().await;
    let mut c = h.client().await;
    c.sync("SUBSCRIBE", &[("destination", "/queue/cum"), ("id", "s"), ("ack", "client")], b"").await;
    for i in 0..3 {
        c.send("SEND", &[("destination", "/queue/cum")], format!("m{i}").as_bytes()).await;
    }
    let mut last = None;
    for _ in 0..3 {
        last = Some(c.expect("MESSAGE").await);
    }
    wait_until(|| h.queue_len("cum") == Some(3)).await;
    c.sync("ACK", &[("id", &Client::ack_id(&last.unwrap()))], b"").await;
    wait_until(|| h.queue_len("cum") == Some(0)).await;
}

#[tokio::test]
async fn ack_headers_per_version() {
    let h = Harness::start().await;
    for version in ["1.0", "1.1"] {
        let mut c = Client::connect(h.addr, version).await;
        let q = format!("/queue/v{version}");
        c.sync("SUBSCRIBE", &[("destination", &q), ("ack", "client"), ("id", "s")], b"").await;
        c.send("SEND", &[("destination", &q)], b"x").await;
        let m = c.expect("MESSAGE").await;
        assert!(m.h("ack").is_none(), "only 1.2 has the ack header");
        let id = m.hh("message-id").to_string();
        c.sync("ACK", &[("message-id", &id), ("subscription", "s")], b"").await;
        wait_until(|| h.queue_len(&format!("v{version}")) == Some(0)).await;
    }
    // 1.2 wants `id`, and only ids issued on this connection.
    let mut c = h.client().await;
    c.send("ACK", &[("message-id", "x")], b"").await;
    let e = c.expect("ERROR").await;
    assert_eq!(e.hh("message"), "Missing header");
    assert_eq!(e.text(), "'ACK' must include the <<\"id\">> header\n");
    let mut c = h.client().await;
    c.send("ACK", &[("id", "T_s@@session-other@@0")], b"").await;
    let e = c.expect("ERROR").await;
    assert_eq!(e.hh("message"), "Invalid header");
    assert_eq!(e.text(), "'ACK' must include a valid <<\"id\">> header\n");
}

#[tokio::test]
async fn transactions_apply_to_send_and_ack() {
    let h = Harness::start().await;
    let mut c = h.client().await;
    c.sync("SUBSCRIBE", &[("destination", "/queue/tx"), ("id", "s"), ("ack", "client-individual")], b"").await;
    c.sync("BEGIN", &[("transaction", "t1")], b"").await;
    c.send("SEND", &[("destination", "/queue/tx"), ("transaction", "t1")], b"a").await;
    c.send("SEND", &[("destination", "/queue/tx"), ("transaction", "t1"), ("receipt", "in-tx")], b"b").await;
    assert!(c.try_recv(300).await.is_none(), "nothing before COMMIT");
    c.sync("COMMIT", &[("transaction", "t1")], b"").await;
    let (mut msgs, mut receipt) = (Vec::new(), false);
    while msgs.len() < 2 || !receipt {
        let f = c.recv().await;
        match f.command.as_str() {
            "MESSAGE" => msgs.push(f),
            _ => receipt |= f.h("receipt-id") == Some("in-tx"),
        }
    }
    assert_eq!(msgs.iter().map(|m| m.text()).collect::<Vec<_>>(), ["a", "b"]);

    // ABORT discards sends and acks.
    c.sync("BEGIN", &[("transaction", "t2")], b"").await;
    c.send("SEND", &[("destination", "/queue/tx"), ("transaction", "t2")], b"dropped").await;
    c.sync("ACK", &[("id", &Client::ack_id(&msgs[0])), ("transaction", "t2")], b"").await;
    c.sync("ABORT", &[("transaction", "t2")], b"").await;
    assert!(c.try_recv(300).await.is_none());
    assert_eq!(h.queue_len("tx"), Some(2));
    // Committed acks settle.
    c.sync("BEGIN", &[("transaction", "t3")], b"").await;
    for m in &msgs {
        c.send("ACK", &[("id", &Client::ack_id(m)), ("transaction", "t3")], b"").await;
    }
    c.sync("COMMIT", &[("transaction", "t3")], b"").await;
    wait_until(|| h.queue_len("tx") == Some(0)).await;

    c.send("COMMIT", &[("transaction", "nope")], b"").await;
    let e = c.expect("ERROR").await;
    assert_eq!(e.hh("message"), "Bad transaction");
    assert_eq!(e.text(), "Invalid transaction identifier: <<\"nope\">>\n");
    let mut c = h.client().await;
    c.send("BEGIN", &[], b"").await;
    let e = c.expect("ERROR").await;
    assert_eq!((e.hh("message"), e.text().as_str()), ("Missing transaction", "'BEGIN' must include a 'transaction' header\n"));
}

#[tokio::test]
async fn receipts_on_every_frame_and_disconnect() {
    let h = Harness::start().await;
    let mut c = h.client().await;
    c.sync("SUBSCRIBE", &[("destination", "/topic/rcpt"), ("id", "s")], b"").await;
    c.sync("UNSUBSCRIBE", &[("id", "s")], b"").await;
    c.sync("BEGIN", &[("transaction", "t")], b"").await;
    c.sync("ABORT", &[("transaction", "t")], b"").await;
    c.sync("SEND", &[("destination", "/topic/nobody-listens")], b"unroutable").await;
    c.send("DISCONNECT", &[("receipt", "bye")], b"").await;
    assert_eq!(c.expect("RECEIPT").await.hh("receipt-id"), "bye");
    assert!(c.closed().await);
    wait_until(|| h.broker.connections().is_empty()).await;
}

#[tokio::test]
async fn send_receipt_waits_for_durability() {
    let h = Harness::with(mq_stomp::Config::default(), true).await;
    let mut c = h.client().await;
    c.sync("SEND", &[("destination", "/queue/durable"), ("persistent", "true")], b"keep").await;
    // The receipt came back, so the message is in the queue's log.
    let q = h.vhost().queue("durable").unwrap();
    assert_eq!(q.shared.messages(), 1);
}

#[tokio::test]
async fn topics_and_subscription_queues() {
    let h = Harness::start().await;
    let mut a = h.client().await;
    let mut b = h.client().await;
    a.sync("SUBSCRIBE", &[("destination", "/topic/stocks.*"), ("id", "wild")], b"").await;
    b.sync("SUBSCRIBE", &[("destination", "/topic/stocks.acme"), ("id", "exact")], b"").await;
    let subq: Vec<_> = h.vhost().queues().into_iter().filter(|q| q.meta.name.starts_with("stomp-subscription-")).collect();
    assert_eq!(subq.len(), 2);
    assert!(subq.iter().all(|q| q.meta.exclusive() && q.meta.auto_delete && !q.meta.durable));
    a.send("SEND", &[("destination", "/topic/stocks.acme")], b"42").await;
    let (ma, mb) = (a.expect("MESSAGE").await, b.expect("MESSAGE").await);
    assert_eq!((ma.text(), mb.text()), ("42".into(), "42".into()));
    assert_eq!(ma.hh("destination"), "/topic/stocks.acme");
    assert_eq!((ma.hh("subscription"), mb.hh("subscription")), ("wild", "exact"));
    // UNSUBSCRIBE removes the auto-delete queue.
    a.sync("UNSUBSCRIBE", &[("id", "wild")], b"").await;
    wait_until(|| h.vhost().queues().iter().filter(|q| q.meta.name.starts_with("stomp-subscription-")).count() == 1).await;
}

#[tokio::test]
async fn durable_topic_subscription_survives_disconnect() {
    let h = Harness::start().await;
    let sub = [("destination", "/topic/news"), ("id", "reader-1"), ("durable", "true"), ("auto-delete", "false"), ("ack", "client")];
    let mut c = h.client().await;
    c.sync("SUBSCRIBE", &sub, b"").await;
    // Same name RabbitMQ would derive for this key and id.
    let name = "stomp-subscription-".to_string();
    let q = h.vhost().queues().into_iter().find(|q| q.meta.name.starts_with(&name)).unwrap();
    assert!(q.meta.durable && !q.meta.auto_delete && !q.meta.exclusive());
    let qname = q.meta.name.clone();
    c.send("DISCONNECT", &[], b"").await;
    assert!(c.closed().await);

    let mut p = h.client().await;
    p.sync("SEND", &[("destination", "/topic/news")], b"while away").await;
    assert_eq!(h.queue_len(&qname), Some(1));

    let mut c = h.client().await;
    c.sync("SUBSCRIBE", &sub, b"").await;
    let m = c.expect("MESSAGE").await;
    assert_eq!(m.text(), "while away");
    c.sync("ACK", &[("id", &Client::ack_id(&m))], b"").await;
    // UNSUBSCRIBE with durable:true deletes the subscription queue.
    c.sync("UNSUBSCRIBE", &[("id", "reader-1"), ("durable", "true")], b"").await;
    wait_until(|| h.vhost().queue(&qname).is_none()).await;
}

#[tokio::test]
async fn exchange_amq_queue_and_queue_arguments() {
    let h = Harness::start().await;
    let mut c = h.client().await;
    c.sync("SUBSCRIBE", &[("destination", "/exchange/amq.direct/rk"), ("id", "d")], b"").await;
    c.sync("SUBSCRIBE", &[("destination", "/exchange/amq.fanout"), ("id", "f")], b"").await;
    c.send("SEND", &[("destination", "/exchange/amq.direct/rk")], b"direct").await;
    let m = c.expect("MESSAGE").await;
    assert_eq!((m.text().as_str(), m.hh("subscription"), m.hh("destination")), ("direct", "d", "/exchange/amq.direct/rk"));
    c.send("SEND", &[("destination", "/exchange/amq.fanout")], b"fan").await;
    let m = c.expect("MESSAGE").await;
    assert_eq!((m.text().as_str(), m.hh("destination")), ("fan", "/exchange/amq.fanout"));

    // x-queue-name and x-* arguments.
    c.sync("SUBSCRIBE", &[("destination", "/topic/args"), ("id", "a"), ("x-queue-name", "named"), ("x-max-length", "5"), ("x-message-ttl", "60000")], b"").await;
    let q = h.vhost().queue("named").unwrap();
    assert_eq!(q.meta.arguments.get_i64("x-max-length"), Some(5));
    assert_eq!(q.meta.arguments.get_i64("x-message-ttl"), Some(60000));

    // /amq/queue/ needs an existing queue.
    h.vhost()
        .declare_queue(DeclareQueue { name: "existing".into(), durable: true, exclusive_owner: None, auto_delete: false, args: FieldTable::new(), passive: false })
        .await
        .unwrap();
    c.sync("SUBSCRIBE", &[("destination", "/amq/queue/existing"), ("id", "e")], b"").await;
    c.send("SEND", &[("destination", "/amq/queue/existing")], b"amq").await;
    let m = c.expect("MESSAGE").await;
    assert_eq!((m.text().as_str(), m.hh("destination")), ("amq", "/queue/existing"));
    c.send("SUBSCRIBE", &[("destination", "/amq/queue/missing"), ("id", "m")], b"").await;
    let e = c.expect("ERROR").await;
    assert_eq!((e.hh("message"), e.text().as_str()), ("not_found", "no queue 'missing' in vhost '/'"));

    let mut c = h.client().await;
    c.send("SUBSCRIBE", &[("destination", "/exchange/nope"), ("id", "x")], b"").await;
    let e = c.expect("ERROR").await;
    assert_eq!((e.hh("message"), e.text().as_str()), ("not_found", "no exchange 'nope' in vhost '/'"));
    assert!(c.closed().await);
    // The subscription queue created for it was removed again.
    wait_until(|| !h.vhost().queues().iter().any(|q| q.meta.name.starts_with("stomp-subscription-"))).await;
}

#[tokio::test]
async fn temp_queue_reply_to() {
    let h = Harness::start().await;
    let mut server = h.client().await;
    server.sync("SUBSCRIBE", &[("destination", "/queue/rpc"), ("id", "srv")], b"").await;
    let mut client = h.client().await;
    client.send("SEND", &[("destination", "/queue/rpc"), ("reply-to", "/temp-queue/answers")], b"ping").await;
    let req = server.expect("MESSAGE").await;
    let reply_to = req.hh("reply-to").to_string();
    assert!(reply_to.starts_with("/reply-queue/amq.gen-"), "{reply_to}");
    server.send("SEND", &[("destination", &reply_to)], b"pong").await;
    let rep = client.expect("MESSAGE").await;
    assert_eq!(rep.text(), "pong");
    assert_eq!(rep.hh("subscription"), "/temp-queue/answers");
    // The same temp queue is reused for later requests.
    client.send("SEND", &[("destination", "/queue/rpc"), ("reply-to", "/temp-queue/answers")], b"ping2").await;
    assert_eq!(server.expect("MESSAGE").await.hh("reply-to"), reply_to);
    // It is exclusive to the requester and goes away with it.
    let qname = reply_to.trim_start_matches("/reply-queue/").to_string();
    assert!(h.vhost().queue(&qname).unwrap().meta.exclusive());
    drop(client);
    wait_until(|| h.vhost().queue(&qname).is_none()).await;
}

#[tokio::test]
async fn frame_errors() {
    let h = Harness::start().await;
    let cases: [CommandCase; 8] = [
        ("SEND", &[], "Missing destination", "'SEND' must include a 'destination' header\n"),
        (
            "SEND",
            &[("destination", "/bogus/x")],
            "Unknown destination",
            "'/bogus/x' is not a valid destination.\nValid destination types are: /temp-queue, /exchange, /topic, /queue, /amq/queue, /reply-queue/.\n",
        ),
        ("SEND", &[("destination", "/topic/a/b")], "Invalid destination", "'/a/b' is not a valid topic destination\n"),
        ("SEND", &[("destination", "/queue/q"), ("message-id", "1")], "Invalid header", "'message-id' is not allowed on 'SEND'.\n"),
        ("SUBSCRIBE", &[("destination", "/temp-queue/x")], "Invalid destination", "'/temp-queue/x' is not a valid destination for 'SUBSCRIBE'\n"),
        ("SUBSCRIBE", &[("destination", "/topic/t"), ("durable", "true")], "Missing Header", "Header 'id' is required for durable subscriptions"),
        ("UNSUBSCRIBE", &[("id", "nope")], "No subscription found", "UNSUBSCRIBE must refer to an existing subscription.\nSubscription to <<\"id='nope'\">> not found.\n"),
        ("FROB", &[], "Bad command", "Could not interpret command \"FROB\"\n"),
    ];
    for (cmd, headers, message, body) in cases {
        let mut c = h.client().await;
        c.send(cmd, headers, b"").await;
        let e = c.expect("ERROR").await;
        assert_eq!((e.hh("message"), e.text().as_str()), (message, body), "{cmd} {headers:?}");
        assert_eq!(e.hh("version"), "1.0,1.1,1.2");
        assert!(c.closed().await, "{cmd}");
    }
    let mut c = h.client().await;
    c.send("SEND", &[("destination", "/queue/amq.reserved")], b"").await;
    let e = c.expect("ERROR").await;
    assert_eq!((e.hh("message"), e.text().as_str()), ("access_refused", "queue name 'amq.reserved' contains reserved prefix 'amq.*'"));
    let mut c = h.client().await;
    c.sync("SUBSCRIBE", &[("destination", "/queue/dup"), ("id", "s")], b"").await;
    c.send("SUBSCRIBE", &[("destination", "/queue/dup"), ("id", "s")], b"").await;
    let e = c.expect("ERROR").await;
    assert_eq!((e.hh("message"), e.text().as_str()), ("Duplicated subscription identifier", "A subscription identified by 'T_s' already exists."));
}

#[tokio::test]
async fn max_frame_size_is_enforced() {
    let h = Harness::with(mq_stomp::Config { max_frame_size: 2048, ..Default::default() }, false).await;
    let mut c = h.client().await;
    c.sync("SEND", &[("destination", "/queue/small")], &[b'x'; 1024]).await;
    c.send("SEND", &[("destination", "/queue/small")], &[b'x'; 4096]).await;
    let e = c.expect("ERROR").await;
    assert!(e.text().contains("maximum frame size of 2048"), "{}", e.text());
    assert!(c.closed().await);
    // Before login the limit is 64 KiB.
    let mut c = Client::raw_connect(h.addr).await;
    let big = "x".repeat(70_000);
    c.send("CONNECT", &[("accept-version", "1.2"), ("junk", &big)], b"").await;
    assert_eq!(c.expect("ERROR").await.hh("message"), "Invalid frame");
}

#[tokio::test]
async fn heartbeats_both_directions() {
    let h = Harness::start().await;
    // The client wants a heart-beat every second.
    let mut c = Client::raw_connect(h.addr).await;
    let f = c.login(&[("accept-version", "1.2"), ("login", "guest"), ("passcode", "guest"), ("heart-beat", "0,500")]).await;
    assert_eq!(f.hh("heart-beat"), "1000,0");
    assert!(c.try_recv(2600).await.is_none());
    assert!(c.eols >= 2, "server heart-beats: {}", c.eols);
    // The client promises one every second but goes silent: the server gives up after two.
    let mut c = Client::raw_connect(h.addr).await;
    let f = c.login(&[("accept-version", "1.2"), ("login", "guest"), ("passcode", "guest"), ("heart-beat", "1000,0")]).await;
    assert_eq!(f.hh("heart-beat"), "0,1000");
    let t = std::time::Instant::now();
    assert!(c.closed().await);
    assert!(t.elapsed() >= Duration::from_millis(1500), "{:?}", t.elapsed());
    // EOLs from the client keep it alive.
    let mut c = Client::raw_connect(h.addr).await;
    c.login(&[("accept-version", "1.2"), ("login", "guest"), ("passcode", "guest"), ("heart-beat", "1000,0")]).await;
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        c.raw(b"\n").await;
    }
    c.sync("SEND", &[("destination", "/queue/alive")], b"").await;
}

#[tokio::test]
async fn force_close_and_server_cancel() {
    let h = Harness::start().await;
    let mut c = h.client().await;
    c.sync("SUBSCRIBE", &[("destination", "/queue/doomed"), ("id", "s")], b"").await;
    h.vhost().delete_queue("doomed", false, false, None).await.unwrap();
    let e = c.expect("ERROR").await;
    assert_eq!(e.hh("message"), "Server cancelled subscription");
    assert_eq!(e.hh("subscription"), "s");
    assert_eq!(e.text(), "The server has canceled a subscription.\nNo more messages will be delivered for <<\"id='s'\">>.\n");
    // The connection stays usable.
    c.sync("SEND", &[("destination", "/queue/other")], b"").await;

    let conn = h.broker.connections().into_iter().next().unwrap();
    conn.request_close("closed by an operator");
    let e = c.expect("ERROR").await;
    assert_eq!((e.hh("message"), e.text().as_str()), ("Connection forced", "closed by an operator"));
    assert!(c.closed().await);
    wait_until(|| h.broker.connections().is_empty()).await;
}

#[tokio::test]
async fn unacked_messages_are_requeued_on_disconnect() {
    let h = Harness::start().await;
    let mut c = h.client().await;
    c.sync("SUBSCRIBE", &[("destination", "/queue/held"), ("id", "s"), ("ack", "client")], b"").await;
    c.sync("SEND", &[("destination", "/queue/held")], b"x").await;
    c.expect("MESSAGE").await;
    drop(c);
    let mut c = h.client().await;
    c.sync("SUBSCRIBE", &[("destination", "/queue/held"), ("id", "s"), ("ack", "client")], b"").await;
    let m = c.expect("MESSAGE").await;
    assert_eq!(m.hh("redelivered"), "true");
}

#[tokio::test]
async fn permissions_are_checked() {
    let h = Harness::start().await;
    h.broker.put_user(User::new("limited", "pw", vec![])).await.unwrap();
    h.broker.set_permission("/", "limited", Permission { configure: "^own\\.".into(), write: "^(own\\..*|amq\\.topic|)$".into(), read: "^own\\.".into() }).await.unwrap();
    let mut c = Client::raw_connect(h.addr).await;
    let f = c.login(&[("accept-version", "1.2"), ("login", "limited"), ("passcode", "pw")]).await;
    assert_eq!(f.command, "CONNECTED");
    c.sync("SUBSCRIBE", &[("destination", "/queue/own.q"), ("id", "s")], b"").await;
    c.send("SUBSCRIBE", &[("destination", "/queue/theirs"), ("id", "t")], b"").await;
    let e = c.expect("ERROR").await;
    assert_eq!((e.hh("message"), e.text().as_str()), ("access_refused", "configure access to queue 'theirs' in vhost '/' refused for user 'limited'"));

    let mut c = Client::raw_connect(h.addr).await;
    c.login(&[("accept-version", "1.2"), ("login", "limited"), ("passcode", "pw")]).await;
    c.send("SEND", &[("destination", "/exchange/amq.direct/k")], b"").await;
    let e = c.expect("ERROR").await;
    assert_eq!(e.text(), "write access to exchange 'amq.direct' in vhost '/' refused for user 'limited'");

    let mut c = Client::raw_connect(h.addr).await;
    c.login(&[("accept-version", "1.2"), ("login", "limited"), ("passcode", "pw")]).await;
    c.send("SEND", &[("destination", "/queue/own.q"), ("user-id", "guest")], b"").await;
    let e = c.expect("ERROR").await;
    assert_eq!(e.text(), "user_id property set to 'guest' but authenticated user was 'limited'");
}

#[tokio::test]
async fn stream_subscription_with_offsets() {
    let h = Harness::with(mq_stomp::Config::default(), true).await;
    let mut c = h.client().await;
    // Declared on demand from the x-queue-type header.
    for i in 0..5 {
        c.send("SEND", &[("destination", "/queue/events"), ("x-queue-type", "stream")], format!("e{i}").as_bytes()).await;
    }
    c.sync("SEND", &[("destination", "/queue/events"), ("x-queue-type", "stream")], b"e5").await;
    assert_eq!(h.vhost().queue("events").unwrap().kind, mq_core::queue::QueueType::Stream);
    let sub = [("destination", "/queue/events"), ("id", "s"), ("ack", "client"), ("prefetch-count", "2"), ("x-queue-type", "stream")];
    let mut c2 = h.client().await;
    c2.sync("SUBSCRIBE", &[sub.as_slice(), &[("x-stream-offset", "offset=3")]].concat(), b"").await;
    let m = c2.expect("MESSAGE").await;
    assert_eq!((m.text().as_str(), m.hh("x-stream-offset")), ("e3", "3"));
    let m2 = c2.expect("MESSAGE").await;
    assert!(c2.try_recv(300).await.is_none(), "prefetch 2");
    c2.send("ACK", &[("id", &Client::ack_id(&m2))], b"").await;
    assert_eq!(c2.expect("MESSAGE").await.text(), "e5");

    let mut c3 = h.client().await;
    c3.sync("SUBSCRIBE", &[sub.as_slice(), &[("x-stream-offset", "first")]].concat(), b"").await;
    assert_eq!(c3.expect("MESSAGE").await.text(), "e0");
    let mut c4 = h.client().await;
    c4.send("SUBSCRIBE", &[("destination", "/queue/events"), ("id", "bad"), ("x-queue-type", "stream"), ("x-stream-offset", "offset=x")], b"").await;
    let e = c4.expect("ERROR").await;
    assert_eq!((e.hh("message"), e.text().as_str()), ("precondition_failed", "invalid value for header 'x-stream-offset'"));
}

#[tokio::test]
async fn quorum_queue_from_headers_and_priorities() {
    let h = Harness::with(mq_stomp::Config::default(), true).await;
    let mut c = h.client().await;
    c.sync("SEND", &[("destination", "/queue/qq"), ("x-queue-type", "quorum")], b"x").await;
    let q = h.vhost().queue("qq").unwrap();
    assert_eq!(q.kind, mq_core::queue::QueueType::Quorum);
    // Consumer priorities: the higher one gets the messages while it has credit.
    let mut lo = h.client().await;
    let mut hi = h.client().await;
    lo.sync("SUBSCRIBE", &[("destination", "/queue/prio"), ("id", "lo"), ("x-priority", "1")], b"").await;
    hi.sync("SUBSCRIBE", &[("destination", "/queue/prio"), ("id", "hi"), ("x-priority", "10")], b"").await;
    for _ in 0..3 {
        c.send("SEND", &[("destination", "/queue/prio")], b"p").await;
    }
    for _ in 0..3 {
        hi.expect("MESSAGE").await;
    }
    assert!(lo.try_recv(200).await.is_none());
}

#[tokio::test]
async fn bare_and_reply_queue_destinations() {
    let h = Harness::start().await;
    let mut c = h.client().await;
    c.sync("SUBSCRIBE", &[("destination", "plain"), ("id", "s")], b"").await;
    c.send("SEND", &[("destination", "/reply-queue/plain")], b"via reply-queue").await;
    let m = c.expect("MESSAGE").await;
    assert_eq!((m.text().as_str(), m.hh("destination")), ("via reply-queue", "/queue/plain"));
    // No id: the subscription is named after the destination and has no subscription header.
    c.sync("SUBSCRIBE", &[("destination", "/queue/noid")], b"").await;
    c.send("SEND", &[("destination", "/queue/noid")], b"x").await;
    let m = c.expect("MESSAGE").await;
    assert!(m.h("subscription").is_none());
    assert!(m.hh("message-id").starts_with("Q_/queue/noid@@"));
    c.sync("UNSUBSCRIBE", &[("destination", "/queue/noid")], b"").await;
}
