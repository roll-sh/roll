import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { State, StreamEvent } from "./agents-api";
import { AgentsClient, type AgentsTransport } from "./agents-client";
import { ev, machine, mirrored, node, project, thread } from "@/test/agents";

function fakeTransport() {
  const streams: { threads: readonly string[]; after: () => number; on: (ev: StreamEvent, id: number | null) => void; stopped: boolean }[] = [];
  const states: (readonly string[])[] = [];
  let cursor = 10;
  const t: AgentsTransport = {
    state: async (threads) => {
      states.push(threads);
      const s: State = {
        cursor,
        machines: [machine()],
        entities: [
          mirrored("project", project(), 1),
          mirrored("thread", thread(), 2),
          ...(threads.includes("thr_1") ? [mirrored("node", node({ type: "message", text: "hi" }), 3)] : []),
        ],
      };
      return s;
    },
    stream: (threads, after, on, onOpen) => {
      const s = { threads, after, on, stopped: false };
      streams.push(s);
      onOpen(true);
      return () => (s.stopped = true);
    },
    command: vi.fn(async () => ({ type: "ok" as const })),
  };
  return { t, streams, states, setCursor: (c: number) => (cursor = c) };
}

describe("agents client", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("loads, streams with resume, and follows thread detail", async () => {
    const f = fakeTransport();
    const c = new AgentsClient("acme", f.t, (fn) => fn());
    const release = c.hold();
    await vi.advanceTimersByTimeAsync(0);
    expect(c.getSnapshot().conn).toBe("live");
    expect(c.data.threads.size).toBe(1);
    expect(f.streams).toHaveLength(1);
    expect(f.streams[0].after()).toBe(10);

    f.streams[0].on(ev("thread", thread({ status: "running" }), 11), 11);
    expect(c.data.threads.get("thr_1")?.value.status).toBe("running");
    expect(f.streams[0].after()).toBe(11);

    // Following a thread fetches its detail and resumes the stream from that snapshot.
    f.setCursor(12);
    c.follow("canvas", ["thr_1"]);
    await vi.advanceTimersByTimeAsync(200);
    expect(f.states.at(-1)).toEqual(["thr_1"]);
    expect(c.data.nodes.get("thr_1")?.size).toBe(1);
    expect(f.streams).toHaveLength(2);
    expect(f.streams[0].stopped).toBe(true);
    expect(f.streams[1].threads).toEqual(["thr_1"]);
    expect(f.streams[1].after()).toBe(12);

    // Unfollowing needs no snapshot.
    c.unfollow("canvas");
    await vi.advanceTimersByTimeAsync(200);
    expect(f.states).toHaveLength(2);
    expect(c.data.nodes.size).toBe(0);
    expect(f.streams[2].threads).toEqual([]);

    release();
    expect(f.streams[2].stopped).toBe(true);
  });

  it("keeps terminal output for a late viewer", () => {
    const c = new AgentsClient("acme", fakeTransport().t, (fn) => fn());
    c.enqueue({ type: "delta", machine: "mch_1", delta: { type: "terminal", terminal: "trm_1", data: "aGk=" } });
    const seen: string[] = [];
    const { backlog, off } = c.onTerminal("trm_1", (b) => seen.push(b));
    expect(backlog).toEqual(["aGk="]);
    c.enqueue({ type: "delta", machine: "mch_1", delta: { type: "terminal", terminal: "trm_1", data: "Cg==" } });
    expect(seen).toEqual(["Cg=="]);
    off();
  });
});
