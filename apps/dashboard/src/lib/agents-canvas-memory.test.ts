import { describe, expect, it, vi } from "vitest";
import { CanvasConflict, createAgentsApi, MAX_CANVAS, streamEventD, type StreamEvent } from "./agents-api";
import { httpCanvas, memoryCanvas } from "./agents-services";
import { applyEvents, emptyData } from "./agents-store";
import { ApiError } from "./code-api";
import { decode } from "./decode";

/** A relay that keeps one canvas per owner the way `agents/canvas.rs` does, and records requests. */
function relay() {
  const kept = new Map<string, { canvas: Record<string, unknown>; revision: number; updated_at: number }>();
  const calls: { method: string; url: string; body: unknown; auth: string | null }[] = [];
  const view = (owner: string) => kept.get(owner) ?? { canvas: null, revision: 0, updated_at: null };
  const doFetch = vi.fn(async (url: string, init: RequestInit) => {
    const method = init.method ?? "GET";
    const body = init.body ? JSON.parse(String(init.body)) : undefined;
    calls.push({ method, url, body, auth: new Headers(init.headers).get("authorization") });
    const owner = decodeURIComponent(url.match(/\/agents\/([^/]+)\/canvas$/)![1]);
    const answer = (status: number, b: unknown) => new Response(JSON.stringify(b), { status });
    if (method === "GET") return answer(200, view(owner));
    const current = view(owner).revision;
    if (body.revision !== undefined && body.revision !== current) return answer(409, { ...view(owner), error: `the canvas changed (it is at revision ${current})` });
    kept.set(owner, { canvas: body.canvas, revision: current + 1, updated_at: 1_762_000_000_000 + current });
    return answer(200, view(owner));
  }) as unknown as typeof fetch;
  return { calls, api: createAgentsApi("http://roll", "rlc_t", undefined, doFetch) };
}

describe("the canvas over the relay", () => {
  it("loads, saves over a revision and says when another device saved first", async () => {
    const { calls, api } = relay();
    expect(await api.canvas("acme")).toEqual({ canvas: null, revision: 0, updated_at: null });
    expect(calls[0]).toMatchObject({ method: "GET", url: "http://roll/v1/code/agents/acme/canvas", auth: "Bearer rlc_t" });

    const doc = { version: 1, order: { mch_1: ["prj_b", "prj_a"] } };
    expect(await api.saveCanvas("acme", doc, 0)).toEqual({ canvas: doc, revision: 1, updated_at: 1_762_000_000_000 });
    expect(calls[1]).toMatchObject({ method: "PUT", url: "http://roll/v1/code/agents/acme/canvas", body: { canvas: doc, revision: 0 } });

    // A stale revision: a CanvasConflict holding what is there now.
    const err = await api.saveCanvas("acme", { version: 1 }, 0).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(CanvasConflict);
    expect((err as CanvasConflict).current).toEqual({ canvas: doc, revision: 1, updated_at: 1_762_000_000_000 });

    // Without a revision the last writer wins (and none is sent).
    expect((await api.saveCanvas("acme", { v: 2 })).revision).toBe(2);
    expect(calls.at(-1)!.body).toEqual({ canvas: { v: 2 } });
  });

  it("other errors stay ApiErrors", async () => {
    const doFetch = vi.fn(async () => new Response(JSON.stringify({ error: "agent tokens can't arrange acme's canvas" }), { status: 403 })) as unknown as typeof fetch;
    const api = createAgentsApi("http://roll", "rlc_agent", undefined, doFetch);
    const err = await api.saveCanvas("acme", {}).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(err).toMatchObject({ status: 403, message: "agent tokens can't arrange acme's canvas" });
  });

  it("services: null before the first save, the doc and its revision after, objects only", async () => {
    const { calls, api } = relay();
    const canvas = httpCanvas(api);
    expect(await canvas.load("acme")).toBeNull();
    expect(await canvas.save("acme", { view: { zoom: 1 } }, 0)).toEqual({ revision: 1 });
    expect(await canvas.load("acme")).toEqual({ doc: { view: { zoom: 1 } }, revision: 1 });
    await expect(canvas.save("acme", { view: {} }, 0)).rejects.toBeInstanceOf(CanvasConflict);
    const before = calls.length;
    for (const bad of [null, [1], "x", 3]) await expect(canvas.save("acme", bad)).rejects.toBeInstanceOf(TypeError);
    expect(calls.length).toBe(before);
    // Each owner has its own.
    expect(await canvas.load("other")).toBeNull();
  });
});

describe("the canvas in memory (demo, tests)", () => {
  it("keeps revisions and conflicts like the relay, and a copy rather than the caller's object", async () => {
    const canvas = memoryCanvas();
    expect(await canvas.load("demo")).toBeNull();
    const doc = { order: ["a", "b"] };
    expect(await canvas.save("demo", doc, 0)).toEqual({ revision: 1 });
    doc.order.push("c");
    expect(await canvas.load("demo")).toEqual({ doc: { order: ["a", "b"] }, revision: 1 });
    const err = await canvas.save("demo", { order: [] }, 0).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(CanvasConflict);
    expect((err as CanvasConflict).current).toMatchObject({ canvas: { order: ["a", "b"] }, revision: 1 });
    expect(await canvas.save("demo", { order: ["z"] })).toEqual({ revision: 2 });
    await expect(canvas.save("demo", { f: "x".repeat(MAX_CANVAS) })).rejects.toMatchObject({ status: 413 });
    await expect(canvas.save("demo", [1])).rejects.toBeInstanceOf(TypeError);
    expect((await canvas.load("demo"))?.revision).toBe(2);
    expect(await memoryCanvas().load("demo")).toBeNull();
  });
});

describe("canvas saves on the event stream", () => {
  it("decodes the canvas event and keeps the newest revision announced", () => {
    const ev = decode(streamEventD, { type: "canvas", cursor: 12, revision: 4, updated_at: 1_762_000_000_000 }, { strict: true });
    expect(ev).toEqual({ type: "canvas", cursor: 12, revision: 4, updated_at: 1_762_000_000_000 });
    let d = emptyData();
    expect(d.canvas).toBeNull();
    d = applyEvents(d, [ev]);
    expect(d.canvas).toEqual({ revision: 4, updated_at: 1_762_000_000_000 });
    expect(d.cursor).toBe(12);
    // A replayed or late announcement never takes it back.
    const older: StreamEvent = { type: "canvas", cursor: 13, revision: 3, updated_at: 1 };
    d = applyEvents(d, [older]);
    expect(d.canvas).toEqual({ revision: 4, updated_at: 1_762_000_000_000 });
    expect(d.cursor).toBe(13);
  });
});
