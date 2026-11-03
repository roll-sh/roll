#!/usr/bin/env python3
"""Counting TCP proxy for postgres_fdw traffic on the reference.

usage: fdwproxy.py <listen port> <target port> <counters file>
Counts client->server reads ("writes" of the wrapper: one per message batch it sends),
bytes each way, and connections; rewrites the counters file after every event.
"""
import asyncio, sys

LISTEN, TARGET, OUT = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
HOST = ".".join(["127", "0", "0", "1"])
c = {"writes": 0, "out": 0, "in": 0, "conns": 0}


def dump():
    with open(OUT, "w") as f:
        f.write("writes %(writes)d out %(out)d in %(in)d conns %(conns)d\n" % c)


async def pipe(r, w, up):
    try:
        while True:
            data = await r.read(65536)
            if not data:
                break
            if up:
                c["writes"] += 1
                c["out"] += len(data)
            else:
                c["in"] += len(data)
            dump()
            w.write(data)
            await w.drain()
    except Exception:
        pass
    finally:
        try:
            w.close()
        except Exception:
            pass


async def handle(cr, cw):
    c["conns"] += 1
    sr, sw = await asyncio.open_connection(HOST, TARGET)
    await asyncio.gather(pipe(cr, sw, True), pipe(sr, cw, False))


async def main():
    dump()
    srv = await asyncio.start_server(handle, HOST, LISTEN)
    async with srv:
        await srv.serve_forever()


asyncio.run(main())
