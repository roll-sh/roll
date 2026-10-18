"""roll queues compatibility scenarios for Python paho-mqtt (MQTT 3.1.1 and 5.0)."""

import os
import queue
import socket
import sys
import time
import traceback
import uuid

import paho.mqtt.client as mqtt
import pika
from paho.mqtt.packettypes import PacketTypes
from paho.mqtt.properties import Properties
from paho.mqtt.subscribeoptions import SubscribeOptions

HOST = os.environ.get("AMQP_HOST", "localhost")
PORT = int(os.environ.get("MQTT_PORT", "1883"))
AMQP_URL = os.environ.get("AMQP_URL", "amqp://guest:guest@localhost:5672/%2F")
P = f"paho{uuid.uuid4().hex[:6]}"
V3, V5 = mqtt.MQTTv311, mqtt.MQTTv5
SCENARIOS = []


def scenario(fn):
    SCENARIOS.append(fn)
    return fn


def check(cond, msg):
    if not cond:
        raise AssertionError(msg)


class Client:
    """A paho client driven synchronously: every callback lands on a queue."""

    def __init__(self, proto=V3, cid=None, clean=True, will=None, password="guest", session_expiry=None, connect=True):
        self.proto = proto
        self.cid = cid or f"{P}-{uuid.uuid4().hex[:6]}"
        kw = {"client_id": self.cid, "protocol": proto}
        if proto != V5:
            kw["clean_session"] = clean
        self.c = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2, **kw)
        self.c.username_pw_set("guest", password)
        self.clean, self.session_expiry = clean, session_expiry
        self.events = queue.Queue()
        self.messages = queue.Queue()
        self.acks = {}
        self.c.on_connect = lambda c, u, flags, rc, props: self.events.put(("connack", flags, rc, props))
        self.c.on_subscribe = lambda c, u, mid, rcs, props: self.events.put(("suback", mid, rcs, props))
        self.c.on_unsubscribe = lambda c, u, mid, rcs, props: self.events.put(("unsuback", mid, rcs))
        self.c.on_message = lambda c, u, m: self.messages.put(m)
        self.c.on_publish = lambda c, u, mid, rc, props: self.acks.__setitem__(mid, rc)
        self.c.on_disconnect = lambda c, u, flags, rc, props: self.events.put(("disconnect", rc))
        if will:
            topic, payload = will
            self.c.will_set(topic, payload, qos=1)
        if connect:
            self.connack = self.connect()

    def connect(self):
        props = None
        kw = {}
        if self.proto == V5:
            props = Properties(PacketTypes.CONNECT)
            if self.session_expiry is not None:
                props.SessionExpiryInterval = self.session_expiry
            kw["clean_start"] = self.clean
        self.c.connect(HOST, PORT, keepalive=30, properties=props, **kw)
        self.c.loop_start()
        ev = self.wait("connack")
        return ev

    def wait(self, kind, timeout=10):
        end = time.time() + timeout
        while time.time() < end:
            try:
                ev = self.events.get(timeout=max(0.01, end - time.time()))
            except queue.Empty:
                break
            if ev[0] == kind:
                return ev
        raise AssertionError(f"no {kind} within {timeout}s")

    def subscribe(self, topic, qos=1, **opts):
        if self.proto == V5:
            props = None
            if "sub_id" in opts:
                props = Properties(PacketTypes.SUBSCRIBE)
                props.SubscriptionIdentifier = opts.pop("sub_id")
            _, mid = self.c.subscribe(topic, options=SubscribeOptions(qos=qos, **opts), properties=props)
        else:
            _, mid = self.c.subscribe(topic, qos=qos)
        ev = self.wait("suback")
        check(ev[1] == mid, f"suback for mid {ev[1]}, expected {mid}")
        return ev[2]

    def publish(self, topic, payload, qos=1, retain=False, props=None):
        info = self.c.publish(topic, payload, qos=qos, retain=retain, properties=props)
        info.wait_for_publish(10)
        check(info.is_published(), f"publish to {topic} not acknowledged")
        return self.acks.get(info.mid)

    def recv(self, timeout=5):
        try:
            return self.messages.get(timeout=timeout)
        except queue.Empty:
            raise AssertionError(f"no message within {timeout}s") from None

    def quiet(self, timeout=0.5):
        try:
            m = self.messages.get(timeout=timeout)
        except queue.Empty:
            return
        raise AssertionError(f"unexpected message on {m.topic}: {m.payload!r}")

    def close(self):
        self.c.disconnect()
        self.c.loop_stop()


def rc_value(rc):
    return rc.value if hasattr(rc, "value") else rc


@scenario
def connect_v311():
    c = Client(V3)
    flags, rc = c.connack[1], c.connack[2]
    check(not rc.is_failure and not flags.session_present, f"connack {rc} session_present={flags.session_present}")
    c.close()


@scenario
def connect_v5():
    c = Client(V5)
    _, flags, rc, props = c.connack
    check(not rc.is_failure, f"connack {rc}")
    # The server tells a v5 client its limits; RabbitMQ sends these.
    names = {k for k in ("TopicAliasMaximum", "MaximumQoS", "RetainAvailable") if hasattr(props, k)}
    check("TopicAliasMaximum" in names, f"connack properties {props}")
    c.close()
    return ", ".join(sorted(names))


@scenario
def bad_password():
    # paho reports MQTT 3.1.1's return code 4 as reason code 0x86 too.
    for proto in (V3, V5):
        c = Client(proto, password="wrong", connect=False)
        c.c.connect(HOST, PORT, keepalive=30)
        c.c.loop_start()
        rc = c.wait("connack")[2]
        c.c.loop_stop()
        check(rc_value(rc) == 0x86, f"MQTT {'5' if proto == V5 else '3.1.1'}: connack {rc}")


@scenario
def qos0_qos1():
    c = Client(V3)
    granted = c.subscribe(f"{P}/q/#", qos=1)
    check([rc_value(g) for g in granted] == [1], f"granted {granted}")
    c.publish(f"{P}/q/0", b"zero", qos=0)
    c.publish(f"{P}/q/1", b"one", qos=1)
    got = sorted((m.topic, m.payload, m.qos) for m in (c.recv(), c.recv()))
    check(got == [(f"{P}/q/0", b"zero", 0), (f"{P}/q/1", b"one", 1)], f"got {got}")
    c.close()


@scenario
def qos2_downgraded():
    # RabbitMQ grants at most QoS 1.
    c = Client(V3)
    granted = c.subscribe(f"{P}/q2", qos=2)
    check([rc_value(g) for g in granted] == [1], f"granted {granted}")
    c.close()


@scenario
def wildcards():
    c = Client(V3)
    c.subscribe(f"{P}/e/+/c")
    c.subscribe(f"{P}/e/x/#")
    for t in ("a/c", "b/c", "a/b/c", "x", "x/y/z", "y"):
        c.publish(f"{P}/e/{t}", t.encode())
    got = sorted(c.recv().topic for _ in range(4))
    c.quiet()
    want = sorted([f"{P}/e/a/c", f"{P}/e/b/c", f"{P}/e/x", f"{P}/e/x/y/z"])
    check(got == want, f"got {got}")
    c.close()


@scenario
def retained():
    pub = Client(V3)
    t = f"{P}/r"
    pub.publish(t, b"kept", retain=True)
    sub = Client(V3)
    sub.subscribe(t)
    m = sub.recv()
    check(m.payload == b"kept" and m.retain, f"retained {m.payload!r} retain={m.retain}")
    pub.publish(t, b"", retain=True)  # clears it
    sub.close()
    time.sleep(0.2)
    sub = Client(V3)
    sub.subscribe(t)
    sub.quiet()
    sub.close()
    pub.close()


@scenario
def persistent_session():
    cid = f"{P}-persist"
    c = Client(V3, cid=cid, clean=False)
    c.subscribe(f"{P}/p", qos=1)
    c.close()
    pub = Client(V3)
    for i in range(3):
        pub.publish(f"{P}/p", str(i).encode())
    pub.close()
    c = Client(V3, cid=cid, clean=False)
    check(c.connack[1].session_present, "session not present after reconnect")
    got = sorted(c.recv().payload for _ in range(3))
    check(got == [b"0", b"1", b"2"], f"queued while offline: {got}")
    c.close()
    Client(V3, cid=cid, clean=True).close()  # drop the session


@scenario
def last_will():
    sub = Client(V3)
    sub.subscribe(f"{P}/will")
    dying = Client(V3, will=(f"{P}/will", b"gone"))
    dying.c.loop_stop()
    dying.c.socket().shutdown(socket.SHUT_RDWR)  # no DISCONNECT: the broker sends the will
    dying.c.socket().close()
    m = sub.recv(10)
    check(m.payload == b"gone", f"will {m.payload!r}")
    sub.close()


@scenario
def clean_disconnect_no_will():
    sub = Client(V3)
    sub.subscribe(f"{P}/nowill")
    c = Client(V3, will=(f"{P}/nowill", b"gone"))
    c.close()
    sub.quiet(1)
    sub.close()


@scenario
def unsubscribe():
    c = Client(V3)
    c.subscribe(f"{P}/u")
    c.c.unsubscribe(f"{P}/u")
    c.wait("unsuback")
    c.publish(f"{P}/u", b"x")
    c.quiet()
    c.close()


@scenario
def v5_properties():
    c = Client(V5)
    c.subscribe(f"{P}/v5p")
    props = Properties(PacketTypes.PUBLISH)
    props.UserProperty = [("k", "v"), ("k2", "v2")]
    props.ContentType = "text/plain"
    props.ResponseTopic = f"{P}/reply"
    props.CorrelationData = b"corr-1"
    props.PayloadFormatIndicator = 1
    c.publish(f"{P}/v5p", b"hi", props=props)
    m = c.recv()
    mp = m.properties
    check(("k", "v") in mp.UserProperty and ("k2", "v2") in mp.UserProperty, f"user properties {mp}")
    check(mp.ContentType == "text/plain" and mp.ResponseTopic == f"{P}/reply" and mp.CorrelationData == b"corr-1", f"properties {mp}")
    c.close()


@scenario
def v5_message_expiry():
    cid = f"{P}-expiry"
    c = Client(V5, cid=cid, clean=True, session_expiry=60)
    c.subscribe(f"{P}/exp")
    c.close()
    pub = Client(V5)
    props = Properties(PacketTypes.PUBLISH)
    props.MessageExpiryInterval = 1
    pub.publish(f"{P}/exp", b"short", props=props)
    pub.publish(f"{P}/exp", b"long")
    pub.close()
    time.sleep(2.2)
    c = Client(V5, cid=cid, clean=False, session_expiry=0)
    check(c.connack[1].session_present, "session not present")
    m = c.recv()
    check(m.payload == b"long", f"expired message delivered: {m.payload!r}")
    c.quiet()
    c.close()


@scenario
def v5_session_expiry():
    cid = f"{P}-sexp"
    c = Client(V5, cid=cid, clean=True, session_expiry=1)
    c.subscribe(f"{P}/sexp")
    c.close()
    time.sleep(2.5)
    c = Client(V5, cid=cid, clean=False, session_expiry=0)
    check(not c.connack[1].session_present, "session outlived its expiry interval")
    c.close()


@scenario
def v5_topic_alias():
    c = Client(V5)
    c.subscribe(f"{P}/alias")
    props = Properties(PacketTypes.PUBLISH)
    props.TopicAlias = 1
    c.publish(f"{P}/alias", b"first", props=props)
    props = Properties(PacketTypes.PUBLISH)
    props.TopicAlias = 1
    c.publish("", b"second", props=props)  # topic from the alias
    got = [c.recv(), c.recv()]
    check([(m.topic, m.payload) for m in got] == [(f"{P}/alias", b"first"), (f"{P}/alias", b"second")], f"got {[(m.topic, m.payload) for m in got]}")
    c.close()


@scenario
def v5_no_local():
    c = Client(V5)
    c.subscribe(f"{P}/nl", noLocal=True)
    c.publish(f"{P}/nl", b"mine")
    c.quiet()
    other = Client(V5)
    other.publish(f"{P}/nl", b"theirs")
    check(c.recv().payload == b"theirs", "message from another client")
    other.close()
    c.close()


@scenario
def v5_retain_as_published():
    pub = Client(V5)
    sub = Client(V5)
    sub.subscribe(f"{P}/rap", retainAsPublished=True)
    pub.publish(f"{P}/rap", b"r", retain=True)
    m = sub.recv()
    check(m.retain, "retain flag dropped despite retain-as-published")
    pub.publish(f"{P}/rap", b"", retain=True)
    sub.close()
    pub.close()


@scenario
def v5_subscription_identifier():
    c = Client(V5)
    c.subscribe(f"{P}/sid/#", sub_id=7)
    c.publish(f"{P}/sid/x", b"x")
    m = c.recv()
    check(m.properties.SubscriptionIdentifier == [7], f"subscription identifiers {m.properties}")
    c.close()


@scenario
def v5_no_matching_subscribers():
    c = Client(V5)
    rc = c.publish(f"{P}/nobody/{uuid.uuid4().hex}", b"x")
    check(rc is not None and rc_value(rc) == 0x10, f"puback reason {rc}")
    c.close()


@scenario
def amqp_interop():
    conn = pika.BlockingConnection(pika.URLParameters(AMQP_URL))
    ch = conn.channel()
    q = ch.queue_declare("", exclusive=True).method.queue
    ch.queue_bind(q, "amq.topic", routing_key=f"{P}.interop.*")
    c = Client(V3)
    c.subscribe(f"{P}/interop/fromamqp")
    c.publish(f"{P}/interop/frommqtt", b"to amqp")
    deadline = time.time() + 5
    m = None
    while time.time() < deadline and not m:
        m, _, body = ch.basic_get(q, auto_ack=True)
        if not m:
            time.sleep(0.05)
    check(m and m.routing_key == f"{P}.interop.frommqtt" and body == b"to amqp", f"AMQP got {m and m.routing_key} {m and body}")
    ch.basic_publish("amq.topic", f"{P}.interop.fromamqp", b"to mqtt")
    got = c.recv()
    check(got.topic == f"{P}/interop/fromamqp" and got.payload == b"to mqtt", f"MQTT got {got.topic} {got.payload!r}")
    c.close()
    conn.close()


@scenario
def throughput():
    n = 5000
    sub = Client(V3)
    sub.subscribe(f"{P}/tp", qos=1)
    pub = Client(V3)
    t0 = time.time()
    infos = [pub.c.publish(f"{P}/tp", b"x" * 64, qos=1) for _ in range(n)]
    for i in infos:
        i.wait_for_publish(30)
    pub_s = time.time() - t0
    for _ in range(n):
        sub.recv(30)
    total = time.time() - t0
    pub.close()
    sub.close()
    return f"publish {n / pub_s:.0f} msg/s, end to end {n / total:.0f} msg/s"


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
