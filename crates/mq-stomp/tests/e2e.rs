//! End-to-end test with the unmodified Python `stomp.py` client in Docker. Runs only with
//! `ROLL_E2E=1`; the container reaches the in-process broker via `host.docker.internal`.

mod common;

use common::Harness;
use std::process::Command;

const SCRIPT: &str = r#"
import os, sys, time, threading
import stomp

host, port = os.environ["STOMP_HOST"], int(os.environ["STOMP_PORT"])

class Listener(stomp.ConnectionListener):
    def __init__(self):
        self.messages, self.errors, self.cond = [], [], threading.Condition()
    def on_message(self, frame):
        with self.cond:
            self.messages.append(frame)
            self.cond.notify_all()
    def on_error(self, frame):
        self.errors.append(frame)
    def wait(self, n, timeout=10):
        with self.cond:
            self.cond.wait_for(lambda: len(self.messages) >= n, timeout)
        assert len(self.messages) >= n, f"got {len(self.messages)} of {n}: {self.errors}"

def connect(version):
    cls = {"1.0": stomp.Connection10, "1.1": stomp.Connection11, "1.2": stomp.Connection12}[version]
    # stomp.py sends its target hostname as `host` (the vhost) unless told otherwise.
    kw = {} if version == "1.0" else {"heartbeats": (1000, 1000), "vhost": "/"}
    c = cls([(host, port)], **kw)
    l = Listener()
    c.set_listener("", l)
    c.connect("guest", "guest", wait=True)
    return c, l

# Subscribe, send, client-individual ack, for each protocol version.
for v in ["1.0", "1.1", "1.2"]:
    c, l = connect(v)
    q = f"/queue/e2e-{v}"
    ack = "client" if v == "1.0" else "client-individual"
    c.subscribe(q, id="s1", ack=ack)
    for i in range(3):
        c.send(q, f"m{i}", headers={"persistent": "true", "x-n": str(i)}, receipt=f"r{i}" if v != "1.0" else None)
    l.wait(3)
    bodies = [m.body for m in l.messages]
    assert bodies == ["m0", "m1", "m2"], bodies
    assert l.messages[1].headers["x-n"] == "1"
    for m in l.messages:
        if v == "1.2":
            c.ack(m.headers["ack"])
        elif v == "1.1":
            c.ack(m.headers["message-id"], "s1")
        else:
            c.ack(m.headers["message-id"])
    c.disconnect(receipt="bye")
    print(f"version {v}: ok", flush=True)

# NACK requeues.
c, l = connect("1.2")
c.subscribe("/queue/e2e-nack", id="n", ack="client-individual")
c.send("/queue/e2e-nack", "again")
l.wait(1)
c.nack(l.messages[0].headers["ack"])
l.wait(2)
assert l.messages[1].body == "again" and l.messages[1].headers["redelivered"] == "true"
c.ack(l.messages[1].headers["ack"])
print("nack: ok", flush=True)

# Transactions: an aborted send never arrives, a committed one does, acks in a transaction.
c.subscribe("/topic/e2e.tx", id="t", ack="client-individual")
time.sleep(0.2)
tx = c.begin()
c.send("/topic/e2e.tx", "aborted", transaction=tx)
c.abort(tx)
tx = c.begin()
c.send("/topic/e2e.tx", "committed", transaction=tx)
time.sleep(0.3)
assert len(l.messages) == 2, [m.body for m in l.messages]
c.commit(tx)
l.wait(3)
assert l.messages[2].body == "committed"
tx = c.begin()
c.ack(l.messages[2].headers["ack"], transaction=tx)
c.commit(tx)
time.sleep(0.3)
assert len(l.messages) == 3 and not l.errors, l.errors
print("transactions: ok", flush=True)

# Request/reply through a temp queue.
c.subscribe("/queue/e2e-rpc", id="rpc")
c.send("/queue/e2e-rpc", "ping", headers={"reply-to": "/temp-queue/answers"})
l.wait(4)
req = l.messages[3]
assert req.headers["reply-to"].startswith("/reply-queue/amq.gen-"), req.headers
c.send(req.headers["reply-to"], "pong")
l.wait(5)
assert l.messages[4].body == "pong"
c.disconnect()
print("ALL OK", flush=True)
"#;

#[tokio::test(flavor = "multi_thread")]
async fn stomp_py_in_docker() {
    if std::env::var("ROLL_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set ROLL_E2E=1 to run the Docker e2e test");
        return;
    }
    let h = Harness::with(mq_stomp::Config { listen: "0.0.0.0:0".parse().unwrap(), ..Default::default() }, true).await;
    let port = h.addr.port();
    let out = tokio::task::spawn_blocking(move || {
        Command::new("docker")
            .args(["run", "--rm", "--add-host=host.docker.internal:host-gateway"])
            .args(["-e", "STOMP_HOST=host.docker.internal", "-e", &format!("STOMP_PORT={port}"), "-e", &format!("SCRIPT={SCRIPT}")])
            .args(["python:3.13-alpine", "sh", "-c", "pip install -q --disable-pip-version-check --root-user-action=ignore stomp.py && python -c \"$SCRIPT\""])
            .output()
            .expect("docker")
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    println!("{stdout}\n{stderr}");
    assert!(out.status.success() && stdout.contains("ALL OK"), "stomp.py failed:\n{stdout}\n{stderr}");
}
