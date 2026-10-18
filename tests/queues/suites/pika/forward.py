"""TCP forwarder: 127.0.0.1:5672 -> <host>:<port>.

pika's acceptance tests hard-code localhost:5672 (tests/base/async_test_base.py,
tests/acceptance/blocking_adapter_test.py), so inside the test container we
listen there and relay to the broker. Half-closes and resets are propagated so
connection-loss tests see the same socket behaviour as a direct connection.
"""
import asyncio
import socket
import sys


async def pipe(reader, writer):
    try:
        while True:
            data = await reader.read(65536)
            if not data:
                break
            writer.write(data)
            await writer.drain()
        if writer.can_write_eof():
            writer.write_eof()
    except (ConnectionError, OSError):
        writer.transport.abort()


async def handle(c_reader, c_writer, host, port):
    try:
        b_reader, b_writer = await asyncio.open_connection(host, port)
    except OSError:
        c_writer.transport.abort()
        return
    for w in (c_writer, b_writer):
        w.get_extra_info('socket').setsockopt(socket.IPPROTO_TCP,
                                              socket.TCP_NODELAY, 1)
    await asyncio.gather(pipe(c_reader, b_writer), pipe(b_reader, c_writer))
    c_writer.close()
    b_writer.close()


async def main(host, port):
    server = await asyncio.start_server(
        lambda r, w: handle(r, w, host, port), '127.0.0.1', 5672,
        reuse_address=True)
    print('forwarding 127.0.0.1:5672 -> %s:%d' % (host, port), flush=True)
    async with server:
        await server.serve_forever()


if __name__ == '__main__':
    asyncio.run(main(sys.argv[1], int(sys.argv[2])))
