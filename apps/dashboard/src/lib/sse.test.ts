import { describe, expect, it, vi } from "vitest";
import { parseSse, streamSse } from "./sse";

describe("parseSse", () => {
  it("splits complete events and keeps the partial rest", () => {
    const { events, rest } = parseSse('event: landed\ndata: {"a":1}\n\nevent: work\ndata: {"b"');
    expect(events).toEqual([{ event: "landed", data: '{"a":1}' }]);
    expect(rest).toBe('event: work\ndata: {"b"');
  });

  it("handles comments, CRLF, multi-line data, ids and the default event", () => {
    const { events, rest } = parseSse(": keepalive\r\n\r\nid: 7\r\ndata: one\r\ndata: two\r\n\r\ndata:no-space\n\n");
    expect(events).toEqual([
      { event: "message", data: "one\ntwo", id: "7" },
      { event: "message", data: "no-space" },
    ]);
    expect(rest).toBe("");
  });

  it("ignores blocks without data", () => {
    expect(parseSse("event: ping\n\n").events).toEqual([]);
  });
});

describe("streamSse", () => {
  it("reconnects after a drop and reports open/closed", async () => {
    const enc = new TextEncoder();
    let n = 0;
    const fetchFn = vi.fn(async () => {
      n++;
      if (n === 1) return new Response(null, { status: 503 });
      return new Response(
        new ReadableStream({
          start(c) {
            c.enqueue(enc.encode(`event: landed\ndata: ${n}\n\n`));
            if (n === 2) c.close();
          },
        }),
      );
    }) as unknown as typeof fetch;
    const events: string[] = [];
    const opens: boolean[] = [];
    const stop = streamSse("http://x/events", { fetch: fetchFn, backoff: [1], onEvent: (e) => events.push(e.data), onOpen: (o) => opens.push(o) });
    await vi.waitFor(() => expect(events).toEqual(["2", "3"]));
    stop();
    expect(opens.slice(0, 4)).toEqual([false, true, false, true]);
  });
});
