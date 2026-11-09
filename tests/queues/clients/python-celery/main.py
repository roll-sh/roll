"""roll queues compatibility scenarios for Celery (py-amqp broker transport, rpc:// result backend)."""

import os
import signal
import subprocess
import sys
import threading
import time
import traceback
import uuid

P = f"celery-{uuid.uuid4().hex[:6]}"
os.environ["CELERY_QTEST_QUEUE"] = f"{P}.tasks"

from celery import chain as celery_chain, group  # noqa: E402
from celery.result import ResultSet  # noqa: E402

from tasks import QUEUE, add, app, fail, flaky, late, mul  # noqa: E402

CONCURRENCY = 4  # with worker_prefetch_multiplier=1 the initial prefetch is 4
SCENARIOS = []


def scenario(fn):
    SCENARIOS.append(fn)
    return fn


def check(cond, msg):
    if not cond:
        raise AssertionError(msg)


@scenario
def connect_():
    with app.connection_for_write() as conn:
        conn.ensure_connection(max_retries=3)
        props = conn.connection.server_properties
        check(conn.connected, "not connected")
        return f"{conn.transport_cls} {props['product']} {props['version']}"


@scenario
def task_roundtrip():
    r = add.delay(2, 3)
    got = r.get(timeout=10)
    check(got == 5 and r.state == "SUCCESS", f"got {got} state {r.state}")


@scenario
def many_tasks():
    res = group(add.s(i, i) for i in range(100)).apply_async()
    got = res.get(timeout=15)
    check(got == [2 * i for i in range(100)], f"results {got}")
    return "100 tasks"


@scenario
def chain():
    got = celery_chain(add.s(1, 2), add.s(3), mul.s(10)).apply_async().get(timeout=10)
    check(got == 60, f"got {got}")


@scenario
def retry():
    got = flaky.delay().get(timeout=10)
    check(got == 2, f"succeeded after {got} retries")
    return "2 retries"


@scenario
def task_error():
    r = fail.delay("boom")
    try:
        r.get(timeout=10)
    except ValueError as e:
        check(str(e) == "boom", f"exception {e!r}")
        check(r.state == "FAILURE", f"state {r.state}")
        return
    raise AssertionError("exception did not propagate")


@scenario
def acks_late():
    got = late.delay(21).get(timeout=10)
    check(got == 42, f"got {got}")


@scenario
def countdown():
    # More ETA tasks than the initial prefetch: the worker holds them unacked and raises its
    # prefetch (basic.qos, global on RabbitMQ) so that it keeps receiving messages.
    n = CONCURRENCY * 2 + 2
    t0 = time.time()
    held = [add.apply_async((i, 1), countdown=2) for i in range(n)]
    quick = add.delay(100, 1)
    check(quick.get(timeout=10) == 101, "immediate task result")
    quick_s = time.time() - t0
    got = ResultSet(held).get(timeout=10)  # native join; per-result get() polls in 1 s steps
    eta_s = time.time() - t0
    check(got == [i + 1 for i in range(n)], f"results {got}")
    check(eta_s >= 1.9, f"countdown tasks ran early ({eta_s:.1f}s)")
    # Not asserted: RabbitMQ 4.3 dropped global QoS, so there the immediate task waits ~2 s.
    return f"{n} eta tasks after {eta_s:.2f}s, immediate task after {quick_s:.2f}s"


def start_worker():
    # Prefork (Celery's default). Not threads: there, acks are queued with the hub's call_soon
    # from pool threads, which doesn't wake its poll, so an ack can wait up to 10 s for the next
    # socket event and stall the worker at its prefetch limit (happens on RabbitMQ too).
    cmd = [
        sys.executable, "-m", "celery", "-A", "tasks", "worker", "-P", "prefork", "-c", str(CONCURRENCY),
        "-Q", QUEUE, "-n", f"{P}@%h", "-l", "info", "--without-gossip", "--without-mingle", "--without-heartbeat",
    ]
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    ready = threading.Event()

    def pump():
        for line in proc.stdout:
            sys.stderr.write(f"[worker] {line}")
            if " ready." in line:
                ready.set()

    threading.Thread(target=pump, daemon=True).start()
    if not ready.wait(30):
        raise RuntimeError("worker did not become ready within 30s")
    return proc


def stop_worker(proc):
    if proc is None or proc.poll() is not None:
        return
    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


def cleanup():
    try:
        with app.connection_for_write() as conn:
            conn.default_channel.queue_delete(QUEUE)
    except Exception:  # noqa: BLE001
        traceback.print_exc(file=sys.stderr)


def main():
    only = set(sys.argv[1:])
    failed = 0
    worker = None
    try:
        worker_err = None
        if not only or only - {"connect"}:
            try:
                worker = start_worker()
            except Exception as e:  # noqa: BLE001
                worker_err = e
        for fn in SCENARIOS:
            name = fn.__name__.rstrip("_")
            if only and name not in only:
                continue
            t = time.time()
            try:
                if worker_err and name != "connect":
                    raise worker_err
                detail = fn() or ""
                print(f"PASS {name} {detail} ({time.time() - t:.1f}s)".replace("  ", " "), flush=True)
            except Exception as e:  # noqa: BLE001
                failed += 1
                print(f"FAIL {name}: {type(e).__name__}: {e}", flush=True)
                traceback.print_exc(file=sys.stderr)
    finally:
        stop_worker(worker)
        cleanup()
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
