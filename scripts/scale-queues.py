"""Scale driver for scripts/scale-queues.sh: declares many queues and holds many idle AMQP
connections against a broker, speaking just enough AMQP 0-9-1 by hand that the driver itself
stays small (a client library would cost more memory and time than the broker under test).

    python3 scale-queues.py queues HOST USER PASS COUNT
    python3 scale-queues.py connections HOST USER PASS COUNT HOLD_FILE

`queues` declares COUNT durable classic queues (`scale-0` ...) on vhost `/`, pipelined with
no-wait, and returns once the broker has confirmed the last one. `connections` opens COUNT
connections with one channel each, writes "ready" to HOLD_FILE, and holds them until
HOLD_FILE is removed.
"""

import asyncio
import os
import struct
import sys

FRAME_END = b"\xce"


def shortstr(s: str) -> bytes:
    b = s.encode()
    return bytes([len(b)]) + b


def longstr(b: bytes) -> bytes:
    return struct.pack(">I", len(b)) + b


def method(channel: int, class_id: int, method_id: int, args: bytes = b"") -> bytes:
    payload = struct.pack(">HH", class_id, method_id) + args
    return struct.pack(">BHI", 1, channel, len(payload)) + payload + FRAME_END


async def read_method(r: asyncio.StreamReader) -> tuple[int, int, int]:
    """The next method frame as (channel, class, method); skips heartbeats."""
    while True:
        kind, channel, size = struct.unpack(">BHI", await r.readexactly(7))
        payload = await r.readexactly(size + 1)
        if kind == 1:
            class_id, method_id = struct.unpack(">HH", payload[:4])
            return channel, class_id, method_id


async def expect(r: asyncio.StreamReader, class_id: int, method_id: int) -> None:
    got = await read_method(r)
    if got[1:] != (class_id, method_id):
        raise RuntimeError(f"expected {class_id}.{method_id}, got {got[1]}.{got[2]}")


async def connect(host: str, user: str, password: str):
    r, w = await asyncio.open_connection(host, 5672)
    w.write(b"AMQP\x00\x00\x09\x01")
    await expect(r, 10, 10)  # connection.start
    auth = b"\x00" + user.encode() + b"\x00" + password.encode()
    w.write(method(0, 10, 11, struct.pack(">I", 0) + shortstr("PLAIN") + longstr(auth) + shortstr("en_US")))
    await expect(r, 10, 30)  # connection.tune
    w.write(method(0, 10, 31, struct.pack(">HIH", 2047, 131072, 0)))  # no heartbeats
    w.write(method(0, 10, 40, shortstr("/") + shortstr("") + b"\x00"))
    await expect(r, 10, 41)  # connection.open-ok
    w.write(method(1, 20, 10, shortstr("")))
    await expect(r, 20, 11)  # channel.open-ok
    return r, w


def declare(name: str, no_wait: bool) -> bytes:
    bits = 0x02 | (0x10 if no_wait else 0)  # durable [| no-wait]
    return method(1, 50, 10, struct.pack(">H", 0) + shortstr(name) + bytes([bits]) + struct.pack(">I", 0))


async def queues(host: str, user: str, password: str, count: int) -> None:
    r, w = await connect(host, user, password)
    batch = 1000
    for start in range(0, count, batch):
        end = min(start + batch, count)
        for i in range(start, end):
            w.write(declare(f"scale-{i}", no_wait=i != end - 1))
        await w.drain()
        # The last declare of each batch waits for its reply; the channel is ordered, so all
        # earlier ones are done too (a failure would have closed the channel instead).
        await expect(r, 50, 11)
    w.close()


async def connections(host: str, user: str, password: str, count: int, hold: str) -> None:
    sem = asyncio.Semaphore(200)
    held = []

    async def one():
        async with sem:
            for attempt in range(5):
                try:
                    held.append(await connect(host, user, password))
                    return
                except (OSError, asyncio.IncompleteReadError):
                    await asyncio.sleep(0.2 * (attempt + 1))
            raise RuntimeError("could not connect")

    await asyncio.gather(*(one() for _ in range(count)))
    with open(hold, "w") as f:
        f.write(f"ready {len(held)}\n")
    while os.path.exists(hold):
        await asyncio.sleep(0.5)
    for _, w in held:
        w.close()


def main() -> None:
    mode, host, user, password, count = sys.argv[1:6]
    if mode == "queues":
        asyncio.run(queues(host, user, password, int(count)))
    else:
        asyncio.run(connections(host, user, password, int(count), sys.argv[6]))


if __name__ == "__main__":
    main()
