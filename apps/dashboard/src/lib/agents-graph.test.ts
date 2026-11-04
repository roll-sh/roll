import { describe, expect, it } from "vitest";
import { deriveGraph, keyOf, projectStats, rolling } from "./agents-graph";
import { arrange, bounds, dropIndex, fitView, mergeOrder, moveTo, ROOTS, tidyLayout, withOrder, zoomAt } from "./agents-layout";
import { applyEvents, emptyData, follow } from "./agents-store";
import { approval, ev, machine, node, project, run, terminal, thread } from "@/test/agents";

function sample() {
  let d = follow(emptyData(), ["thr_1"]);
  d = applyEvents(d, [
    { type: "machine", cursor: 1, machine: machine() },
    { type: "machine", cursor: 2, machine: machine({ id: "mch_2", name: "build-box", online: false }) },
    ev("project", project(), 3),
    ev("project", project({ id: "prj_2", name: "web" }), 4, "mch_2"),
    ev("thread", thread({ status: "running", active_run: "run_1", created_at: 10 }), 5),
    ev("thread", thread({ id: "thr_2", title: "Fork", created_at: 20, lineage: { parent: "thr_1", relation: "fork", root: "thr_1", from_run: "run_1" } }), 6),
    ev("thread", thread({ id: "thr_3", title: "Docs", created_at: 30 }), 7),
    ev("thread", thread({ id: "thr_old", title: "Archived", archived_at: 5 }), 8),
    ev("run", run(), 9),
    ev("node", node({ type: "subagent", agent: "Explore", prompt: "find callers", child: null, result: null }, { id: "nod_s" }), 10),
    ev("node", node({ type: "subagent", agent: "Delegated", prompt: "x", child: "thr_2", result: null }, { id: "nod_d", ordinal: 2 }), 11),
    ev("request", approval(), 12),
    ev("terminal", terminal(), 13),
    ev("terminal", terminal({ id: "trm_login", project: null, title: "codex login" }), 14),
  ]);
  return d;
}

describe("agents graph", () => {
  it("hangs projects off machines, threads off projects, forks off their parent", () => {
    const g = deriveGraph(sample());
    expect(g.roots).toEqual(["m:mch_1", "m:mch_2"]); // by name: ada-mbp, build-box
    const kids = (k: string) => g.nodes.get(k)!.children;
    expect(kids("m:mch_1")).toEqual(["p:prj_1", "x:trm_login"]);
    expect(kids("p:prj_1")).toEqual(["t:thr_3", "t:thr_1", "x:trm_1"]);
    // The fork is a thread under thr_1; the delegated subagent node is drawn as that thread, not twice.
    expect(kids("t:thr_1")).toEqual(["t:thr_2", "s:nod_s", "r:req_1"]);
    expect(g.nodes.has("t:thr_old")).toBe(false);
    expect(g.edges.find((e) => e.to === "t:thr_2")).toMatchObject({ dashed: true, busy: false });
  });

  it("animates only what runs: not the machine and project above it, not what waits on you", () => {
    const g = deriveGraph(sample());
    expect(g.nodes.get("t:thr_1")!.busy).toBe(true);
    expect(g.nodes.get("p:prj_1")!.busy).toBe(false);
    expect(g.nodes.get("m:mch_1")!.busy).toBe(false);
    expect(g.nodes.get("r:req_1")!.busy).toBe(false);
    expect(g.edges.find((e) => e.to === "t:thr_1")!.busy).toBe(true);
    expect(g.edges.find((e) => e.to === "p:prj_1")!.busy).toBe(false);
    expect(g.edges.find((e) => e.to === "t:thr_3")!.busy).toBe(false);
    const waiting = applyEvents(sample(), [ev("thread", thread({ status: "waiting", active_run: "run_1", created_at: 10 }), 20)]);
    expect(deriveGraph(waiting).nodes.get("t:thr_1")!.busy).toBe(false);
  });

  it("moves an open thread's requests into the session and shows archived threads on request", () => {
    const g = deriveGraph(sample(), { expanded: new Set(["thr_1"]), showArchived: true });
    expect(g.nodes.has(keyOf.request("req_1"))).toBe(false);
    expect(g.nodes.has("t:thr_old")).toBe(true);
  });

  it("sums up a project like T3 does: its threads, the PRs they made, the agents in it", () => {
    const change = { repo: "acme/roll", number: 41, status: "ready" as const, url: "/acme/roll/changes/41", detail: null };
    const d = applyEvents(sample(), [
      ev("thread", thread({ id: "thr_3", title: "Docs", created_at: 30, provider: "codex", model: "gpt-5.5-codex", change, usage: { input_tokens: 1, output_tokens: 1, cached_tokens: 0, cost_usd: 0.5 } }), 20),
      ev("thread", thread({ id: "thr_2", title: "Fork", status: "waiting", created_at: 20, lineage: { parent: "thr_1", relation: "fork", root: "thr_1", from_run: "run_1" } }), 21),
    ]);
    const s = projectStats(d, "prj_1");
    expect(s).toMatchObject({ threads: 3, running: 1, waiting: 1, cost_usd: 0.5 });
    expect(s.changes.map((c) => c.number)).toEqual([41]);
    expect(s.agents.map((a) => [a.provider, a.threads])).toEqual([["claude", 2], ["codex", 1]]);
    // Archived threads don't count.
    expect(projectStats(d, "prj_1").threads).toBe(3);
  });

  it("lists what's rolling", () => {
    const r = rolling(sample());
    expect(r.busy.map((t) => t.id)).toEqual(["thr_1"]);
    expect(r.requests.map((q) => q.id)).toEqual(["req_1"]);
  });
});

describe("agents layout", () => {
  const tree = {
    roots: ["a", "b"],
    nodes: new Map([
      ["a", { children: ["a1", "a2"] }],
      ["a1", { children: [] }],
      ["a2", { children: ["a2x"] }],
      ["a2x", { children: [] }],
      ["b", { children: [] }],
    ]),
  };
  const size = (k: string) => (k === "a2" ? { w: 300, h: 200 } : { w: 100, h: 40 });

  it("lays out a tidy tree to the right, parents centred on their children", () => {
    const p = tidyLayout(tree, size, { gapX: 50, gapY: 10, rootGap: 30 });
    expect(p.get("a1")).toEqual({ x: 150, y: 0 });
    expect(p.get("a2")).toEqual({ x: 150, y: 50 });
    expect(p.get("a2x")).toEqual({ x: 500, y: 50 + 80 });
    // a's subtree is 40 + 10 + 200 = 250 tall; a sits in its middle.
    expect(p.get("a")).toEqual({ x: 0, y: 105 });
    expect(p.get("b")).toEqual({ x: 0, y: 280 });
    // No two siblings overlap.
    expect(p.get("a2")!.y).toBeGreaterThanOrEqual(p.get("a1")!.y + 40 + 10);
  });

  it("lines depths up in columns, so cousins of different widths never overlap", () => {
    const t = {
      roots: ["m"],
      nodes: new Map([
        ["m", { children: ["narrow", "wide"] }],
        ["narrow", { children: ["n1"] }],
        ["wide", { children: ["w1"] }],
        ["n1", { children: [] }],
        ["w1", { children: [] }],
      ]),
    };
    const sz = (k: string) => (k === "wide" ? { w: 400, h: 40 } : { w: 100, h: 40 });
    const p = tidyLayout(t, sz, { gapX: 50, gapY: 10 });
    expect(p.get("n1")!.x).toBe(p.get("w1")!.x);
    expect(p.get("w1")!.x).toBe(p.get("wide")!.x + 400 + 50);
    const keys = [...p.keys()];
    for (const a of keys)
      for (const b of keys) {
        if (a === b) continue;
        const [pa, pb, sa, sb] = [p.get(a)!, p.get(b)!, sz(a), sz(b)];
        const apart = pa.x + sa.w <= pb.x || pb.x + sb.w <= pa.x || pa.y + sa.h <= pb.y || pb.y + sb.h <= pa.y;
        expect(apart, `${a} and ${b} overlap`).toBe(true);
      }
  });

  it("keeps the user's order among siblings; new work keeps its default place", () => {
    // The user put c above a; b and the new n were never moved.
    expect(mergeOrder(["n", "a", "b", "c"], ["c", "b", "a"])).toEqual(["n", "c", "b", "a"]);
    expect(mergeOrder(["a", "b"], ["gone", "b", "a"])).toEqual(["b", "a"]);
    expect(mergeOrder(["a", "b"], undefined)).toEqual(["a", "b"]);
    const o = withOrder(withOrder({}, ROOTS, ["b", "a"]), "a", ["a2", "a1"]);
    const t = arrange(tree, o);
    expect(t.roots).toEqual(["b", "a"]);
    expect(t.nodes.get("a")!.children).toEqual(["a2", "a1"]);
    const p = tidyLayout(t, size, { gapX: 50, gapY: 10, rootGap: 30 });
    expect(p.get("b")!.y).toBeLessThan(p.get("a")!.y);
    expect(p.get("a2")!.y).toBeLessThan(p.get("a1")!.y);
  });

  it("drops a dragged card after the siblings it passed", () => {
    const centres = new Map([["a", 20], ["b", 100], ["c", 180]]);
    const at = (y: number) => moveTo(["a", "b", "c"], "a", dropIndex(["a", "b", "c"], "a", y, (k) => centres.get(k)!));
    expect(at(30)).toEqual(["a", "b", "c"]);
    expect(at(120)).toEqual(["b", "a", "c"]);
    expect(at(400)).toEqual(["b", "c", "a"]);
  });

  it("fits and zooms about a point", () => {
    const r = bounds(new Map([["a", { x: 0, y: 0 }], ["b", { x: 900, y: 500 }]]), () => ({ w: 100, h: 100 }))!;
    expect(r).toEqual({ x: 0, y: 0, w: 1000, h: 600 });
    const v = fitView(r, 1100, 700, 50);
    expect(v.k).toBe(1);
    expect(v).toEqual({ k: 1, x: 50, y: 50 });
    const z = zoomAt(v, 2, 550, 350);
    // The world point under the cursor stays put.
    expect((550 - z.x) / z.k).toBeCloseTo((550 - v.x) / v.k);
    expect((350 - z.y) / z.k).toBeCloseTo((350 - v.y) / v.k);
  });
});

describe("sandboxes and phones on the canvas", () => {
  it("a sandbox pulses while it boots, and not once it runs", () => {
    const sb = (status: "creating" | "running") => machine({ id: "mch_s", name: "sandbox", online: status === "running", info: null, sandbox: { backend: "docker", size: "standard", status, hourly_cents: 3 } });
    let d = applyEvents(emptyData(), [{ type: "machine", cursor: 1, machine: sb("creating") }]);
    expect(deriveGraph(d).nodes.get(keyOf.machine("mch_s"))?.busy).toBe(true);
    d = applyEvents(d, [{ type: "machine", cursor: 2, machine: sb("running") }]);
    expect(deriveGraph(d).nodes.get(keyOf.machine("mch_s"))?.busy).toBe(false);
  });

  it("a phone's fit stays readable and starts at the machines (top left)", () => {
    const wide = { x: 0, y: 0, w: 3000, h: 400 };
    const desk = fitView(wide, 1200, 800, 64, 1);
    expect(desk.k).toBeCloseTo((1200 - 128) / 3000);
    expect(desk.x + (wide.w / 2) * desk.k).toBeCloseTo(600);
    const phone = fitView(wide, 390, 700, 24, 1, 0.55);
    expect(phone.k).toBe(0.55);
    expect(phone.x).toBe(24);
    // What fits vertically stays centred.
    expect(phone.y + (wide.h / 2) * phone.k).toBeCloseTo(350);
  });
});
