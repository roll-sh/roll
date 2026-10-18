"""roll queues compatibility scenarios for Python stomp.py (STOMP 1.0, 1.1 and 1.2)."""

import os
import queue
import sys
import time
import traceback
import uuid

import pika
import stomp

HOST = os.environ.get("AMQP_HOST", "localhost")
PORT = int(os.environ.get("STOMP_PORT", "61613"))
AMQP_URL = os.environ.get("AMQP_URL", "amqp://guest:guest@localhost:5672/%2F")
P = f"stomp-{uuid.uuid4().hex[:6]}"
SCENARIOS = []


def scenario(fn):
    SCENARIOS.append(fn)
    return fn


def check(cond, msg):
    if not cond:
        raise AssertionError(msg)


class Listener(stomp.ConnectionListener):
    def __init__(self):
        self.messages = queue.Queue()
        self.receipts = queue.Queue()
        self.errors = queue.Queue()
        self.connected = queue.Queue()
        self.disconnected = False

    def on_message(self, frame):
        self.messages.put(frame)

    def on_receipt(self, frame):
        self.receipts.put(frame.headers.get("receipt-id"))

    def on_error(self, frame):
        self.errors.put(frame)

    def on_connected(self, frame):
        self.connected.put(frame)

    def on_disconnected(self):
        self.disconnected = True


class Client:
    def __init__(self, version="1.2", heartbeats=(0, 0), login="guest", passcode="guest", wait=True, decode=True):
        cls = {"1.0": stomp.Connection10, "1.1": stomp.Connection11, "1.2": stomp.Connection12}[version]
        # STOMP's host header names the vhost; stomp.py would send the DNS name.
        kw = {"auto_decode": decode} if version == "1.0" else {"heartbeats": heartbeats, "vhost": "/", "auto_decode": decode}
        self.c = cls([(HOST, PORT)], **kw)
        self.version = version
        self.l = Listener()
        self.c.set_listener("", self.l)
        self.c.connect(login, passcode, wait=wait)
        self.subs = 0

    def subscribe(self, dest, ack="auto", headers=None):
        self.subs += 1
        sid = f"s{self.subs}"
        receipt = f"sub-{uuid.uuid4().hex[:6]}"
        if self.version == "1.0":
            self.c.subscribe(dest, id=sid, ack=ack, headers=headers or {}, receipt=receipt)
        else:
            self.c.subscribe(dest, sid, ack=ack, headers=headers or {}, receipt=receipt)
        self.receipt(receipt)
        return sid

    def send(self, dest, body, receipt=True, **headers):
        r = f"send-{uuid.uuid4().hex[:6]}" if receipt else None
        if r:
            headers["receipt"] = r
        self.c.send(dest, body, **headers)
        if r:
            self.receipt(r)

    def receipt(self, rid, timeout=10):
        end = time.time() + timeout
        while time.time() < end:
            try:
                got = self.l.receipts.get(timeout=max(0.01, end - time.time()))
            except queue.Empty:
                break
            if got == rid:
                return
        err = None if self.l.errors.empty() else self.l.errors.get()
        raise AssertionError(f"no receipt {rid}" + (f"; ERROR {err.headers.get('message')}: {err.body}" if err else ""))

    def recv(self, timeout=5):
        try:
            return self.l.messages.get(timeout=timeout)
        except queue.Empty:
            raise AssertionError(f"no message within {timeout}s") from None

    def quiet(self, timeout=0.5):
        try:
            m = self.l.messages.get(timeout=timeout)
        except queue.Empty:
            return
        raise AssertionError(f"unexpected message {m.headers.get('destination')}: {m.body!r}")

    def ack(self, frame):
        h = frame.headers
        if self.version == "1.2":
            self.c.ack(h["ack"])
        else:
            self.c.ack(h["message-id"], h["subscription"])

    def nack(self, frame, **headers):
        h = frame.headers
        if self.version == "1.2":
            self.c.nack(h["ack"], **headers)
        else:
            self.c.nack(h["message-id"], h["subscription"], **headers)

    def close(self):
        if self.c.is_connected():
            self.c.disconnect()


def body(frame):
    b = frame.body
    return b if isinstance(b, str) else b.decode()


@scenario
def connect():
    out = []
    for v in ("1.0", "1.1", "1.2"):
        c = Client(v)
        frame = c.l.connected.get(timeout=5)
        server = frame.headers.get("server", "")
        check(v == "1.0" or frame.headers.get("version") == v, f"{v}: CONNECTED version {frame.headers.get('version')}")
        out.append(server)
        c.close()
    return out[-1]


@scenario
def bad_login():
    c = stomp.Connection12([(HOST, PORT)], vhost="/")
    l = Listener()
    c.set_listener("", l)
    try:
        c.connect("guest", "wrong", wait=True)
    except Exception:  # noqa: BLE001 - stomp.py raises once the server hangs up
        pass
    err = l.errors.get(timeout=5)
    check(err.headers.get("message") == "Bad CONNECT" and "Access refused for user 'guest'" in body(err), f"ERROR {err.headers} {err.body}")


@scenario
def queue_send_receive():
    c = Client()
    q = f"/queue/{P}.q"
    c.subscribe(q)
    c.send(q, "hello", content_type="text/plain", custom="v1")
    m = c.recv()
    check(body(m) == "hello" and m.headers.get("custom") == "v1", f"got {m.headers} {m.body!r}")
    check(m.headers.get("destination") == q, f"destination {m.headers.get('destination')}")
    c.close()


@scenario
def topic_fanout():
    a, b = Client(), Client()
    t = f"/topic/{P}.t"
    a.subscribe(t)
    b.subscribe(t)
    a.send(t, "all")
    check(body(a.recv()) == "all" and body(b.recv()) == "all", "both subscribers get it")
    a.close()
    b.close()


@scenario
def exchange_destination():
    c = Client()
    key = f"{P}.k"
    c.subscribe(f"/exchange/amq.direct/{key}")
    c.send(f"/exchange/amq.direct/{key}", "direct")
    check(body(c.recv()) == "direct", "via /exchange/amq.direct")
    c.close()


@scenario
def amq_queue_destination():
    conn = pika.BlockingConnection(pika.URLParameters(AMQP_URL))
    ch = conn.channel()
    q = f"{P}.existing"
    ch.queue_declare(q)
    c = Client()
    c.subscribe(f"/amq/queue/{q}")
    c.send(f"/amq/queue/{q}", "existing")
    check(body(c.recv()) == "existing", "via /amq/queue")
    c.close()
    ch.queue_delete(q)
    conn.close()


@scenario
def client_ack():
    c = Client()
    q = f"/queue/{P}.ack"
    for i in range(3):
        c.send(q, str(i))
    c.subscribe(q, ack="client-individual", headers={"prefetch-count": "10"})
    frames = [c.recv() for _ in range(3)]
    check([body(f) for f in frames] == ["0", "1", "2"], f"order {[body(f) for f in frames]}")
    c.ack(frames[1])  # only the middle one
    c.close()
    c = Client()
    c.subscribe(q, ack="client")
    rest = sorted(body(c.recv()) for _ in range(2))
    check(rest == ["0", "2"], f"unacked redelivered: {rest}")
    c.quiet()
    c.close()


@scenario
def nack_requeue():
    c = Client()
    q = f"/queue/{P}.nack"
    c.subscribe(q, ack="client-individual")
    c.send(q, "again")
    f = c.recv()
    c.nack(f)
    f2 = c.recv()
    check(body(f2) == "again" and f2.headers.get("redelivered") == "true", f"redelivery {f2.headers}")
    c.ack(f2)
    c.close()


@scenario
def nack_dead_letter():
    conn = pika.BlockingConnection(pika.URLParameters(AMQP_URL))
    ch = conn.channel()
    q, dlq = f"{P}.dlx", f"{P}.dlq"
    ch.queue_declare(dlq)
    ch.queue_declare(q, arguments={"x-dead-letter-exchange": "", "x-dead-letter-routing-key": dlq})
    c = Client()
    c.subscribe(f"/amq/queue/{q}", ack="client-individual")
    c.send(f"/amq/queue/{q}", "dead")
    c.nack(c.recv(), requeue="false")
    deadline = time.time() + 5
    m = None
    while not m and time.time() < deadline:
        m, _, b = ch.basic_get(dlq, auto_ack=True)
        time.sleep(0.05)
    check(m and b == b"dead", "dead-lettered after NACK requeue:false")
    c.close()
    ch.queue_delete(q)
    ch.queue_delete(dlq)
    conn.close()


@scenario
def transactions():
    c = Client()
    q = f"/queue/{P}.tx"
    c.subscribe(q)
    tx = c.c.begin()
    c.c.send(q, "aborted", transaction=tx)
    c.c.abort(tx)
    tx = c.c.begin()
    c.c.send(q, "committed", transaction=tx)
    c.quiet()
    c.c.commit(tx)
    check(body(c.recv()) == "committed", "committed message")
    c.quiet()
    c.close()


@scenario
def durable_topic_subscription():
    t = f"/topic/{P}.durable"
    hdr = {"durable": "true", "auto-delete": "false", "id": "dsub"}
    c = Client()
    c.c.set_listener("", c.l)
    c.c.subscribe(t, "dsub", ack="auto", headers=hdr, receipt="ds1")
    c.receipt("ds1")
    c.close()
    pub = Client()
    pub.send(t, "while away", persistent="true")
    pub.close()
    c = Client()
    c.c.subscribe(t, "dsub", ack="auto", headers=hdr, receipt="ds2")
    c.receipt("ds2")
    check(body(c.recv()) == "while away", "durable subscription kept the message")
    c.c.unsubscribe("dsub", headers={"durable": "true"})
    c.close()


@scenario
def temp_queue_reply():
    server, client = Client(), Client()
    q = f"/queue/{P}.rpc"
    server.subscribe(q)
    client.send(q, "ping", **{"reply-to": "/temp-queue/r1"})
    req = server.recv()
    reply_to = req.headers.get("reply-to")
    check(reply_to and reply_to.startswith("/reply-queue/"), f"reply-to rewritten to {reply_to}")
    server.send(reply_to, "pong")
    r = client.recv()
    check(body(r) == "pong", f"reply {r.body!r}")
    server.close()
    client.close()


@scenario
def heartbeats():
    c = Client(heartbeats=(1000, 1000))
    time.sleep(4)
    check(c.c.is_connected(), "dropped while heart-beating")
    q = f"/queue/{P}.hb"
    c.subscribe(q)
    c.send(q, "alive")
    check(body(c.recv()) == "alive", "works after idling")
    c.close()


@scenario
def quorum_queue_header():
    c = Client()
    q = f"/queue/{P}.quorum"
    c.subscribe(q, ack="client-individual", headers={"x-queue-type": "quorum", "durable": "true", "auto-delete": "false"})
    c.send(q, "q", **{"x-queue-type": "quorum"})
    f = c.recv()
    c.ack(f)
    c.close()
    conn = pika.BlockingConnection(pika.URLParameters(AMQP_URL))
    ch = conn.channel()
    ok = ch.queue_declare(f"{P}.quorum", durable=True, arguments={"x-queue-type": "quorum"}, passive=False)
    check(ok.method.queue == f"{P}.quorum", "declared as quorum")
    ch.queue_delete(f"{P}.quorum")
    conn.close()


@scenario
def amqp_interop():
    conn = pika.BlockingConnection(pika.URLParameters(AMQP_URL))
    ch = conn.channel()
    q = f"{P}.interop"
    ch.queue_declare(q, durable=True)
    c = Client()
    c.send(f"/amq/queue/{q}", "from stomp", custom="h")
    m, props, b = None, None, None
    deadline = time.time() + 5
    while not m and time.time() < deadline:
        m, props, b = ch.basic_get(q, auto_ack=True)
        time.sleep(0.05)
    check(m and b == b"from stomp" and (props.headers or {}).get("custom") == "h", f"AMQP got {b!r} {props and props.headers}")
    c.subscribe(f"/topic/{P}.fromamqp")
    ch.basic_publish("amq.topic", f"{P}.fromamqp", b"from amqp", pika.BasicProperties(headers={"k": "v"}, content_type="text/plain"))
    f = c.recv()
    check(body(f) == "from amqp" and f.headers.get("k") == "v" and f.headers.get("content-type") == "text/plain", f"STOMP got {f.headers} {f.body!r}")
    c.close()
    ch.queue_delete(q)
    conn.close()


@scenario
def large_binary():
    c = Client(decode=False)
    q = f"/queue/{P}.big"
    c.subscribe(q)
    data = bytes(range(256)) * 4096  # 1 MiB with NULs
    c.send(q, data, content_type="application/octet-stream")
    m = c.recv(15)
    check(m.body == data, f"{len(m.body)} bytes back")
    c.close()
    return "1 MiB"


@scenario
def throughput():
    n = 5000
    c = Client()
    q = f"/queue/{P}.tp"
    c.subscribe(q)
    t0 = time.time()
    for _ in range(n):
        c.c.send(q, "x" * 64)
    pub = time.time() - t0
    for _ in range(n):
        c.recv(30)
    total = time.time() - t0
    c.close()
    return f"publish {n / pub:.0f} msg/s, end to end {n / total:.0f} msg/s"


def main():
    only = set(sys.argv[1:])
    failed = 0
    for fn in SCENARIOS:
        name = fn.__name__
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
