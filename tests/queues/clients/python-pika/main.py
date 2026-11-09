"""roll queues compatibility scenarios for Python pika (BlockingConnection)."""

import hashlib
import os
import sys
import time
import traceback
import uuid

import pika
from pika.exceptions import ChannelClosedByBroker, UnroutableError

URL = os.environ.get("AMQP_URL", "amqp://guest:guest@localhost:5672/%2F")
P = f"pika-{uuid.uuid4().hex[:6]}"
SCENARIOS = []


def scenario(fn):
    SCENARIOS.append(fn)
    return fn


def connect(**kw):
    params = pika.URLParameters(URL)
    for k, v in kw.items():
        setattr(params, k, v)
    return pika.BlockingConnection(params)


def check(cond, msg):
    if not cond:
        raise AssertionError(msg)


def count(ch, q):
    return ch.queue_declare(q, passive=True).method.message_count


def get_wait(ch, q, timeout=5.0):
    end = time.time() + timeout
    while time.time() < end:
        m, p, b = ch.basic_get(q)
        if m:
            return m, p, b
        time.sleep(0.05)
    raise AssertionError(f"no message on {q} within {timeout}s")


def pump(c, seconds):
    # process_data_events returns at the first event, and deliveries can arrive in several reads.
    end = time.time() + seconds
    while (left := end - time.time()) > 0:
        c.process_data_events(time_limit=left)


def expect_close(fn, code):
    try:
        fn()
    except ChannelClosedByBroker as e:
        check(e.reply_code == code, f"expected {code}, got {e.reply_code} {e.reply_text}")
        return e.reply_text
    raise AssertionError(f"expected channel error {code}")


@scenario
def connect_():
    c = connect()
    props = c._impl.server_properties
    check(c.is_open, "connection not open")
    ch = c.channel()
    check(ch.is_open, "channel not open")
    c.close()
    return f"{props['product'].decode() if isinstance(props['product'], bytes) else props['product']} {props['version']}"


@scenario
def declare():
    c = connect()
    ch = c.channel()
    x, q = f"{P}.declare.x", f"{P}.declare.q"
    ch.exchange_declare(x, "direct", durable=True)
    ok = ch.queue_declare(q, durable=True)
    check(ok.method.queue == q and ok.method.message_count == 0, f"declare-ok {ok.method}")
    ch.queue_bind(q, x, "k")
    ch.exchange_declare(x, "direct", passive=True)
    srv = ch.queue_declare("", exclusive=True).method.queue
    check(srv.startswith("amq.gen-"), f"server-named queue {srv}")
    text = expect_close(lambda: ch.queue_declare(q, durable=False), 406)
    check("inequivalent arg 'durable'" in text, text)
    ch = c.channel()
    expect_close(lambda: ch.queue_declare(f"{P}.missing", passive=True), 404)
    ch = c.channel()
    ch.queue_unbind(q, x, "k")
    ch.queue_delete(q)
    ch.exchange_delete(x)
    c.close()


@scenario
def publish_confirms():
    c = connect()
    ch = c.channel()
    q = f"{P}.confirms"
    ch.queue_declare(q, durable=True)
    ch.confirm_delivery()
    for i in range(100):
        ch.basic_publish("", q, f"m{i}".encode(), pika.BasicProperties(delivery_mode=2), mandatory=True)
    check(count(ch, q) == 100, f"count {count(ch, q)}")
    try:
        ch.basic_publish("", f"{P}.nowhere", b"x", mandatory=True)
        raise AssertionError("unroutable mandatory publish was not returned")
    except UnroutableError:
        pass
    ch.queue_delete(q)
    c.close()
    return "100 acked, mandatory return ok"


@scenario
def consume_ack():
    c = connect()
    ch = c.channel()
    q = f"{P}.consume"
    ch.queue_declare(q)
    ch.confirm_delivery()
    for i in range(10):
        ch.basic_publish("", q, f"m{i}".encode())
    got = []
    for m, _, body in ch.consume(q, inactivity_timeout=5):
        check(m is not None, f"timed out after {len(got)}")
        got.append(body.decode())
        ch.basic_ack(m.delivery_tag)
        if len(got) == 10:
            break
    ch.cancel()
    check(got == [f"m{i}" for i in range(10)], f"order {got}")
    check(count(ch, q) == 0, "queue not empty after acks")
    ch.queue_delete(q)
    c.close()


@scenario
def nack_requeue():
    c = connect()
    ch = c.channel()
    q = f"{P}.nack"
    ch.queue_declare(q)
    ch.basic_publish("", q, b"again")
    n = 0
    for m, _, body in ch.consume(q, inactivity_timeout=5):
        check(m is not None, "timed out")
        n += 1
        if n == 1:
            check(not m.redelivered, "first delivery flagged redelivered")
            ch.basic_nack(m.delivery_tag, requeue=True)
        else:
            check(m.redelivered and body == b"again", "requeued delivery not redelivered")
            ch.basic_ack(m.delivery_tag)
            break
    ch.cancel()
    ch.queue_delete(q)
    c.close()


@scenario
def reject_dlx():
    c = connect()
    ch = c.channel()
    dlx, dlq, q = f"{P}.dlx", f"{P}.dlq", f"{P}.work"
    ch.exchange_declare(dlx, "fanout")
    ch.queue_declare(dlq)
    ch.queue_bind(dlq, dlx)
    ch.queue_declare(q, arguments={"x-dead-letter-exchange": dlx})
    ch.basic_publish("", q, b"bad")
    m, _, _ = get_wait(ch, q)
    ch.basic_reject(m.delivery_tag, requeue=False)
    _, props, body = get_wait(ch, dlq)
    death = props.headers["x-death"][0]
    check(body == b"bad", "body")
    check(death["reason"] == "rejected" and death["queue"] == q and death["count"] == 1, f"x-death {death}")
    for n in (q, dlq):
        ch.queue_delete(n)
    ch.exchange_delete(dlx)
    c.close()


@scenario
def topic():
    c = connect()
    ch = c.channel()
    x, q1, q2 = f"{P}.topic", f"{P}.t1", f"{P}.t2"
    ch.exchange_declare(x, "topic")
    for q, k in ((q1, "a.*"), (q2, "#.z")):
        ch.queue_declare(q)
        ch.queue_bind(q, x, k)
    ch.confirm_delivery()
    for k in ("a.b", "a.b.z", "x.z", "a.z", "b.c"):
        try:
            ch.basic_publish(x, k, k.encode(), mandatory=True)
        except UnroutableError:
            check(k == "b.c", f"{k} unroutable")
    check((count(ch, q1), count(ch, q2)) == (2, 3), f"counts {count(ch, q1)},{count(ch, q2)}")
    for q in (q1, q2):
        ch.queue_delete(q)
    ch.exchange_delete(x)
    c.close()


@scenario
def headers():
    c = connect()
    ch = c.channel()
    x, qa, qb = f"{P}.headers", f"{P}.hall", f"{P}.hany"
    ch.exchange_declare(x, "headers")
    ch.queue_declare(qa)
    ch.queue_declare(qb)
    ch.queue_bind(qa, x, arguments={"x-match": "all", "format": "pdf", "type": "report"})
    ch.queue_bind(qb, x, arguments={"x-match": "any", "format": "zip", "type": "log"})
    ch.confirm_delivery()
    for h in ({"format": "pdf", "type": "report"}, {"format": "pdf"}, {"type": "log"}, {"format": "zip", "type": "x"}):
        try:
            ch.basic_publish(x, "", b"h", pika.BasicProperties(headers=h), mandatory=True)
        except UnroutableError:
            check(h == {"format": "pdf"}, f"{h} unroutable")
    check((count(ch, qa), count(ch, qb)) == (1, 2), f"counts {count(ch, qa)},{count(ch, qb)}")
    for q in (qa, qb):
        ch.queue_delete(q)
    ch.exchange_delete(x)
    c.close()


@scenario
def tx():
    c = connect()
    ch = c.channel()
    q = f"{P}.tx"
    ch.queue_declare(q)
    ch.tx_select()
    ch.basic_publish("", q, b"rolled back")
    ch.tx_rollback()
    check(count(ch, q) == 0, "rollback left a message")
    ch.basic_publish("", q, b"committed")
    ch.basic_publish("", q, b"committed")
    ch.tx_commit()
    check(count(ch, q) == 2, f"commit count {count(ch, q)}")
    m, _, _ = ch.basic_get(q)
    ch.basic_ack(m.delivery_tag)
    ch.tx_rollback()
    ch.close()  # the rolled-back ack leaves the message unacked, so closing requeues it
    ch = c.channel()
    check(count(ch, q) == 2, "ack was not rolled back")
    ch.tx_select()
    m, _, _ = ch.basic_get(q)
    ch.basic_ack(m.delivery_tag)
    ch.tx_commit()
    ch.close()
    ch = c.channel()
    check(count(ch, q) == 1, "ack was not committed")
    ch.queue_delete(q)
    c.close()


@scenario
def prefetch():
    c = connect()
    ch = c.channel()
    q = f"{P}.prefetch"
    ch.queue_declare(q)
    ch.confirm_delivery()
    for i in range(20):
        ch.basic_publish("", q, str(i).encode())
    ch.basic_qos(prefetch_count=5)
    tags = []
    ch.basic_consume(q, lambda _ch, m, _p, _b: tags.append(m.delivery_tag))
    pump(c, 1)
    check(len(tags) == 5, f"received {len(tags)} with prefetch 5")
    ch.basic_ack(tags[-1], multiple=True)
    pump(c, 1)
    check(len(tags) == 10, f"received {len(tags)} after acking 5")
    ch.close()
    ch = c.channel()
    ch.queue_delete(q)
    c.close()


@scenario
def ttl():
    c = connect()
    ch = c.channel()
    q1, q2 = f"{P}.ttlq", f"{P}.ttlm"
    ch.queue_declare(q1, arguments={"x-message-ttl": 200})
    ch.queue_declare(q2)
    ch.confirm_delivery()
    ch.basic_publish("", q1, b"x")
    ch.basic_publish("", q2, b"short", pika.BasicProperties(expiration="200"))
    ch.basic_publish("", q2, b"long", pika.BasicProperties(expiration="60000"))
    time.sleep(0.7)
    check(ch.basic_get(q1, auto_ack=True)[0] is None, "queue TTL did not expire")
    m, _, body = ch.basic_get(q2, auto_ack=True)
    check(m is not None and body == b"long", f"per-message TTL: got {body}")
    for q in (q1, q2):
        ch.queue_delete(q)
    c.close()


@scenario
def priority():
    c = connect()
    ch = c.channel()
    q = f"{P}.prio"
    ch.queue_declare(q, arguments={"x-max-priority": 10})
    ch.confirm_delivery()
    for p in (1, 9, 5, 0, 7):
        ch.basic_publish("", q, str(p).encode(), pika.BasicProperties(priority=p))
    got = [int(ch.basic_get(q, auto_ack=True)[2]) for _ in range(5)]
    check(got == [9, 7, 5, 1, 0], f"order {got}")
    ch.queue_delete(q)
    c.close()


@scenario
def exclusive_autodelete():
    a, b = connect(), connect()
    cha = a.channel()
    q = cha.queue_declare("", exclusive=True).method.queue
    chb = b.channel()
    expect_close(lambda: chb.queue_declare(q, passive=True), 405)
    a.close()
    chb = b.channel()
    expect_close(lambda: chb.queue_declare(q, passive=True), 404)
    chb = b.channel()
    ad = f"{P}.autodel"
    chb.queue_declare(ad, auto_delete=True)
    tag = chb.basic_consume(ad, lambda *_: None)
    chb.basic_cancel(tag)
    expect_close(lambda: chb.queue_declare(ad, passive=True), 404)
    b.close()


@scenario
def direct_reply_to():
    c = connect()
    client, server = c.channel(), c.channel()
    q = f"{P}.rpc"
    server.queue_declare(q)

    def serve(_ch, m, props, body):
        server.basic_publish("", props.reply_to, body.upper(), pika.BasicProperties(correlation_id=props.correlation_id))
        server.basic_ack(m.delivery_tag)

    server.basic_consume(q, serve)
    replies = []
    client.basic_consume("amq.rabbitmq.reply-to", lambda _c, _m, p, b: replies.append((p.correlation_id, b)), auto_ack=True)
    client.basic_publish("", q, b"ping", pika.BasicProperties(reply_to="amq.rabbitmq.reply-to", correlation_id="c1"))
    end = time.time() + 5
    while not replies and time.time() < end:
        c.process_data_events(time_limit=0.1)
    check(replies == [("c1", b"PING")], f"replies {replies}")
    server.queue_delete(q)
    c.close()


@scenario
def basic_get():
    c = connect()
    ch = c.channel()
    q = f"{P}.get"
    ch.queue_declare(q)
    ch.confirm_delivery()
    for i in range(3):
        ch.basic_publish("", q, str(i).encode())
    m, _, body = ch.basic_get(q)
    check(m.message_count == 2 and body == b"0", f"get-ok message_count {m.message_count}")
    ch.basic_ack(m.delivery_tag)
    ch.basic_get(q, auto_ack=True)
    ch.basic_get(q, auto_ack=True)
    check(ch.basic_get(q)[0] is None, "expected get-empty")
    ch.queue_delete(q)
    c.close()


@scenario
def cancel_notify():
    c = connect()
    ch, other = c.channel(), c.channel()
    q = f"{P}.cancel"
    ch.queue_declare(q)
    cancelled = []
    ch.add_on_cancel_callback(lambda frame: cancelled.append(frame.method.consumer_tag))
    tag = ch.basic_consume(q, lambda *_: None)
    other.queue_delete(q)
    end = time.time() + 5
    while not cancelled and time.time() < end:
        c.process_data_events(time_limit=0.1)
    check(cancelled == [tag], f"cancel notifications {cancelled}")
    c.close()


@scenario
def connection_blocked():
    c = connect()
    caps = c._impl.server_properties["capabilities"]
    check(caps.get("connection.blocked") is True, f"capabilities {caps}")
    c.add_on_connection_blocked_callback(lambda *_: None)
    c.close()


@scenario
def heartbeat():
    c = connect(heartbeat=2)
    check(c._impl.params.heartbeat == 2, f"negotiated heartbeat {c._impl.params.heartbeat}")
    c.sleep(7)
    check(c.is_open, "connection closed while idle")
    ch = c.channel()
    ch.queue_declare("", exclusive=True)
    c.close()
    return "2s heartbeat, idle 7s"


@scenario
def large_message():
    c = connect()
    ch = c.channel()
    q = f"{P}.large"
    ch.queue_declare(q)
    ch.confirm_delivery()
    body = os.urandom(5 * 1024 * 1024)
    ch.basic_publish("", q, body)
    _, _, got = ch.basic_get(q, auto_ack=True)
    check(hashlib.sha256(got).digest() == hashlib.sha256(body).digest(), "body mismatch")
    ch.queue_delete(q)
    c.close()
    return "5 MiB"


@scenario
def throughput():
    n = 10_000
    c = connect()
    ch = c.channel()
    q = f"{P}.tput"
    ch.queue_declare(q)
    t0 = time.time()
    for i in range(n):
        ch.basic_publish("", q, b"x" * 100)
    pub = time.time() - t0
    ch.basic_qos(prefetch_count=500)
    got = 0
    t1 = time.time()
    for m, _, _ in ch.consume(q, inactivity_timeout=10):
        check(m is not None, f"timed out after {got}")
        got += 1
        if got % 100 == 0 or got == n:
            ch.basic_ack(m.delivery_tag, multiple=True)
        if got == n:
            break
    ch.cancel()
    con = time.time() - t1
    ch.queue_delete(q)
    c.close()
    return f"publish {n / pub:.0f} msg/s, consume {n / con:.0f} msg/s"


def main():
    only = set(sys.argv[1:])
    failed = 0
    for fn in SCENARIOS:
        name = fn.__name__.rstrip("_")
        if only and name not in only:
            continue
        t = time.time()
        try:
            detail = fn() or ""
            print(f"PASS {name} {detail} ({time.time() - t:.1f}s)".replace("  ", " "), flush=True)
        except Exception as e:  # noqa: BLE001
            failed += 1
            print(f"FAIL {name}: {type(e).__name__}: {e}", flush=True)
            traceback.print_exc(file=sys.stderr)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
